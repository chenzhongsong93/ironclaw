// arch-exempt: large_file, mechanical SSE parser lint fix in the existing provider pending transport decomposition, plan #3031
//! Anthropic OAuth provider (direct HTTP, `Authorization: Bearer`).
//!
//! This provider exists because the `rig-core` Anthropic client hardcodes the
//! `x-api-key` header, which is rejected by Anthropic's OAuth tokens from
//! `claude login`. OAuth tokens require `Authorization: Bearer <token>` instead.
//!
//! Pattern follows `nearai_chat.rs`: direct HTTP calls via `reqwest::Client`.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::anthropic_thinking::{AnthropicThinking, thinking_for_request_model};
use crate::config::RegistryProviderConfig;
use crate::error::LlmError;
use crate::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, CompletionStreamSink, ContentPart, FinishReason, LlmProvider,
    Role, ToolCall, ToolCompletionRequest, ToolCompletionResponse,
    strip_unsupported_completion_params, strip_unsupported_tool_params,
};
use ironclaw_common::llm_costs as costs;

#[path = "anthropic_request_headers.rs"]
mod request_headers;

/// Read a fresh `claude login` OAuth token from the OS credential store.
///
/// Mirrors `ironclaw::config::ClaudeCodeConfig::extract_oauth_token` but is
/// inlined here so this crate doesn't depend on the main binary. Used to
/// retry once after a 401 if the user's OAuth token has been rotated by
/// Claude Code's background refresh.
fn refresh_claude_oauth_token() -> Option<String> {
    if cfg!(target_os = "macos") {
        match std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                if let Ok(json) = String::from_utf8(output.stdout) {
                    return parse_oauth_access_token(json.trim());
                }
            }
            _ => {}
        }
    }
    if let Some(home) = dirs::home_dir() {
        let creds_path = home.join(".claude").join(".credentials.json");
        if let Ok(json) = std::fs::read_to_string(&creds_path) {
            return parse_oauth_access_token(&json);
        }
    }
    None
}

fn parse_oauth_access_token(json: &str) -> Option<String> {
    let creds: serde_json::Value = serde_json::from_str(json).ok()?;
    let token = creds["claudeAiOauth"]["accessToken"].as_str()?;
    if !token.starts_with("sk-ant-oat") {
        tracing::debug!("Ignoring credential store token with unexpected prefix");
        return None;
    }
    Some(token.to_string())
}
/// Map an HTTP error status + response body to a context-length error when it
/// indicates the prompt exceeded the model's context window.
///
/// Returns `Some(LlmError::ContextLengthExceeded { .. })` for HTTP 413 or for
/// an HTTP 400 whose body matches a context-overflow pattern, and `None`
/// otherwise. Delegates to the shared `crate::error::context_length_error`
/// helper so detection stays consistent across direct-HTTP providers.
fn context_length_error_for_status(status_code: u16, response_text: &str) -> Option<LlmError> {
    crate::error::context_length_error(status_code, response_text)
}

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";

async fn policy_response_error(response: reqwest::Response) -> LlmError {
    match crate::provider_policy::from_response(response).await {
        Some(reason) => LlmError::PolicyDenied {
            provider: "anthropic_oauth".to_owned(),
            reason,
        },
        None => LlmError::RequestFailed {
            provider: "anthropic_oauth".to_owned(),
            reason: "HTTP 403: policy response classification is unknown".to_owned(),
        },
    }
}

/// A custom Bearer endpoint cannot authorize reading an unrelated local credential.
/// Bind both the configured destination and the final response destination.
fn allows_claude_cli_refresh(requested_url: &str, response_url: &str) -> bool {
    let official =
        |value: &str| url::Url::parse(value).is_ok_and(|url| url.as_str() == ANTHROPIC_API_URL);
    official(requested_url) && official(response_url)
}
/// OAuth beta requires 2023-06-01; the 2024-10-22 version is not valid with the beta flag.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Required beta flag to enable OAuth Bearer auth on api.anthropic.com.
/// Without this header, the API returns 401 "OAuth authentication is currently not supported."
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
/// Fallback `max_tokens` when neither the request nor the provider config
/// set one. Also reused by the rig-core Anthropic path (lib.rs factory).
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Anthropic provider using OAuth Bearer authentication.
pub(crate) struct AnthropicOAuthProvider {
    client: Client,
    request_headers: request_headers::RequestHeaders,
    provider_id: String,
    /// OAuth token, wrapped in RwLock so it can be updated after a successful
    /// Keychain refresh (fixes #1136: stale token reuse after expiry).
    token: std::sync::RwLock<SecretString>,
    model: String,
    base_url: Option<String>,
    active_model: std::sync::RwLock<String>,
    /// Parameter names that this provider does not support.
    unsupported_params: HashSet<String>,
    /// Fallback `max_tokens` when a request leaves it unset
    /// (`config.max_tokens` at construction, else [`DEFAULT_MAX_TOKENS`]).
    default_max_tokens: u32,
    /// Unit-only credential IO seam; never configurable by runtime or model input.
    #[cfg(test)]
    cli_refresh_loader: Option<fn() -> Option<String>>,
}

impl AnthropicOAuthProvider {
    pub(crate) fn new(
        config: &RegistryProviderConfig,
        request_timeout_secs: u64,
    ) -> Result<Self, LlmError> {
        let token = config
            .oauth_token
            .clone()
            .ok_or_else(|| LlmError::AuthFailed {
                provider: "anthropic_oauth".to_string(),
            })?;

        // 2026-08-20 天权补丁:超时随链配置(LLM_REQUEST_TIMEOUT_SECS)而非硬编码默认 180s
        // ——火山网关(Anthropic 协议)全量写手任务非流式生成实测 386s,180s 必超时致子 run
        // 悬挂(spawn 后 provider 调用超时,child 卡非终态阻塞后续 spawn)。
        let client = crate::config::hardened_client_builder(request_timeout_secs)
            .build()
            .map_err(|e| LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: format!("Failed to build HTTP client: {}", e),
            })?;

        let active_model = std::sync::RwLock::new(config.model.clone());
        let request_headers = request_headers::RequestHeaders::new(&config.extra_headers)?;
        let base_url = if config.base_url.is_empty() {
            None
        } else {
            Some(config.base_url.clone())
        };

        let unsupported_params: HashSet<String> =
            config.unsupported_params.iter().cloned().collect();

