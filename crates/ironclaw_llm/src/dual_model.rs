//! Dual-model provider router for Reborn.
//!
//! Routes each model request to one of two providers based on the per-request
//! model override: requests whose override equals the *mission* model name go
//! to the mission provider (e.g. minimax, cheaper/secondary), everything else
//! goes to the default provider (the primary model).
//!
//! Used by TianQuan's dual-model wiring: the orchestrating parent agent runs on
//! the default provider (deepseek-v4-flash) while the `novelist` writer
//! subagent — whose run profile binds `mission_model` — is routed to the
//! mission provider (minimax). See `ironclaw_runner::planned_driver_factory`
//! (`subagent_novelist_planned_profile_definition`) and the runtime assembly in
//! `ironclaw_reborn_composition::runtime`.
//!
//! The two inner providers are expected to be hot-swappable
//! ([`SwappableLlmProvider`]): the reload path swaps each inner independently
//! and the router keeps delegating to the live swappable.

use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;

use crate::error::LlmError;
use crate::provider::{
    CompletionRequest, CompletionResponse, CompletionStreamSink, LlmProvider, ModelMetadata,
    ToolCompletionRequest, ToolCompletionResponse,
};

/// Routes requests to a default or mission provider by model override.
pub struct DualModelRouter {
    default: Arc<dyn LlmProvider>,
    mission: Arc<dyn LlmProvider>,
    /// Model name that selects the mission provider when used as a per-request
    /// override (e.g. `MiniMax-M3`).
    mission_model: String,
}

impl DualModelRouter {
    /// Build a router dispatching on the mission model name.
    ///
    /// `mission_model` must be non-empty; requests carrying that exact model
    /// override are served by `mission`, all others by `default`.
    pub fn new(
        default: Arc<dyn LlmProvider>,
        mission: Arc<dyn LlmProvider>,
        mission_model: impl Into<String>,
    ) -> Result<Self, LlmError> {
        let mission_model = mission_model.into();
        if mission_model.trim().is_empty() {
            return Err(LlmError::RequestFailed {
                provider: "dual_model".to_string(),
                reason: "mission model name must be non-empty".to_string(),
            });
        }
        Ok(Self {
            default,
            mission,
            mission_model,
        })
    }

    fn provider_for(&self, model_override: Option<&str>) -> Arc<dyn LlmProvider> {
        match model_override.map(str::trim) {
            Some(model) if model == self.mission_model => Arc::clone(&self.mission),
            _ => Arc::clone(&self.default),
        }
    }
}

#[async_trait]
impl LlmProvider for DualModelRouter {
    fn model_name(&self) -> &str {
        self.default.model_name()
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        self.default.cost_per_token()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let provider = self.provider_for(request.model.as_deref());
        provider.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<CompletionResponse, LlmError> {
        let provider = self.provider_for(request.model.as_deref());
        provider.complete_streaming(request, sink).await
    }

    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        // 2026-08-29 二分定位日志(治子 agent 210s 挂起):router 收到/转发各一条
        let chosen = self.provider_for(request.model.as_deref());
        tracing::warn!(
            model = ?request.model.as_deref(),
            routed_to = chosen.model_name(),
            messages = request.messages.len(),
            "DualModelRouter: complete_with_tools ENTER"
        );
        let result = chosen.complete_with_tools(request).await;
        tracing::warn!(
            ok = result.is_ok(),
            "DualModelRouter: complete_with_tools EXIT"
        );
        result
    }

    async fn complete_with_tools_streaming(
        &self,
        request: ToolCompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let provider = self.provider_for(request.model.as_deref());
        provider.complete_with_tools_streaming(request, sink).await
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.default.list_models().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.default.model_metadata().await
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        self.default.effective_model_name(requested_model)
    }

    fn active_model_name(&self) -> String {
        self.default.active_model_name()
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        self.default.set_model(model)
    }

    fn calculate_cost(&self, input_tokens: u32, output_tokens: u32) -> Decimal {
        self.default.calculate_cost(input_tokens, output_tokens)
    }

    fn cache_write_multiplier(&self) -> Decimal {
        self.default.cache_write_multiplier()
    }

    fn cache_read_discount(&self) -> Decimal {
        self.default.cache_read_discount()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ChatMessage, FinishReason};

    struct StubProvider {
        name: &'static str,
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn model_name(&self) -> &str {
            self.name
        }
        fn cost_per_token(&self) -> (Decimal, Decimal) {
            (Decimal::ZERO, Decimal::ZERO)
        }
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: self.name.to_string(),
                input_tokens: 0,
                output_tokens: 0,
                finish_reason: FinishReason::Stop,
                reasoning: None,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }
        async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<ToolCompletionResponse, LlmError> {
            Ok(ToolCompletionResponse {
                content: Some(self.name.to_string()),
                tool_calls: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
                finish_reason: FinishReason::Stop,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning: None,
                reasoning_details: None,
            })
        }
    }

    #[tokio::test]
    async fn routes_by_model_override() {
        let default = Arc::new(StubProvider { name: "default" });
        let mission = Arc::new(StubProvider { name: "mission" });
        let router = DualModelRouter::new(default, mission, "MiniMax-M3").unwrap();

        // Default request (no mission override) → default provider.
        let resp = router
            .complete(CompletionRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .unwrap();
        assert_eq!(resp.content, "default");

        // Mission override → mission provider.
        let mut req = CompletionRequest::new(vec![ChatMessage::user("hi")]);
        req.model = Some("MiniMax-M3".to_string());
        let resp = router.complete(req).await.unwrap();
        assert_eq!(resp.content, "mission");

        // Non-mission override → default provider.
        let mut req = CompletionRequest::new(vec![ChatMessage::user("hi")]);
        req.model = Some("deepseek-v4-flash".to_string());
        let resp = router.complete(req).await.unwrap();
        assert_eq!(resp.content, "default");
    }

    #[test]
    fn rejects_empty_mission_model() {
        let default = Arc::new(StubProvider { name: "default" });
        let mission = Arc::new(StubProvider { name: "mission" });
        assert!(DualModelRouter::new(default, mission, "  ").is_err());
    }
}
