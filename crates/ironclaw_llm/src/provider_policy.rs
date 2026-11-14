//! Bounded classification of explicit request-scope budget stop responses.
use serde::Deserialize;

use crate::error::ProviderPolicyDeniedReason;

const MAX_POLICY_ERROR_BYTES: usize = 8192;

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    error: PermissionError,
    #[serde(default, rename = "request_id")]
    _request_id: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct PermissionError {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "message")]
    _message: String,
    #[serde(default, rename = "code")]
    code: Option<String>,
}

fn classify(raw: &[u8]) -> Option<ProviderPolicyDeniedReason> {
    if raw.len() > MAX_POLICY_ERROR_BYTES {
        return None;
    }
    // Derived struct deserialization rejects duplicate known fields.
    let value: Envelope = serde_json::from_slice(raw).ok()?;
    (value.kind == "error"
        && value.error.kind == "permission_error"
        && value.error.code.as_deref() == Some("run_budget_exceeded"))
    .then_some(ProviderPolicyDeniedReason::PermissionDenied)
}

pub(crate) async fn from_response(
    mut response: reqwest::Response,
) -> Option<ProviderPolicyDeniedReason> {
    if response.status().as_u16() != 403
        || response
            .content_length()
            .is_some_and(|size| size > MAX_POLICY_ERROR_BYTES as u64)
        || response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)?
            .to_str()
            .ok()?
            .split(';')
            .next()?
            .trim()
            .to_ascii_lowercase()
            != "application/json"
    {
        return None;
    }
    let mut raw = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if raw.len().checked_add(chunk.len())? > MAX_POLICY_ERROR_BYTES {
            return None;
        }
        raw.extend_from_slice(&chunk);
    }
    classify(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_standard_permission_error_discards_private_message() {
        let raw = br#"{"type":"error","error":{"type":"permission_error","message":"private upstream sentinel","code":"run_budget_exceeded"}}"#;
        let reason = classify(raw).expect("standard error");
        assert_eq!(reason, ProviderPolicyDeniedReason::PermissionDenied);
        assert!(!reason.to_string().contains("private"));
    }

    #[test]
    fn incomplete_duplicate_wrong_shape_and_oversize_are_unknown() {
        for raw in [
            br#"{"type":"error","error":{"type":"permission_error","code":"run_budget_exceeded","message":"x"}"#.as_slice(),
            br#"{"type":"error","type":"error","error":{"type":"permission_error","code":"run_budget_exceeded","message":"x"}}"#,
            br#"{"type":"error","error":{"type":"permission_error","type":"permission_error","code":"run_budget_exceeded","message":"x"}}"#,
            br#"{"type":"error","error":{"type":"permission_error","code":"run_budget_exceeded","message":"x","message":"x"}}"#,
            br#"{"type":"error","error":{"type":"permission_error","code":"run_budget_exceeded","code":"run_budget_exceeded","message":"x"}}"#,
            br#"{"type":"error","error":{"type":"rate_limit_error","code":"run_budget_exceeded","message":"permission_error"}}"#,
            br#"{"type":"error","error":{"type":"permission_error","code":"run_budget_exceeded"}}"#,
        ] {
            assert!(classify(raw).is_none());
        }
        assert!(classify(&vec![b' '; MAX_POLICY_ERROR_BYTES + 1]).is_none());
    }

    #[test]
    fn exact_limit_is_valid_but_limit_plus_one_and_trailing_data_are_not() {
        let base = r#"{"type":"error","error":{"type":"permission_error","code":"run_budget_exceeded","message":""}}"#;
        let payload = base.replace(
            "\"\"",
            &format!("\"{}\"", "x".repeat(MAX_POLICY_ERROR_BYTES - base.len())),
        );
        assert_eq!(payload.len(), MAX_POLICY_ERROR_BYTES);
        assert!(classify(payload.as_bytes()).is_some());
        assert!(classify(format!("{payload} ").as_bytes()).is_none());
        assert!(classify(format!("{base}garbage").as_bytes()).is_none());
    }

    #[test]
    fn ordinary_permission_denial_does_not_claim_request_scope_stop() {
        for code in [None, Some("arbitrary-code"), Some("budget_exceeded")] {
            let mut value = serde_json::json!({"type":"error","error":{"type":"permission_error","message":"ordinary permission denial"}});
            if let Some(code) = code {
                value["error"]["code"] = code.into();
            }
            assert!(classify(value.to_string().as_bytes()).is_none());
        }
    }
}