        Ok(Self {
            client,
            request_headers,
            provider_id: config.provider_id.clone(),
            token: std::sync::RwLock::new(token),
            model: config.model.clone(),
            base_url,
            active_model,
            unsupported_params,
            default_max_tokens: config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            #[cfg(test)]
            cli_refresh_loader: None,
        })
    }

    /// Strip unsupported fields from a `CompletionRequest` in place.
    fn strip_unsupported_completion_params(&self, req: &mut CompletionRequest) {
        strip_unsupported_completion_params(&self.unsupported_params, req);
    }

    /// Strip unsupported fields from a `ToolCompletionRequest` in place.
    fn strip_unsupported_tool_params(&self, req: &mut ToolCompletionRequest) {
        strip_unsupported_tool_params(&self.unsupported_params, req);
    }

    fn api_url(&self) -> String {
        if let Some(ref base) = self.base_url {
            let base = base.trim_end_matches('/');
            format!("{}/v1/messages", base)
        } else {
            ANTHROPIC_API_URL.to_string()
        }
    }

    /// Read the current token from the RwLock.
    fn current_token(&self) -> String {
        match self.token.read() {
            Ok(guard) => guard.expose_secret().to_string(),
            Err(poisoned) => poisoned.into_inner().expose_secret().to_string(),
        }
    }

    async fn token_for_request(
        &self,
        metadata: &std::collections::HashMap<String, String>,
    ) -> Result<(String, bool), LlmError> {
        let registered = crate::subject_keys::registered_subject_key_resolver();
        let subject = metadata
            .get("llm_subject")
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        match (subject, registered) {
            (Some(subject), Some((resolver, _policy))) => resolver
                .resolve_bearer_token(subject, &self.provider_id)
                .await
                .map_err(|error| match error {
                    crate::SubjectKeyResolveError::SubjectRequired => {
                        LlmError::SubjectAuthRejected {
                            code: "llm_subject_required",
                        }
                    }
                    crate::SubjectKeyResolveError::UnknownSubject => {
                        LlmError::SubjectAuthRejected {
                            code: "llm_subject_unknown",
                        }
                    }
                    crate::SubjectKeyResolveError::StoreUnavailable => {
                        LlmError::SubjectAuthRejected {
                            code: "llm_subject_store_unavailable",
                        }
                    }
                })?
                .map(|token| (token.expose_secret().to_string(), true))
                .ok_or(LlmError::SubjectAuthRejected {
                    code: "llm_subject_unknown",
                }),
            (Some(_), None) => Err(LlmError::SubjectAuthRejected {
                code: "llm_subject_unknown",
            }),
            (None, Some((_resolver, policy))) if policy.required => {
                Err(LlmError::SubjectAuthRejected {
                    code: "llm_subject_required",
                })
            }
            (None, _) => Ok((self.current_token(), false)),
        }
    }

    /// Update the stored token after a successful Keychain refresh.
    fn update_token(&self, new_token: SecretString) {
        match self.token.write() {
            Ok(mut guard) => *guard = new_token,
            Err(poisoned) => *poisoned.into_inner() = new_token,
        }
    }

    /// 流式发送请求并聚合为完整响应(2026-08-28 天权补丁,治子 agent 长生成死循环)。
    ///
    /// 实测根因:Anthropic 协议兼容网关(火山)对**非流式**长生成请求(≥2500 字正文)
    /// 返回慢(136s+)且 content 为空;**流式**首 token 1.9s 正常。子 agent(novelist)
    /// 走 complete_with_tools 非流式 → 永远拿不到可用响应,重试死循环。
    /// 修法:tool-capable 请求改走 stream + SSE 聚合,与 Anthropic 官方流式协议对齐
    /// (message_start/content_block_start/content_block_delta/message_delta 聚合回
    /// AnthropicResponse,调用方零改动)。
    async fn send_request_streaming(
        &self,
        body: &AnthropicRequest,
        bearer_token: &str,
        subject_scoped: bool,
        extra_headers: &reqwest::header::HeaderMap,
        sink: Option<&dyn CompletionStreamSink>,
        trace: Option<&crate::provider_wire_trace::ProviderWireTrace>,
    ) -> Result<AnthropicResponse, LlmError> {
        let url = self.api_url();
        let mut streamed = body.clone();
        streamed.stream = Some(true);
        let wire = serde_json::to_vec(&streamed).map_err(|_| LlmError::RequestFailed {
            provider: "anthropic_oauth".to_string(),
            reason: "request serialization failed".to_string(),
        })?;
        let physical_request = trace.map(|trace| trace.observe_anthropic(&wire));

        tracing::warn!(
            url = %url,
            "Sending STREAMING request to Anthropic OAuth"
        );
        let response = self
            .client
            .post(&url)
            .headers(extra_headers.clone())
            .bearer_auth(bearer_token)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .body(wire)
            .send()
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: format!("streaming request failed: {e}"),
            })?;
        let status = response.status();
        if let (Some(trace), Some(id)) = (trace, physical_request.as_deref()) {
            trace.observe_response(id, status.as_u16());
        }
        if status.as_u16() == 403 {
            return Err(policy_response_error(response).await);
        }
        if !status.is_success() {
            let response_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));
            if subject_scoped && status.as_u16() == 401 {
                return Err(LlmError::SubjectAuthRejected {
                    code: "llm_subject_key_invalid",
                });
            }
            return Err(LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: format!("streaming HTTP {status}: {response_text}"),
            });
        }

        use futures::StreamExt;
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut aggregator = SseAggregator::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: format!("streaming body read failed: {e}"),
            })?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line_end) = buffer.find('\n') {
                let line: String = buffer.drain(..=line_end).collect();
                let line = line.trim_end();
                if let Some(payload) = line.strip_prefix("data:")
                    && let Ok(event) = serde_json::from_str::<SseEvent>(payload.trim())
                {
                    if let Some(sink) = sink {
                        match &event {
                            SseEvent::ContentBlockDelta { delta: SseDelta::TextDelta { text }, .. }
                                if !text.is_empty() => {
                                    sink.text_delta(text.clone()).await;
                                }
                            SseEvent::ContentBlockDelta { delta: SseDelta::ThinkingDelta { thinking }, .. }
                                if !thinking.is_empty() => sink.provider_activity().await,
                            SseEvent::ContentBlockDelta { delta: SseDelta::InputJsonDelta { partial_json }, .. }
                                if !partial_json.is_empty() => sink.provider_activity().await,
                            _ => {}
                        }
                    }
                    aggregator.absorb(event);
                }
            }
        }
        tracing::debug!(
            blocks = aggregator.blocks.len(),
            "Anthropic OAuth streaming aggregation complete"
        );
        aggregator.finish()
    }

    async fn refresh_token_after_401(
        &self,
        requested_url: &str,
        response_url: &str,
    ) -> Option<String> {
        if !allows_claude_cli_refresh(requested_url, response_url) {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        #[cfg(test)]
        if let Some(load) = self.cli_refresh_loader {
            return load();
        }
        refresh_claude_oauth_token()
    }

    async fn send_request<R: for<'de> Deserialize<'de>>(
        &self,
        body: &AnthropicRequest,
        bearer_token: &str,
        subject_scoped: bool,
        extra_headers: &reqwest::header::HeaderMap,
        trace: Option<&crate::provider_wire_trace::ProviderWireTrace>,
    ) -> Result<R, LlmError> {
        let url = self.api_url();
        let wire = serde_json::to_vec(body).map_err(|_| LlmError::RequestFailed {
            provider: "anthropic_oauth".to_string(),
            reason: "request serialization failed".to_string(),
        })?;

        tracing::warn!("Sending request to Anthropic OAuth: {}", url);

        let physical_request = trace.map(|trace| trace.observe_anthropic(&wire));

        let response = self
            .client
            .post(&url)
            .headers(extra_headers.clone())
            .bearer_auth(bearer_token)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
            .header("Content-Type", "application/json")
            .body(wire.clone())
            .send()
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: e.to_string(),
            })?;

        let status = response.status();

        if let (Some(trace), Some(id)) = (trace, physical_request.as_deref()) {
            trace.observe_response(id, status.as_u16());
        }

        if status.as_u16() == 403 {
            return Err(policy_response_error(response).await);
        }
        if !status.is_success() {
            let response_url = response.url().to_string();
            // Parse Retry-After header before consuming the body.
            let retry_after = Some(crate::retry::parse_retry_after(
                response.headers().get("retry-after"),
            ));

            let response_text = response
                .text()
                .await
                .unwrap_or_else(|e| format!("(failed to read error body: {e})"));

            if status.as_u16() == 401 {
                // A subject-scoped key is authoritative for that subject. It
                // must never fall back to or refresh the deployment shared key
                // (ISSUE-IRONCLAW-008 fail-closed rule).
                if subject_scoped {
                    return Err(LlmError::SubjectAuthRejected {
                        code: "llm_subject_key_invalid",
                    });
                }
                // OAuth tokens from `claude login` expire in ~8-12h. Attempt
                // to re-extract a fresh token from the OS credential store
                // (macOS Keychain / Linux credentials file) before giving up.
                //
                // Brief delay to give Claude Code time to complete its async
                // Keychain refresh write (fixes race in #1136).
                if let Some(fresh) = self.refresh_token_after_401(&url, &response_url).await {
                    let fresh_token = SecretString::from(fresh);
                    // Retry once with the refreshed token
                    let retry_request = trace.map(|trace| trace.observe_anthropic(&wire));
                    let retry = self
                        .client
                        .post(&url)
                        .headers(extra_headers.clone())
                        .bearer_auth(fresh_token.expose_secret())
                        .header("anthropic-version", ANTHROPIC_API_VERSION)
                        .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
                        .header("Content-Type", "application/json")
                        .body(wire.clone())
                        .send()
                        .await
                        .map_err(|e| LlmError::RequestFailed {
                            provider: "anthropic_oauth".to_string(),
                            reason: e.to_string(),
                        })?;
                    if let (Some(trace), Some(id)) = (trace, retry_request.as_deref()) {
                        trace.observe_response(id, retry.status().as_u16());
                    }
                    if retry.status().as_u16() == 403 {
                        return Err(policy_response_error(retry).await);
                    }
                    if retry.status().is_success() {
                        // Persist the refreshed token so subsequent requests
                        // don't hit 401 again (fixes #1136).
                        self.update_token(fresh_token);
                        tracing::info!("Anthropic OAuth token refreshed from credential store");

                        let text = retry.text().await.map_err(|e| LlmError::RequestFailed {
                            provider: "anthropic_oauth".to_string(),
                            reason: format!("Failed to read response body: {}", e),
                        })?;
                        return serde_json::from_str(&text).map_err(|e| {
                            let truncated = ironclaw_common::truncate_for_preview(&text, 512);
                            LlmError::InvalidResponse {
                                provider: "anthropic_oauth".to_string(),
                                reason: format!("JSON parse error: {}. Raw: {}", e, truncated),
                            }
                        });
                    }
                    tracing::warn!(
                        "Anthropic OAuth 401 retry with refreshed token also failed ({})",
                        retry.status()
                    );
                }
                return Err(LlmError::AuthFailed {
                    provider: "anthropic_oauth".to_string(),
                });
            }
            if status.as_u16() == 429 {
                return Err(LlmError::RateLimited {
                    provider: "anthropic_oauth".to_string(),
                    retry_after,
                });
            }
            // A too-large prompt (HTTP 413, or a 400 whose body says the context
            // window was exceeded) must map to ContextLengthExceeded so the
            // loop's context-shrink recovery can compact and retry instead of
            // borking on a generic RequestFailed.
            if let Some(error) = context_length_error_for_status(status.as_u16(), &response_text) {
                tracing::warn!("Anthropic OAuth: context length exceeded");
                return Err(error);
            }
            let truncated = ironclaw_common::truncate_for_preview(&response_text, 512);
            return Err(LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: format!("HTTP {}: {}", status, truncated),
            });
        }

        let response_text = response.text().await.map_err(|e| LlmError::RequestFailed {
            provider: "anthropic_oauth".to_string(),
            reason: format!("Failed to read response body: {}", e),
        })?;

        tracing::debug!(
            "Anthropic OAuth response: status={}, bytes={}",
            status,
            response_text.len()
        );

        serde_json::from_str(&response_text).map_err(|e| {
            let truncated = ironclaw_common::truncate_for_preview(&response_text, 512);
            LlmError::InvalidResponse {
                provider: "anthropic_oauth".to_string(),
                reason: format!("JSON parse error: {}. Raw: {}", e, truncated),
            }
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicOAuthProvider {
    async fn complete(&self, mut req: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let trace = crate::provider_wire_trace::ProviderWireTrace::from_metadata(&req.metadata);
        let headers = self.request_headers.resolve(&req.metadata)?;
        let (bearer_token, subject_scoped) = self.token_for_request(&req.metadata).await?;
        let model = req
            .take_model_override()
            .unwrap_or_else(|| self.active_model_name());
        self.strip_unsupported_completion_params(&mut req);
        let (system, messages) = convert_messages(req.messages);
        let max_tokens = req.max_tokens.unwrap_or(self.default_max_tokens);
        let (model, thinking) =
            thinking_for_request_model(&model, max_tokens, req.temperature, false);

        let request = AnthropicRequest {
            stream: None,
            thinking,
            model,
            messages,
            system,
            max_tokens,
            temperature: req.temperature,
            tools: None,
            tool_choice: None,
        };

        let response: AnthropicResponse = self
            .send_request(&request, &bearer_token, subject_scoped, &headers, trace.as_ref())
            .await?;
        let extracted = extract_response_content(&response);

        let finish_reason = match response.stop_reason.as_deref() {
            Some("end_turn") | Some("stop") => FinishReason::Stop,
            Some("max_tokens") => FinishReason::Length,
            Some("tool_use") => FinishReason::ToolUse,
            _ => FinishReason::Unknown,
        };

        let normalized = response.usage.normalization_fields_present.then(|| crate::usage_normalization::parse_anthropic_messages_usage(&serde_json::json!({
            "input_tokens":response.usage.input_tokens,"output_tokens":response.usage.output_tokens,
            "cache_read_input_tokens":response.usage.cache_read_input_tokens,
            "cache_creation_input_tokens":response.usage.cache_creation_input_tokens
        }))).and_then(Result::ok);
        let normalization_version = normalized.map(|_| 1);
        Ok(CompletionResponse {
            content: extracted.content.unwrap_or_default(),
            finish_reason,
            input_tokens: response.usage.input_tokens,
            normalization_version,
            normalized_input_tokens: normalized.map(|value| value.parts().0),
            cache_details_observed: normalized.map(|value| value.cache_details_observed()),
            output_tokens: response.usage.output_tokens,
            reasoning: extracted.reasoning,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        })
    }

    async fn complete_with_tools(
        &self,
        req: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        self.complete_anthropic_tools(req, None).await
    }

    async fn complete_with_tools_streaming(
        &self,
        req: ToolCompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        self.complete_anthropic_tools(req, Some(sink.as_ref())).await
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        let model = self.active_model_name();
        costs::model_cost(&model).unwrap_or_else(costs::default_cost)
    }

    fn active_model_name(&self) -> String {
        match self.active_model.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        match self.active_model.write() {
            Ok(mut guard) => {
                *guard = model.to_string();
            }
            Err(poisoned) => {
                *poisoned.into_inner() = model.to_string();
            }
        }
        Ok(())
    }
}

impl AnthropicOAuthProvider {
    async fn complete_anthropic_tools(
        &self,
        mut req: ToolCompletionRequest,
        sink: Option<&dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let trace = crate::provider_wire_trace::ProviderWireTrace::from_metadata(&req.metadata);
        let headers = self.request_headers.resolve(&req.metadata)?;
        let (bearer_token, subject_scoped) = self.token_for_request(&req.metadata).await?;
        let model = req
            .take_model_override()
            .unwrap_or_else(|| self.active_model_name());
        self.strip_unsupported_tool_params(&mut req);
        let (system, messages) = convert_messages(req.messages);

        let tools: Vec<AnthropicTool> = req
            .tools
            .into_iter()
            .map(|t| AnthropicTool {
                name: t.name,
                description: t.description,
                input_schema: t.parameters,
            })
            .collect();

        // Map tool_choice from OpenAI format to Anthropic format
        let tool_choice = req.tool_choice.map(|tc| match tc.as_str() {
            "auto" => AnthropicToolChoice {
                choice_type: "auto".to_string(),
                name: None,
            },
            "required" => AnthropicToolChoice {
                choice_type: "any".to_string(),
                name: None,
            },
            "none" => AnthropicToolChoice {
                choice_type: "none".to_string(),
                name: None,
            },
            specific => AnthropicToolChoice {
                choice_type: "tool".to_string(),
                name: Some(specific.to_string()),
            },
        });
        let max_tokens = req.max_tokens.unwrap_or(self.default_max_tokens);

        // Suppress thinking for tool-capable requests to avoid signature round-trip issues.
        // Anthropic requires signed thinking blocks to be echoed back on subsequent tool_result
        // turns; without round-tripping the signature, the next turn fails. Reasoning is
        // preserved on text-only turns via `complete()`.
        let has_tools = !tools.is_empty();
        let opt_tools = if has_tools { Some(tools) } else { None };
        let (model, request_thinking) =
            thinking_for_request_model(&model, max_tokens, req.temperature, false);

        let request = AnthropicRequest {
            stream: None,
            thinking: if has_tools { None } else { request_thinking },
            model,
            messages,
            system,
            max_tokens,
            temperature: req.temperature,
            tools: opt_tools,
            tool_choice,
        };

        // 2026-08-28 天权补丁:tool-capable 请求走流式聚合(非流式长生成在兼容
        // 网关上慢且空,见 send_request_streaming 注释)。
        let response: AnthropicResponse = self
            .send_request_streaming(&request, &bearer_token, subject_scoped, &headers, sink, trace.as_ref())
            .await?;
        let extracted = extract_response_content(&response);

        let finish_reason = match response.stop_reason.as_deref() {
            Some("end_turn") | Some("stop") => FinishReason::Stop,
            Some("max_tokens") => FinishReason::Length,
            Some("tool_use") => FinishReason::ToolUse,
            _ => {
                if !extracted.tool_calls.is_empty() {
                    FinishReason::ToolUse
                } else {
                    FinishReason::Unknown
                }
            }
        };

        let normalized = response.usage.normalization_fields_present.then(|| crate::usage_normalization::parse_anthropic_messages_usage(&serde_json::json!({
            "input_tokens":response.usage.input_tokens,"output_tokens":response.usage.output_tokens,
            "cache_read_input_tokens":response.usage.cache_read_input_tokens,
            "cache_creation_input_tokens":response.usage.cache_creation_input_tokens
        }))).and_then(Result::ok);
        let normalization_version = normalized.map(|_| 1);
        Ok(ToolCompletionResponse {
            content: extracted.content,
            tool_calls: extracted.tool_calls,
            finish_reason,
            input_tokens: response.usage.input_tokens,
            normalization_version,
            normalized_input_tokens: normalized.map(|value| value.parts().0),
            cache_details_observed: normalized.map(|value| value.cache_details_observed()),
            output_tokens: response.usage.output_tokens,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            reasoning: extracted.reasoning,
            reasoning_details: None,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        })
    }

}

// --- Anthropic Messages API types ---

#[derive(Debug, Clone, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    /// 流式开关(send_request_streaming 设 true;None=非流式,序列化省略)。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

/// Anthropic content can be a simple string or a list of content blocks.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Inline base64 image source for an Anthropic `image` content block.
#[derive(Debug, Clone, Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: &'static str,
    media_type: String,
    data: String,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    choice_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

/// SSE 流事件(Anthropic Messages streaming 协议子集,仅取聚合所需字段)。
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SseEvent {
    #[serde(rename = "message_start")]
    MessageStart {
        #[serde(default)]
        message: SseMessageUsage,
    },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: SseBlockStart,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: SseDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        #[serde(default)]
        delta: SseStopDelta,
        #[serde(default)]
        usage: SseUsageDelta,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(other)]
    Ignored,
}

#[derive(Debug, Default, Deserialize)]
struct SseMessageUsage {
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Debug, Default, Deserialize)]
struct SseUsageDelta {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default, deserialize_with = "observed_usage_u32")]
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}

