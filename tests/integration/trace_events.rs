//! Caller-path contract for Reborn model/tool trace events.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use reborn_support::{group::RebornIntegrationGroup, reply::RebornScriptedReply};
use serde_json::{Value, json};

static TRACE_ROOT_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TraceRootGuard(Option<std::ffi::OsString>);

impl TraceRootGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("TIANQUAN_TRACE_DIR");
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", path) };
        Self(previous)
    }
}

impl Drop for TraceRootGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", previous) };
        } else {
            unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        }
    }
}

fn read_run_events(root: &std::path::Path, run_id: &str) -> Vec<Value> {
    let archive = root.join(format!("{run_id}.jsonl.gz"));
    let hot = root.join(format!("{run_id}.jsonl"));
    let mut text = String::new();
    if archive.exists() {
        let bytes = std::fs::read(archive).expect("read completed run archive");
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        std::io::Read::read_to_string(&mut decoder, &mut text).expect("decompress trace");
    }
    if hot.exists() {
        text.push_str(&std::fs::read_to_string(hot).expect("read active run trace"));
    }
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("trace event is valid JSON"))
        .collect()
}

fn assert_provider_tool_trace(
    events: &[Value],
    provider_tools: &[Vec<ironclaw_llm::ToolDefinition>],
    trace_root: &std::path::Path,
) {
    let model_requests = events
        .iter()
        .filter(|event| event["kind"] == "model_request")
        .collect::<Vec<_>>();
    assert_eq!(model_requests.len(), provider_tools.len());
    for (request, sent_tools) in model_requests.iter().zip(provider_tools) {
        let traced_tools = request["data"]["tool_definitions"]
            .as_array()
            .expect("final model request must record the provider tool definitions");
        assert_eq!(traced_tools.len(), sent_tools.len());
        for (traced, sent) in traced_tools.iter().zip(sent_tools) {
            assert_eq!(traced["name"], sent.name);
            assert_eq!(
                traced["schema_sha256"],
                ironclaw_common::hashing::sha256_hex(
                    &serde_json::to_vec(&sent.parameters).expect("schema JSON")
                )
            );
            let content_ref = traced["definition_ref"]
                .as_str()
                .expect("provider definition content reference");
            let stored = std::fs::read(
                trace_root
                    .join("content")
                    .join(format!("{content_ref}.json")),
            )
            .expect("read addressed provider definition");
            let full_hash = ironclaw_common::hashing::sha256_hex(&stored);
            assert_eq!(traced["definition_sha256"], full_hash);
            assert_eq!(content_ref, &full_hash[..16]);
            let stored: Value = serde_json::from_slice(&stored).expect("valid definition JSON");
            assert_eq!(
                stored,
                serde_json::to_value(sent).expect("provider definition JSON")
            );
        }
    }
}

#[tokio::test]
async fn reborn_tool_call_writes_correlated_request_and_response_trace() {
    let _env_lock = TRACE_ROOT_ENV_LOCK.lock().await;
    let trace_root = tempfile::tempdir().expect("isolated trace root");
    let _trace_root_guard = TraceRootGuard::set(trace_root.path());
    let group = RebornIntegrationGroup::builtin_tools()
        .await
        .expect("builtin tool group builds");
    let harness = group
        .thread("trace-event-contract")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.time",
                json!({"operation":"parse","input":1_778_590_800_123_i64}),
            ),
            RebornScriptedReply::text("time parsed"),
        ])
        .build()
        .await
        .expect("thread builds");

    let run_id = harness
        .submit_turn("parse this timestamp")
        .await
        .expect("turn completes");
    harness
        .assert_tool_invoked("builtin.time")
        .await
        .expect("time capability ran");

    let events = read_run_events(trace_root.path(), &run_id.to_string());
    let provider_tools = harness.scripted_llm.captured_tool_definitions();
    assert!(
        provider_tools
            .iter()
            .flatten()
            .any(|tool| tool.name == "builtin__time"),
        "the recorded provider really received the invoked time tool"
    );
    assert_provider_tool_trace(&events, &provider_tools, trace_root.path());
    let kinds = events
        .iter()
        .filter_map(|event| event["kind"].as_str())
        .collect::<Vec<_>>();
    // This in-process harness injects at the scripted-provider SDK seam; the
    // production gateway's model_exchange events are checked in the live
    // isolated Gateway dossier verification.
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "tool_request").count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "tool_response")
            .count(),
        1
    );

    let request = events
        .iter()
        .find(|event| event["kind"] == "tool_request")
        .expect("tool request trace exists");
    let response = events
        .iter()
        .find(|event| event["kind"] == "tool_response")
        .expect("tool response trace exists");
    assert_eq!(request["data"]["capability_id"], "builtin.time");
    assert_eq!(request["data"]["input"]["operation"], "parse");
    assert_eq!(
        request["data"]["invocation_id"],
        response["data"]["invocation_id"]
    );
    assert!(response["data"]["output"].is_object());
}
