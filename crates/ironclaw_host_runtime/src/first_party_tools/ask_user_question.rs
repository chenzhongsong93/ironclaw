use ironclaw_extensions::{CapabilityManifest, ExtensionError};
use ironclaw_host_api::{EffectKind, PermissionMode};
use serde_json::{json, Value};

use crate::FirstPartyCapabilityError;

use super::{first_party_capability_manifest, input_error, resource_profile};

/// Native host capability for structured author questions.
pub(crate) const ASK_USER_QUESTION_CAPABILITY_ID: &str = "builtin.ask_user_question";

pub(super) fn manifest() -> Result<CapabilityManifest, ExtensionError> {
    first_party_capability_manifest(
        ASK_USER_QUESTION_CAPABILITY_ID,
        "Ask the author a structured question and render selectable options in the chat card",
        vec![EffectKind::DispatchCapability],
        PermissionMode::Allow,
        resource_profile(),
    )
}

pub(super) fn dispatch(input: &Value) -> Result<Value, FirstPartyCapabilityError> {
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty() && items.len() <= 4)
        .ok_or_else(input_error)?;
    if questions.iter().any(|question| {
        question
            .get("question")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            || question
                .get("options")
                .and_then(Value::as_array)
                .is_none_or(std::vec::Vec::is_empty)
    }) {
        return Err(input_error());
    }
    Ok(json!({
        "interaction": "AskUserQuestion",
        "status": "awaiting_user",
        "questions": questions,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::dispatch;

    #[test]
    fn returns_structured_question_payload_for_valid_input() {
        let output = dispatch(&json!({
            "questions": [{
                "question": "下一步走哪条线?",
                "options": [{"label": "A", "description": "主线"}],
                "allowOther": true
            }]
        }))
        .expect("valid question input");
        assert_eq!(output["interaction"], "AskUserQuestion");
        assert_eq!(output["status"], "awaiting_user");
        assert_eq!(output["questions"][0]["options"][0]["label"], "A");
    }

    #[test]
    fn rejects_empty_or_oversized_question_lists() {
        assert!(dispatch(&json!({"questions": []})).is_err());
        assert!(dispatch(&json!({"questions": [{"question": "缺选项"}]})).is_err());
        assert!(dispatch(&json!({
            "questions": [
                {"question": "1", "options": [{"label": "A"}]},
                {"question": "2", "options": [{"label": "A"}]},
                {"question": "3", "options": [{"label": "A"}]},
                {"question": "4", "options": [{"label": "A"}]},
                {"question": "5", "options": [{"label": "A"}]}
            ]
        }))
        .is_err());
    }
}