#[derive(Debug, Default, Deserialize)]
struct SseStopDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseBlockStart {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum SseDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Ignored,
}

/// SSE 聚合器:事件流 → AnthropicResponse(纯逻辑,单测覆盖)。
#[derive(Default)]
struct SseAggregator {
    blocks: std::collections::BTreeMap<usize, AggregatingBlock>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
    final_output_observed: bool,
    message_stop_seen: bool,
    message_start_seen: bool,
    usage_sequence_fault: bool,
    initial_output_reported: u32,
}

enum AggregatingBlock {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
}

impl SseAggregator {
    fn absorb(&mut self, event: SseEvent) {
        if self.message_stop_seen && !matches!(&event, SseEvent::Ignored) {
            self.usage_sequence_fault = true;
        }
        match event {
            SseEvent::MessageStart { message } => {
                if self.message_start_seen {
                    self.usage_sequence_fault = true;
                }
                self.message_start_seen = true;
                self.initial_output_reported = message.usage.output_tokens;
                self.usage.normalization_fields_present =
                    message.usage.normalization_fields_present;
                self.final_output_observed = false;
                self.usage.input_tokens = message.usage.input_tokens;
                self.usage.cache_creation_input_tokens = message.usage.cache_creation_input_tokens;
                self.usage.cache_read_input_tokens = message.usage.cache_read_input_tokens;
            }
            SseEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let block = match content_block.block_type.as_str() {
                    "tool_use" => AggregatingBlock::ToolUse {
                        id: content_block.id.unwrap_or_default(),
                        name: content_block.name.unwrap_or_default(),
                        json: String::new(),
                    },
                    "thinking" => AggregatingBlock::Thinking(String::new()),
                    _ => AggregatingBlock::Text(String::new()),
                };
                self.blocks.insert(index, block);
            }
            SseEvent::ContentBlockDelta { index, delta } => match delta {
                SseDelta::TextDelta { text } => {
                    if let Some(AggregatingBlock::Text(buf)) = self.blocks.get_mut(&index) {
                        buf.push_str(&text);
                    }
                }
                SseDelta::ThinkingDelta { thinking } => {
                    if let Some(AggregatingBlock::Thinking(buf)) = self.blocks.get_mut(&index) {
                        buf.push_str(&thinking);
                    }
                }
                SseDelta::InputJsonDelta { partial_json } => {
                    if let Some(AggregatingBlock::ToolUse { json, .. }) =
                        self.blocks.get_mut(&index)
                    {
                        json.push_str(&partial_json);
                    }
                }
                SseDelta::Ignored => {}
            },
            SseEvent::MessageDelta { delta, usage } => {
                if !self.message_start_seen {
                    self.usage_sequence_fault = true;
                }
                if delta.stop_reason.is_some() {
                    self.stop_reason = delta.stop_reason;
                }
                if let Some(output) = usage.output_tokens {
                    if output < self.usage.output_tokens || output < self.initial_output_reported {
                        self.usage_sequence_fault = true;
                    }
                    self.final_output_observed = true;
                    if output > 0 {
                        self.usage.output_tokens = output;
                    }
                }
            }
            SseEvent::MessageStop => self.message_stop_seen = true,
            SseEvent::Ignored => {}
        }
    }

    fn finish(mut self) -> Result<AnthropicResponse, LlmError> {
        self.usage.normalization_fields_present &= self.message_start_seen
            && self.final_output_observed
            && self.message_stop_seen
            && !self.usage_sequence_fault;
        if self.blocks.is_empty() {
            return Err(LlmError::RequestFailed {
                provider: "anthropic_oauth".to_string(),
                reason: "streaming response contained no content blocks".to_string(),
            });
        }
        let content = self
            .blocks
            .into_values()
            .map(|block| match block {
                AggregatingBlock::Text(text) => AnthropicResponseBlock::Text { text },
                AggregatingBlock::Thinking(thinking) => AnthropicResponseBlock::Thinking {
                    thinking: Some(thinking),
                    summary: None,
                    _signature: None,
                },
                AggregatingBlock::ToolUse { id, name, json } => AnthropicResponseBlock::ToolUse {
                    id,
                    name,
                    input: serde_json::from_str(&json).unwrap_or(serde_json::Value::Null),
                },
            })
            .collect();
        Ok(AnthropicResponse {
            content,
            stop_reason: self.stop_reason,
            usage: self.usage,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default, rename = "signature")]
        _signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {},
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default)]
struct AnthropicUsage {
    // Internal presence proof, never read from a model/user field.
    normalization_fields_present: bool,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: u32,
    cache_read_input_tokens: u32,
}
fn observed_usage_u32<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    u32::deserialize(deserializer).map(Some)
}
impl<'de> serde::Deserialize<'de> for AnthropicUsage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default, deserialize_with = "observed_usage_u32")]
            input_tokens: Option<u32>,
            #[serde(default, deserialize_with = "observed_usage_u32")]
            output_tokens: Option<u32>,
            #[serde(default, deserialize_with = "observed_usage_u32")]
            cache_creation_input_tokens: Option<u32>,
            #[serde(default, deserialize_with = "observed_usage_u32")]
            cache_read_input_tokens: Option<u32>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let normalization_fields_present = raw.input_tokens.is_some()
            && raw.output_tokens.is_some()
            && raw.cache_creation_input_tokens.is_some()
            && raw.cache_read_input_tokens.is_some();
        Ok(Self {
            normalization_fields_present,
            input_tokens: raw.input_tokens.unwrap_or_default(),
            output_tokens: raw.output_tokens.unwrap_or_default(),
            cache_creation_input_tokens: raw.cache_creation_input_tokens.unwrap_or_default(),
            cache_read_input_tokens: raw.cache_read_input_tokens.unwrap_or_default(),
        })
    }
}

