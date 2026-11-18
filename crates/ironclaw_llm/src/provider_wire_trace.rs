//! Explicit trace opt-in at the final provider boundary. No headers, URLs,
//! authentication values or arbitrary request metadata enter this module.
use std::{collections::HashMap, sync::OnceLock};

use ironclaw_common::{hashing::sha256_hex, run_trace};
use ironclaw_safety::LeakDetector;
use serde_json::{Value, json};

#[cfg(test)]
#[path = "provider_wire_trace_tests.rs"]
mod tests;

const MAX_OBSERVED_BODY_BYTES: usize = 2 * 1024 * 1024;
const ANTHROPIC_FIELDS: &[&str] = &[
    "model",
    "system",
    "messages",
    "max_tokens",
    "temperature",
    "thinking",
    "stream",
    "tools",
    "tool_choice",
    "top_p",
    "top_k",
    "stop_sequences",
    "output_config",
];

pub(crate) struct ProviderWireTrace {
    run_id: String,
    turn_id: Option<String>,
    exchange_id: String,
}

impl ProviderWireTrace {
    pub(crate) fn from_metadata(metadata: &HashMap<String, String>) -> Option<Self> {
        std::env::var_os("TIANQUAN_TRACE_DIR")?;
        let run_id = uuid::Uuid::parse_str(metadata.get("run_id")?)
            .ok()?
            .to_string();
        let exchange_id = uuid::Uuid::parse_str(metadata.get("ironclaw_trace_exchange_id")?)
            .ok()?
            .to_string();
        let turn_id = metadata
            .get("turn_id")
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .map(|id| id.to_string());
        Some(Self {
            run_id,
            turn_id,
            exchange_id,
        })
    }

    /// `wire` is the same serialized byte buffer passed to RequestBuilder::body.
    /// The CAS hash describes canonical allowlisted JSON; wire_sha256 separately
    /// describes actual sent bytes. Secret suspicion withholds the whole payload
    /// instead of silently changing fields and calling it an exact request.
    pub(crate) fn observe_anthropic(&self, wire: &[u8]) -> String {
        let physical_request_id = uuid::Uuid::new_v4().to_string();
        let mut event = json!({
            "schema_version": "provider-wire-request/1",
            "protocol": "anthropic_messages_bearer",
            "phase": "before_send",
            "exchange_id": self.exchange_id,
            "physical_request_id": physical_request_id,
            "wire_sha256": sha256_hex(wire),
            "wire_bytes": wire.len(),
            "payload_status": "unavailable",
        });
        if wire.len() > MAX_OBSERVED_BODY_BYTES {
            event["payload_status"] = json!("withheld_size_limit");
        } else if let Ok(Value::Object(object)) = serde_json::from_slice::<Value>(wire) {
            let selected = Value::Object(
                object
                    .into_iter()
                    .filter(|(key, _)| ANTHROPIC_FIELDS.contains(&key.as_str()))
                    .collect(),
            );
            if let Ok(canonical) = serde_json::to_string(&selected) {
                event["body_sha256"] = json!(sha256_hex(canonical.as_bytes()));
                event["body_bytes"] = json!(canonical.len());
                static SCANNER: OnceLock<LeakDetector> = OnceLock::new();
                let clean = SCANNER
                    .get_or_init(LeakDetector::new)
                    .scan_and_clean(&canonical);
                if !matches!(clean, Ok(ref safe) if safe == &canonical) {
                    event["payload_status"] = json!("withheld_sensitive_content");
                } else if let Some(reference) = run_trace::store_message_content(&selected) {
                    event["body_ref"] = json!(reference);
                    event["payload_status"] = json!("stored");
                } else {
                    event["payload_status"] = json!("storage_unavailable");
                }
            }
        }
        run_trace::append_trace_event(
            "provider_wire_request",
            &self.run_id,
            None,
            self.turn_id.as_deref(),
            event,
        );
        physical_request_id
    }

    pub(crate) fn observe_response(&self, physical_request_id: &str, status: u16) {
        run_trace::append_trace_event(
            "provider_wire_response",
            &self.run_id,
            None,
            self.turn_id.as_deref(),
            json!({
                "schema_version":"provider-wire-response/1", "exchange_id":self.exchange_id,
                "physical_request_id":physical_request_id, "phase":"response_headers_received", "http_status":status,
            }),
        );
    }
}
