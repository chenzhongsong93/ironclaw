//! NoModel loopback reproduction: only a synthetic public fixture token is used.
#![cfg(feature = "registry-provider-factory")]
use ironclaw_llm::config::{AnthropicAuthMode, RegistryProviderConfig};
use ironclaw_llm::registry::ProviderProtocol;
use ironclaw_llm::retry::{RetryConfig, RetryProvider};
use ironclaw_llm::{ChatMessage, CompletionRequest, LlmProvider, create_registry_provider};
use secrecy::SecretString;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

struct Fallback(AtomicUsize);

#[async_trait::async_trait]
impl LlmProvider for Fallback {
    fn model_name(&self) -> &str {
        "unused-fixture-fallback"
    }
    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
    }
    async fn complete(
        &self,
        _: CompletionRequest,
    ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ironclaw_llm::LlmError::RequestFailed {
            provider: self.model_name().into(),
            reason: "fallback must not run".into(),
        })
    }
    async fn complete_with_tools(
        &self,
        _: ironclaw_llm::ToolCompletionRequest,
    ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ironclaw_llm::LlmError::RequestFailed {
            provider: self.model_name().into(),
            reason: "fallback must not run".into(),
        })
    }
}

async fn denied_call(code: Option<&str>, streaming: bool, failover: bool) {
    exercise_response(code, streaming, failover, None).await;
}

async fn exercise_response(
    code: Option<&str>,
    streaming: bool,
    failover: bool,
    override_body: Option<(String, usize)>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let addr = listener.local_addr().expect("fixture address");
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let (stop, mut stopped) = oneshot::channel::<()>();
    let mut error = serde_json::json!({"type":"error","error":{"type":"permission_error","message":"PRIVATE_UPSTREAM_SENTINEL"}});
    if let Some(code) = code {
        error["error"]["code"] = code.into();
    }
    let expected_policy = override_body.is_none();
    let (body, extra_length) = override_body.unwrap_or_else(|| (error.to_string(), 0));
    let server = tokio::spawn(async move {
        loop {
            let accepted =
                tokio::select! { _ = &mut stopped => break, value=listener.accept()=>value };
            let (mut socket, _) = accepted.expect("fixture accept");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let size = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read(&mut chunk),
                )
                .await
                .expect("bounded fixture read")
                .expect("fixture read");
                if size == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..size]);
                assert!(request.len() <= 16384);
                if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| {
                                    value.trim().parse::<usize>().expect("fixture length")
                                })
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len() + extra_length,
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    let mut config = RegistryProviderConfig::generic(
        ProviderProtocol::Anthropic,
        "policy-fixture",
        None,
        format!("http://{addr}"),
        "fixture-model",
    )
    .with_auth_mode(Some(AnthropicAuthMode::Bearer))
    .with_max_tokens(Some(8));
    config.oauth_token = Some(SecretString::from("synthetic-fixture-token"));
    let provider = create_registry_provider(&config, 5).expect("actual factory");
    let fallback = Arc::new(Fallback(AtomicUsize::new(0)));
    let provider: Arc<dyn LlmProvider> = if failover {
        Arc::new(
            ironclaw_llm::failover::FailoverProvider::new(vec![provider, fallback.clone()])
                .expect("failover"),
        )
    } else {
        provider
    };
    let provider = RetryProvider::new(
        provider,
        RetryConfig {
            max_retries: if expected_policy { 2 } else { 0 },
        },
    );
    let result = if streaming {
        provider
            .complete_with_tools(ironclaw_llm::ToolCompletionRequest::new(
                vec![ChatMessage::user("fixture")],
                vec![],
            ))
            .await
            .map(|_| ())
    } else {
        provider
            .complete(CompletionRequest::new(vec![ChatMessage::user("fixture")]))
            .await
            .map(|_| ())
    };
    let _ = stop.send(());
    server.await.expect("original fixture joined");
    let actual = count.load(Ordering::SeqCst);
    let error = result.expect_err("policy response cannot succeed");
    println!(
        "public-fixture status=403 requests={actual} is_retryable={} original_fixture_joined=true",
        ironclaw_llm::retry::is_retryable(&error)
    );
    assert_eq!(actual, 1, "standard policy denial must not retry");
    assert_eq!(ironclaw_llm::retry::is_retryable(&error), !expected_policy);
    assert_eq!(
        matches!(
            &error,
            ironclaw_llm::LlmError::PolicyDenied {
                reason: ironclaw_llm::error::ProviderPolicyDeniedReason::PermissionDenied,
                ..
            }
        ),
        expected_policy
    );
    assert!(!format!("{error:?} {error}").contains("PRIVATE_UPSTREAM"));
    assert_eq!(
        fallback.0.load(Ordering::SeqCst),
        0,
        "policy denial must not fail over"
    );
}