/// Build Anthropic `image` content blocks from a user message's multimodal
/// parts. Only inline base64 `data:` images are forwarded (the Anthropic
/// messages API also accepts `url` sources, but the model gateway always emits
/// `data:` URLs); anything else is skipped so the text still reaches the model.
fn user_image_blocks(parts: &[ContentPart]) -> Vec<AnthropicContentBlock> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ImageUrl { image_url } => {
                let (media_type, data) = image_url.decode_data_url()?;
                Some(AnthropicContentBlock::Image {
                    source: AnthropicImageSource {
                        source_type: "base64",
                        media_type: media_type.to_string(),
                        data: data.to_string(),
                    },
                })
            }
            ContentPart::Text { .. } => None,
        })
        .collect()
}

/// Convert ChatMessage list to Anthropic format.
///
/// Extracts system messages to the top-level `system` parameter (Anthropic
/// doesn't allow system messages in the `messages` array). Tool-call/tool-result
/// pairs are converted to content blocks.
fn convert_messages(messages: Vec<ChatMessage>) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut anthropic_msgs: Vec<AnthropicMessage> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if !msg.content.is_empty() {
                    system_parts.push(msg.content);
                }
            }
            Role::User => {
                let content = match user_image_blocks(&msg.content_parts) {
                    // Text-only (or no inline images): keep the compact string form.
                    blocks if blocks.is_empty() => AnthropicContent::Text(msg.content),
                    // Multimodal: text block first (when present), then images.
                    image_blocks => {
                        let mut blocks = Vec::with_capacity(1 + image_blocks.len());
                        if !msg.content.is_empty() {
                            blocks.push(AnthropicContentBlock::Text { text: msg.content });
                        }
                        blocks.extend(image_blocks);
                        AnthropicContent::Blocks(blocks)
                    }
                };
                anthropic_msgs.push(AnthropicMessage {
                    role: "user".to_string(),
                    content,
                });
            }
            Role::Assistant => {
                if let Some(tool_calls) = msg.tool_calls {
                    // Assistant message with tool calls → content blocks
                    let mut blocks: Vec<AnthropicContentBlock> = Vec::new();
                    if !msg.content.is_empty() {
                        blocks.push(AnthropicContentBlock::Text { text: msg.content });
                    }
                    for tc in tool_calls {
                        blocks.push(AnthropicContentBlock::ToolUse {
                            id: tc.id,
                            name: tc.name,
                            input: tc.arguments,
                        });
                    }
                    anthropic_msgs.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                } else {
                    anthropic_msgs.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: AnthropicContent::Text(msg.content),
                    });
                }
            }
            Role::Tool => {
                let Some(tool_call_id) = msg.tool_call_id else {
                    tracing::warn!("Skipping Tool message without tool_call_id");
                    continue;
                };
                // Tool results go into a user message with tool_result blocks
                let block = AnthropicContentBlock::ToolResult {
                    tool_use_id: tool_call_id,
                    content: msg.content,
                };
                // If the last message is already a user message of *only*
                // tool-result blocks, append to it (Anthropic requires
                // consecutive tool results in one user message). Crucially, do
                // not merge into a multimodal user prompt (text + image
                // blocks) — that would fold a tool result into a different
                // conversational turn.
                if let Some(last) = anthropic_msgs.last_mut()
                    && last.role == "user"
                    && let AnthropicContent::Blocks(ref mut blocks) = last.content
                    && blocks
                        .iter()
                        .all(|b| matches!(b, AnthropicContentBlock::ToolResult { .. }))
                {
                    blocks.push(block);
                    continue;
                }
                anthropic_msgs.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: AnthropicContent::Blocks(vec![block]),
                });
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, anthropic_msgs)
}

