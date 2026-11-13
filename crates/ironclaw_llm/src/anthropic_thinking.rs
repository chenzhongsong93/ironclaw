use serde::Serialize;

const THINKING_BUDGET_TOKENS: u32 = 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub(crate) enum AnthropicThinking {
    #[serde(rename = "enabled")]
    Enabled {
        budget_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<&'static str>,
    },
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<&'static str>,
    },
    #[serde(rename = "disabled")]
    Disabled,
}

pub(crate) fn thinking_for_request(
    model: &str,
    max_tokens: u32,
    temperature: Option<f32>,
    forces_tool_use: bool,
) -> Option<AnthropicThinking> {
    if max_tokens <= THINKING_BUDGET_TOKENS || temperature.is_some() || forces_tool_use {
        return None;
    }

    let lower = model.to_ascii_lowercase();
    if crate::reasoning_models::supports_anthropic_adaptive_thinking(&lower) {
        return Some(AnthropicThinking::Adaptive {
            display: Some("summarized"),
        });
    }
    if crate::reasoning_models::supports_anthropic_enabled_thinking(&lower) {
        return Some(AnthropicThinking::Enabled {
            budget_tokens: THINKING_BUDGET_TOKENS,
            display: Some("summarized"),
        });
    }
    None
}

/// Resolve the effective upstream model plus thinking intent.
///
/// `@thinking:off` is a host-private route marker: it must never be sent as
/// the upstream model id because the shared router channel is keyed by the
/// base model. Strip the marker before routing and send an explicit disabled
/// thinking intent in the Anthropic Messages payload instead.
pub(crate) fn thinking_for_request_model(
    model: &str,
    max_tokens: u32,
    temperature: Option<f32>,
    forces_tool_use: bool,
) -> (String, Option<AnthropicThinking>) {
    if let Some(base) = model.strip_suffix("@thinking:off") {
        return (base.to_string(), Some(AnthropicThinking::Disabled));
    }
    (
        model.to_string(),
        thinking_for_request(model, max_tokens, temperature, forces_tool_use),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_thinking_to_supported_models_and_valid_request_shapes() {
        assert!(thinking_for_request("claude-sonnet-4-6", 8192, None, false).is_some());
        assert!(thinking_for_request("claude-3-7-sonnet-latest", 8192, None, false).is_some());
        assert!(thinking_for_request("claude-3-5-sonnet-latest", 8192, None, false).is_none());
        assert!(thinking_for_request("claude-sonnet-4-20250514", 1024, None, false).is_none());
        assert!(thinking_for_request("claude-sonnet-4-20250514", 8192, Some(0.2), false).is_none());
        assert!(thinking_for_request("claude-sonnet-4-20250514", 8192, None, true).is_none());
    }

    /// Regression: `claude-4-5-sonnet`, `claude-sonnet-4-5`, and `claude-haiku-4*\`
    /// are non-thinking Claude 4.x variants that must NOT trigger enabled thinking.
    #[test]
    fn thinking_disabled_for_non_thinking_claude_4_variants() {
        assert!(
            thinking_for_request("claude-4-5-sonnet", 8192, None, false).is_none(),
            "claude-4-5-sonnet should not enable thinking",
        );
        assert!(
            thinking_for_request("claude-sonnet-4-5-20250919", 8192, None, false).is_none(),
            "claude-sonnet-4-5-* should not enable thinking",
        );
        assert!(
            thinking_for_request("claude-haiku-4-5-20251001", 8192, None, false).is_none(),
            "claude-haiku-4-5-* should not enable thinking",
        );
    }

    #[test]
    fn thinking_for_request_returns_adaptive_variants_where_supported() {
        match thinking_for_request("claude-sonnet-4-6-20250501", 8192, None, false) {
            Some(AnthropicThinking::Adaptive { .. }) => {}
            other => panic!("expected Adaptive thinking, got {other:?}"),
        }
        match thinking_for_request("claude-opus-4-7", 8192, None, false) {
            Some(AnthropicThinking::Adaptive { .. }) => {}
            other => panic!("expected Adaptive thinking, got {other:?}"),
        }
    }

    #[test]
    fn thinking_for_request_returns_enabled_variants_where_supported() {
        match thinking_for_request("claude-3-7-sonnet-latest", 8192, None, false) {
            Some(AnthropicThinking::Enabled { .. }) => {}
            other => panic!("expected Enabled thinking, got {other:?}"),
        }
        match thinking_for_request("claude-sonnet-4-2-20250415", 8192, None, false) {
            Some(AnthropicThinking::Enabled { .. }) => {}
            other => panic!("expected Enabled thinking, got {other:?}"),
        }
    }
    #[test]
    fn thinking_off_suffix_strips_router_model_and_marks_explicitly_disabled() {
        let (model, thinking) =
            thinking_for_request_model("deepseek-v4.1-flash@thinking:off", 8192, None, false);
        assert_eq!(model, "deepseek-v4.1-flash");
        assert!(matches!(thinking, Some(AnthropicThinking::Disabled)));
        assert_eq!(
            serde_json::to_value(thinking).unwrap(),
            serde_json::json!({"type": "disabled"})
        );
    }
}