#[tokio::test]
async fn explicit_run_stop_http_is_not_retried() {
    denied_call(Some("run_budget_exceeded"), false, false).await;
}

#[tokio::test]
async fn explicit_run_stop_budget_report_is_not_retried() {
    denied_call(Some("run_budget_exceeded"), false, false).await;
}

#[tokio::test]
async fn streaming_explicit_run_stop_http_is_not_retried() {
    denied_call(Some("run_budget_exceeded"), true, false).await;
}

#[tokio::test]
async fn explicit_run_stop_http_does_not_fail_over() {
    denied_call(Some("run_budget_exceeded"), false, true).await;
}

#[tokio::test]
async fn explicit_run_stop_message_is_not_persisted_in_typed_error() {
    denied_call(Some("run_budget_exceeded"), false, false).await;
}

#[tokio::test]
async fn malformed_duplicate_trailing_oversize_and_truncated_http_remain_unknown() {
    let valid = r#"{"type":"error","error":{"type":"permission_error","message":"PRIVATE_UPSTREAM_SENTINEL","code":"run_budget_exceeded"}}"#;
    for (body, extra) in [
        (
            valid.replace(
                "\"type\":\"error\"",
                "\"type\":\"error\",\"type\":\"error\"",
            ),
            0,
        ),
        (format!("{valid}garbage"), 0),
        (
            valid.replace("PRIVATE_UPSTREAM_SENTINEL", &"x".repeat(8192)),
            0,
        ),
        (valid.to_owned(), 10),
        (
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"permission_error"}}"#
                .to_owned(),
            0,
        ),
    ] {
        exercise_response(None, true, false, Some((body, extra))).await;
    }
}

struct ConfiguredBackup(AtomicUsize);

#[async_trait::async_trait]
impl LlmProvider for ConfiguredBackup {
    fn model_name(&self) -> &str {
        "configured-backup-fixture"
    }
    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
    }
    async fn complete(
        &self,
        _: CompletionRequest,
    ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ironclaw_llm::CompletionResponse {
            normalization_version: None,
            normalized_input_tokens: None,
            cache_details_observed: None,
            content: "configured backup fixture".into(),
            input_tokens: 0,
            output_tokens: 1,
            finish_reason: ironclaw_llm::FinishReason::Stop,
            reasoning: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }
    async fn complete_with_tools(
        &self,
        _: ironclaw_llm::ToolCompletionRequest,
    ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
        unreachable!("text fixture")
    }
}

async fn ordinary_permission_uses_backup(code: Option<&'static str>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let addr = listener.local_addr().expect("fixture address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("fixture accept");
        let mut request = Vec::new();
        loop {
            let mut buffer = [0u8; 4096];
            let n = socket.read(&mut buffer).await.expect("fixture read");
            assert!(n > 0);
            request.extend_from_slice(&buffer[..n]);
            assert!(request.len() <= 16384);
            if let Some(end) = request.windows(4).position(|x| x == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&request[..end]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                            .map(|(_, v)| v.trim().parse::<usize>().expect("length"))
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        let mut error = serde_json::json!({"type":"error","error":{"type":"permission_error","message":"primary permission denied"}});
        if let Some(code) = code {
            error["error"]["code"] = code.into();
        }
        let body = error.to_string();
        socket.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.expect("fixture response");
        socket.shutdown().await.expect("fixture close");
    });
    let mut config = RegistryProviderConfig::generic(
        ProviderProtocol::Anthropic,
        "primary-fixture",
        None,
        format!("http://{addr}"),
        "fixture-model",
    )
    .with_auth_mode(Some(AnthropicAuthMode::Bearer));
    config.oauth_token = Some(SecretString::from("synthetic-fixture-token"));
    let primary = create_registry_provider(&config, 5).expect("actual provider factory");
    let backup = Arc::new(ConfiguredBackup(AtomicUsize::new(0)));
    let chain = ironclaw_llm::failover::FailoverProvider::new(vec![primary, backup.clone()])
        .expect("configured chain");
    let result = chain
        .complete(CompletionRequest::new(vec![ChatMessage::user("fixture")]))
        .await;
    server.await.expect("original HTTP fixture joined");
    println!(
        "public ordinary_permission backup_calls={} result_success={}",
        backup.0.load(Ordering::SeqCst),
        result.is_ok()
    );
    assert!(
        result.is_ok(),
        "ordinary primary permission denial must preserve configured backup"
    );
    assert_eq!(backup.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ordinary_permission_denial_uses_configured_backup() {
    ordinary_permission_uses_backup(None).await;
}

#[tokio::test]
async fn arbitrary_and_old_budget_codes_preserve_configured_backup() {
    ordinary_permission_uses_backup(Some("PRIVATE_UPSTREAM_CODE")).await;
    ordinary_permission_uses_backup(Some("budget_exceeded")).await;
}