#[derive(Debug, Default)]
struct ExtractedAnthropicResponse {
    content: Option<String>,
    reasoning: Option<String>,
    tool_calls: Vec<ToolCall>,
}

/// Extract text content and tool calls from an Anthropic response.
fn extract_response_content(response: &AnthropicResponse) -> ExtractedAnthropicResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in &response.content {
        match block {
            AnthropicResponseBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            AnthropicResponseBlock::Thinking {
                thinking,
                summary,
                _signature: _,
            } => {
                if let Some(reasoning) = summary
                    .as_deref()
                    .filter(|summary| !summary.trim().is_empty())
                    .or_else(|| {
                        thinking
                            .as_deref()
                            .filter(|thinking| !thinking.trim().is_empty())
                    })
                {
                    reasoning_parts.push(reasoning.to_string());
                }
            }
            AnthropicResponseBlock::RedactedThinking {} | AnthropicResponseBlock::Other => {}
            AnthropicResponseBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                    reasoning: None,
                    signature: None,
                    arguments_parse_error: None,
                });
            }
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    ExtractedAnthropicResponse {
        content,
        reasoning,
        tool_calls,
    }
}

#[cfg(test)]
#[path = "anthropic_oauth_header_tests.rs"]
mod request_header_tests;

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static CLI_REFRESH_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn fixture_cli_refresh() -> Option<String> {
        CLI_REFRESH_READS.with(|count| count.set(count.get() + 1));
        Some("fixture-cli-credential".to_string())
    }

    async fn custom_401_caller(subject_scoped: bool) {
        use std::io::{Read, Write};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::clone(&stop);
        let observed = Arc::clone(&headers);
        let server = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                            .unwrap();
                        let mut raw = Vec::new();
                        let mut bytes = [0; 2048];
                        loop {
                            let n = socket.read(&mut bytes).unwrap();
                            assert!(n > 0);
                            raw.extend_from_slice(&bytes[..n]);
                            assert!(raw.len() <= 65536);
                            if let Some(end) = raw.windows(4).position(|p| p == b"\r\n\r\n") {
                                let head = std::str::from_utf8(&raw[..end]).unwrap();
                                let length = head
                                    .lines()
                                    .find_map(|line| {
                                        line.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .map(|v| v.trim().parse::<usize>().unwrap())
                                    })
                                    .unwrap_or(0);
                                if raw.len() >= end + 4 + length {
                                    let authorization = head
                                        .lines()
                                        .find_map(|line| {
                                            line.split_once(':')
                                                .filter(|(key, _)| {
                                                    key.eq_ignore_ascii_case("authorization")
                                                })
                                                .map(|(_, value)| value.trim().to_owned())
                                        })
                                        .unwrap();
                                    observed.lock().unwrap().push(authorization);
                                    break;
                                }
                            }
                        }
                        let body = br#"{"error":{"type":"authentication_error","message":"fixture denied"}}"#;
                        write!(socket, "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                        socket.write_all(body).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        let mut config = RegistryProviderConfig::generic(
            crate::registry::ProviderProtocol::Anthropic,
            "fixture",
            None,
            origin,
            "fixture-model",
        );
        config.oauth_token = Some(SecretString::from("fixture-original-token"));
        let mut provider = AnthropicOAuthProvider::new(&config, 5).unwrap();
        provider.client = reqwest::Client::builder().no_proxy().build().unwrap();
        provider.cli_refresh_loader = Some(fixture_cli_refresh);
        CLI_REFRESH_READS.with(|count| count.set(0));
        let body = AnthropicRequest {
            model: "fixture-model".into(),
            messages: vec![],
            stream: None,
            system: None,
            max_tokens: 1,
            temperature: None,
            thinking: None,
            tools: None,
            tool_choice: None,
        };
        let result = provider
            .send_request::<serde_json::Value>(&body, "fixture-original-token", subject_scoped, &reqwest::header::HeaderMap::new(), None)
            .await;
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let reads = CLI_REFRESH_READS.with(std::cell::Cell::get);
        let headers = headers.lock().unwrap();
        eprintln!(
            "CustomBearer401Fixture:requests={},refreshReads={}",
            headers.len(),
            reads
        );
        assert_eq!(
            reads, 0,
            "custom endpoints must not read host CLI credentials"
        );
        assert_eq!(
            headers.len(),
            1,
            "401 must not retry with another credential"
        );
        assert_eq!(headers[0], "Bearer fixture-original-token");
        if subject_scoped {
            assert!(matches!(
                result,
                Err(LlmError::SubjectAuthRejected {
                    code: "llm_subject_key_invalid"
                })
            ));
        } else {
            assert!(matches!(result, Err(LlmError::AuthFailed { .. })));
        }
    }

    // TEST-SPEC-custom-bearer-401 CB01/CB04: actual HTTP and original send_request.
    #[tokio::test(flavor = "current_thread")]
    async fn custom_bearer_401_never_reads_cli_credentials() {
        custom_401_caller(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn custom_bearer_subject_401_preserves_subject_rejection() {
        custom_401_caller(true).await;
    }

    #[test]
    fn custom_bearer_refresh_origin_requires_both_exact_official_destinations() {
        assert!(allows_claude_cli_refresh(
            ANTHROPIC_API_URL,
            ANTHROPIC_API_URL
        ));
        assert!(allows_claude_cli_refresh(
            "https://API.ANTHROPIC.com:443/v1/messages",
            ANTHROPIC_API_URL
        ));
        for custom in [
            "http://127.0.0.1:3100/v1/messages",
            "https://proxy.invalid/v1/messages",
            "https://api.anthropic.com.evil.invalid/v1/messages",
            "https://user@api.anthropic.com/v1/messages",
            "http://api.anthropic.com/v1/messages",
            "https://api.anthropic.com:8443/v1/messages",
            "https://api.anthropic.com/v1/messages?override=1",
            "https://api.anthropic.com/v1/messages#fragment",
            "https://api.anthropic.com/other",
        ] {
            assert!(!allows_claude_cli_refresh(custom, ANTHROPIC_API_URL));
            assert!(!allows_claude_cli_refresh(ANTHROPIC_API_URL, custom));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn custom_bearer_guard_preserves_official_cli_refresh_only() {
        let mut config = RegistryProviderConfig::generic(
            crate::registry::ProviderProtocol::Anthropic,
            "fixture",
            None,
            String::new(),
            "fixture-model",
        );
        config.oauth_token = Some(SecretString::from("fixture-original-token"));
        let mut provider = AnthropicOAuthProvider::new(&config, 5).unwrap();
        provider.cli_refresh_loader = Some(fixture_cli_refresh);
        CLI_REFRESH_READS.with(|count| count.set(0));
        assert!(
            provider
                .refresh_token_after_401("http://127.0.0.1:3100/v1/messages", ANTHROPIC_API_URL)
                .await
                .is_none()
        );
        assert_eq!(CLI_REFRESH_READS.with(std::cell::Cell::get), 0);
        let fresh = provider
            .refresh_token_after_401(ANTHROPIC_API_URL, ANTHROPIC_API_URL)
            .await;
        assert_eq!(fresh.as_deref(), Some("fixture-cli-credential"));
        assert_eq!(CLI_REFRESH_READS.with(std::cell::Cell::get), 1);
    }

    #[test]
    fn context_overflow_413_maps_to_context_length_exceeded() {
        // A raw HTTP 413 (payload too large) must become ContextLengthExceeded
        // so the loop's context-shrink recovery fires.
        match context_length_error_for_status(413, "Request Entity Too Large") {
            Some(LlmError::ContextLengthExceeded { .. }) => {}
            other => panic!("expected ContextLengthExceeded, got {other:?}"),
        }
    }

    #[test]
    fn context_overflow_400_body_maps_to_context_length_exceeded() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 234872 tokens > 200000 maximum"}}"#;
        match context_length_error_for_status(400, body) {
            Some(LlmError::ContextLengthExceeded { used, limit }) => {
                assert_eq!(used, 234872);
                assert_eq!(limit, 200000);
            }
            other => panic!("expected ContextLengthExceeded, got {other:?}"),
        }
    }

    #[test]
    fn unrelated_400_is_not_context_overflow() {
        // A plain bad-request (e.g. invalid request shape) must NOT be
        // classified as context overflow — the caller falls through to
        // RequestFailed.
        assert!(
            context_length_error_for_status(400, r#"{"error":{"message":"invalid request body"}}"#)
                .is_none()
        );
    }

    #[test]
    fn unrelated_5xx_is_not_context_overflow() {
        assert!(context_length_error_for_status(503, "service unavailable").is_none());
    }

    #[test]
    fn test_convert_messages_extracts_system() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
        ];
        let (system, msgs) = convert_messages(messages);
        assert_eq!(system, Some("You are helpful.".to_string()));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn test_convert_messages_multiple_systems() {
        let messages = vec![
            ChatMessage::system("System 1"),
            ChatMessage::system("System 2"),
            ChatMessage::user("Hello"),
        ];
        let (system, msgs) = convert_messages(messages);
        assert_eq!(system, Some("System 1\n\nSystem 2".to_string()));
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn test_convert_messages_user_image_becomes_base64_image_block() {
        let messages = vec![ChatMessage::user_with_parts(
            "what is this?",
            vec![ContentPart::ImageUrl {
                image_url: crate::provider::ImageUrl {
                    url: "data:image/png;base64,AQIDBA==".to_string(),
                    detail: None,
                },
            }],
        )];
        let (_system, msgs) = convert_messages(messages);
        assert_eq!(msgs.len(), 1);
        // Text rides as the first block, the image as a base64 `image` block.
        let value = serde_json::to_value(&msgs[0]).expect("serialize");
        let blocks = value["content"].as_array().expect("content blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "what is this?");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AQIDBA==");
    }

    #[test]
    fn test_convert_messages_text_only_user_stays_a_string() {
        let messages = vec![ChatMessage::user("just text")];
        let (_system, msgs) = convert_messages(messages);
        let value = serde_json::to_value(&msgs[0]).expect("serialize");
        // No inline images → compact string content, not a blocks array.
        assert_eq!(value["content"], "just text");
    }

    #[test]
    fn test_convert_messages_tool_calls() {
        let tool_calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "search".to_string(),
            arguments: serde_json::json!({"q": "test"}),
            reasoning: None,
            signature: None,
            arguments_parse_error: None,
        }];
        let messages = vec![
            ChatMessage::user("Search for test"),
            ChatMessage::assistant_with_tool_calls(Some("Let me search.".to_string()), tool_calls),
            ChatMessage::tool_result("call_1", "search", "found it"),
        ];
        let (system, msgs) = convert_messages(messages);
        assert!(system.is_none());
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        // Tool result should be a user message
        assert_eq!(msgs[2].role, "user");
    }

    #[test]
    fn test_extract_response_text_only() {
        let response = AnthropicResponse {
            content: vec![AnthropicResponseBlock::Text {
                text: "Hello!".to_string(),
            }],
            stop_reason: Some("end_turn".to_string()),
            usage: AnthropicUsage {
                normalization_fields_present: false,
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        let extracted = extract_response_content(&response);
        assert_eq!(extracted.content, Some("Hello!".to_string()));
        assert!(extracted.tool_calls.is_empty());
    }

    #[test]
    fn test_extract_response_with_tool_use() {
        let response = AnthropicResponse {
            content: vec![
                AnthropicResponseBlock::Text {
                    text: "Let me search.".to_string(),
                },
                AnthropicResponseBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "search".to_string(),
                    input: serde_json::json!({"q": "test"}),
                },
            ],
            stop_reason: Some("tool_use".to_string()),
            usage: AnthropicUsage {
                normalization_fields_present: false,
                input_tokens: 20,
                output_tokens: 15,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        let extracted = extract_response_content(&response);
        assert_eq!(extracted.content, Some("Let me search.".to_string()));
        assert_eq!(extracted.tool_calls.len(), 1);
        assert_eq!(extracted.tool_calls[0].name, "search");
    }

    #[test]
    fn test_extract_response_preserves_thinking_as_reasoning() {
        let response = AnthropicResponse {
            content: vec![
                AnthropicResponseBlock::Thinking {
                    thinking: Some("Raw thinking".to_string()),
                    summary: Some("Summarized thinking".to_string()),
                    _signature: Some("sig".to_string()),
                },
                AnthropicResponseBlock::Text {
                    text: "Done.".to_string(),
                },
            ],
            stop_reason: Some("end_turn".to_string()),
            usage: AnthropicUsage {
                normalization_fields_present: false,
                input_tokens: 20,
                output_tokens: 15,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        let extracted = extract_response_content(&response);
        assert_eq!(extracted.content, Some("Done.".to_string()));
        assert_eq!(extracted.reasoning, Some("Summarized thinking".to_string()));
    }

    #[test]
    fn test_extract_response_uses_thinking_when_summary_absent() {
        let response = AnthropicResponse {
            content: vec![
                AnthropicResponseBlock::Thinking {
                    thinking: Some("Raw thinking fallback".to_string()),
                    summary: None,
                    _signature: Some("sig".to_string()),
                },
                AnthropicResponseBlock::Text {
                    text: "Done.".to_string(),
                },
            ],
            stop_reason: Some("end_turn".to_string()),
            usage: AnthropicUsage {
                normalization_fields_present: false,
                input_tokens: 20,
                output_tokens: 15,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };

        let extracted = extract_response_content(&response);

        assert_eq!(extracted.content, Some("Done.".to_string()));
        assert_eq!(
            extracted.reasoning,
            Some("Raw thinking fallback".to_string())
        );
    }

    /// Regression test for #1136: token field must be mutable via RwLock
    /// so that a refreshed token persists across subsequent requests.
    #[test]
    fn test_token_update_persists() {
        let original = SecretString::from("old_token".to_string());
        let token = std::sync::RwLock::new(original);

        // Read the original
        assert_eq!(token.read().unwrap().expose_secret(), "old_token");

        // Simulate a successful refresh
        let refreshed = SecretString::from("new_token".to_string());
        *token.write().unwrap() = refreshed;

        // Subsequent reads see the updated token
        assert_eq!(token.read().unwrap().expose_secret(), "new_token");
    }
}

// 覆盖:2026-08-28 天权补丁——SSE 流式聚合(治子 agent 长生成非流式死循环)
#[cfg(test)]
mod streaming_tests;

#[cfg(test)]
mod normalized_usage_presence_tests {
    use super::*;
    fn event(aggregator: &mut SseAggregator, value: serde_json::Value) {
        aggregator.absorb(serde_json::from_value(value).unwrap());
    }
    fn aggregate(final_usage: bool, stop: bool) -> AnthropicResponse {
        let mut original = SseAggregator::default();
        event(
            &mut original,
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
        );
        event(
            &mut original,
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
        );
        if final_usage {
            event(
                &mut original,
                serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":0}}),
            );
        }
        if stop {
            event(&mut original, serde_json::json!({"type":"message_stop"}));
        }
        original.finish().unwrap()
    }
    #[test]
    fn actual_zero_requires_present_final_usage_and_message_stop() {
        assert!(aggregate(true, true).usage.normalization_fields_present);
        assert!(!aggregate(false, true).usage.normalization_fields_present);
        assert!(!aggregate(true, false).usage.normalization_fields_present);
        let missing: AnthropicUsage = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!missing.normalization_fields_present);
        assert_eq!(missing.input_tokens, 0); // Old raw projection remains compatible, never V1.
    }

    #[test]
    fn null_counters_and_post_stop_or_decreasing_usage_cannot_mint_v1() {
        assert!(
            serde_json::from_value::<AnthropicUsage>(serde_json::json!({"input_tokens":null}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<SseUsageDelta>(serde_json::json!({"output_tokens":null}))
                .is_err()
        );
        let mut original = SseAggregator::default();
        event(
            &mut original,
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":2,"output_tokens":0,"cache_read_input_tokens":7,"cache_creation_input_tokens":100}}}),
        );
        event(
            &mut original,
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
        );
        event(
            &mut original,
            serde_json::json!({"type":"message_delta","usage":{"output_tokens":3}}),
        );
        event(
            &mut original,
            serde_json::json!({"type":"message_delta","usage":{"output_tokens":0}}),
        );
        event(&mut original, serde_json::json!({"type":"message_stop"}));
        assert!(
            !original
                .finish()
                .unwrap()
                .usage
                .normalization_fields_present
        );
        let mut post = SseAggregator::default();
        event(
            &mut post,
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
        );
        event(
            &mut post,
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
        );
        event(
            &mut post,
            serde_json::json!({"type":"message_delta","usage":{"output_tokens":0}}),
        );
        event(&mut post, serde_json::json!({"type":"message_stop"}));
        event(
            &mut post,
            serde_json::json!({"type":"message_delta","usage":{"output_tokens":0}}),
        );
        assert!(!post.finish().unwrap().usage.normalization_fields_present);
    }
    fn aggregate_original_start_and_delta(
        start: serde_json::Value,
        delta_usage: serde_json::Value,
    ) -> AnthropicResponse {
        let mut original = SseAggregator::default();
        event(
            &mut original,
            serde_json::json!({"type":"message_start","message":start}),
        );
        event(
            &mut original,
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}),
        );
        event(
            &mut original,
            serde_json::json!({"type":"message_delta","usage":delta_usage}),
        );
        event(&mut original, serde_json::json!({"type":"message_stop"}));
        original.finish().unwrap()
    }
    #[test]
    fn message_start_full_presence_is_required_even_with_final_explicit_zero_and_stop() {
        let full = serde_json::json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
        let known = aggregate_original_start_and_delta(
            serde_json::json!({"usage":full.clone()}),
            serde_json::json!({"output_tokens":0}),
        );
        assert!(known.usage.normalization_fields_present);
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            let mut missing = full.clone();
            missing.as_object_mut().unwrap().remove(key);
            let response = aggregate_original_start_and_delta(
                serde_json::json!({"usage":missing}),
                serde_json::json!({"output_tokens":0}),
            );
            assert!(
                !response.usage.normalization_fields_present,
                "missing {key} cannot qualify raw zero"
            );
            assert_eq!(response.usage.output_tokens, 0); // Only raw backwards-compatible projection.
        }
        assert!(
            !aggregate_original_start_and_delta(
                serde_json::json!({}),
                serde_json::json!({"output_tokens":0})
            )
            .usage
            .normalization_fields_present
        );
        assert!(
            !aggregate_original_start_and_delta(
                serde_json::json!({"usage":full}),
                serde_json::json!({})
            )
            .usage
            .normalization_fields_present
        );
    }
    #[test]
    fn message_start_null_and_initial_output_lower_bound_cannot_mint_known_usage() {
        let full = serde_json::json!({"input_tokens":0,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
        for key in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            let mut null = full.clone();
            null[key] = serde_json::Value::Null;
            assert!(
                serde_json::from_value::<SseEvent>(
                    serde_json::json!({"type":"message_start","message":{"usage":null}})
                )
                .is_err(),
                "null {key}"
            );
        }
        assert!(
            serde_json::from_value::<SseEvent>(
                serde_json::json!({"type":"message_start","message":{"usage":null}})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<SseEvent>(
                serde_json::json!({"type":"message_delta","usage":{"output_tokens":null}})
            )
            .is_err()
        );
        let decreasing = aggregate_original_start_and_delta(
            serde_json::json!({"usage":full.clone()}),
            serde_json::json!({"output_tokens":4}),
        );
        assert!(!decreasing.usage.normalization_fields_present);
        let monotonic = aggregate_original_start_and_delta(
            serde_json::json!({"usage":full}),
            serde_json::json!({"output_tokens":5}),
        );
        assert!(monotonic.usage.normalization_fields_present);
        assert_eq!(monotonic.usage.output_tokens, 5);
    }
}

#[cfg(test)]
mod subject_policy_tests {
    use super::*;

    fn oauth_test_config(max_tokens: Option<u32>) -> crate::config::RegistryProviderConfig {
        let mut config = crate::config::RegistryProviderConfig::generic(
            crate::registry::ProviderProtocol::Anthropic,
            "anthropic",
            Some(secrecy::SecretString::from(
                crate::config::OAUTH_PLACEHOLDER,
            )),
            "",
            "test-model",
        )
        .with_max_tokens(max_tokens);
        config.oauth_token = Some(secrecy::SecretString::from("test-oauth-token"));
        config
    }

    struct TestSubjectResolver;

    #[async_trait]
    impl crate::SubjectKeyResolver for TestSubjectResolver {
        async fn resolve_bearer_token(
            &self,
            subject: &str,
            _provider_id: &str,
        ) -> Result<Option<SecretString>, crate::SubjectKeyResolveError> {
            Ok((subject == "known").then(|| SecretString::from("subject-token")))
        }
    }

    static SUBJECT_RESOLVER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn optional_subject_policy_uses_shared_token_only_when_subject_absent() {
        let _guard = SUBJECT_RESOLVER_TEST_LOCK.lock().await;
        crate::subject_keys::clear_subject_key_resolver();
        crate::register_subject_key_resolver(
            std::sync::Arc::new(TestSubjectResolver),
            crate::SubjectKeyPolicy::OPTIONAL,
        );
        let provider = AnthropicOAuthProvider::new(&oauth_test_config(None), 60).unwrap();

        let empty = std::collections::HashMap::new();
        assert_eq!(
            provider.token_for_request(&empty).await.unwrap(),
            ("test-oauth-token".to_string(), false)
        );
        let known =
            std::collections::HashMap::from([("llm_subject".to_string(), "known".to_string())]);
        assert_eq!(
            provider.token_for_request(&known).await.unwrap(),
            ("subject-token".to_string(), true)
        );
        crate::subject_keys::clear_subject_key_resolver();
    }

    #[tokio::test]
    async fn required_subject_policy_rejects_missing_and_unknown_without_fallback() {
        let _guard = SUBJECT_RESOLVER_TEST_LOCK.lock().await;
        crate::subject_keys::clear_subject_key_resolver();
        crate::register_subject_key_resolver(
            std::sync::Arc::new(TestSubjectResolver),
            crate::SubjectKeyPolicy::REQUIRED,
        );
        let provider = AnthropicOAuthProvider::new(&oauth_test_config(None), 60).unwrap();

        let missing = provider
            .token_for_request(&std::collections::HashMap::new())
            .await
            .expect_err("missing subject must fail closed");
        assert!(matches!(
            missing,
            LlmError::SubjectAuthRejected {
                code: "llm_subject_required"
            }
        ));

        let unknown =
            std::collections::HashMap::from([("llm_subject".to_string(), "unknown".to_string())]);
        let error = provider
            .token_for_request(&unknown)
            .await
            .expect_err("unknown subject must fail closed");
        assert!(matches!(
            error,
            LlmError::SubjectAuthRejected {
                code: "llm_subject_unknown"
            }
        ));
        crate::subject_keys::clear_subject_key_resolver();
    }
}
