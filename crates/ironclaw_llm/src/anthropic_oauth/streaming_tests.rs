use super::*;

fn feed(aggregator: &mut SseAggregator, json_line: &str) {
    let event: SseEvent = serde_json::from_str(json_line).expect("测试事件应可解析");
    aggregator.absorb(event);
}

#[test]
fn sse_text_and_tool_use_aggregate() {
    let mut agg = SseAggregator::default();
    feed(
        &mut agg,
        r#"{"type":"message_start","message":{"usage":{"input_tokens":120,"cache_read_input_tokens":30}}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"正文"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"续写"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"write_file"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"/tmp/a\"}"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":456}}"#,
    );
    feed(&mut agg, r#"{"type":"message_stop"}"#);
    let resp = agg.finish().expect("聚合应成功");
    assert_eq!(resp.usage.input_tokens, 120);
    assert_eq!(resp.usage.cache_read_input_tokens, 30);
    assert_eq!(resp.usage.output_tokens, 456);
    assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
    match &resp.content[0] {
        AnthropicResponseBlock::Text { text } => assert_eq!(text, "正文续写"),
        other => panic!("块 0 应为 Text: {other:?}"),
    }
    match &resp.content[1] {
        AnthropicResponseBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "tu_1");
            assert_eq!(name, "write_file");
            assert_eq!(input["path"], "/tmp/a");
        }
        other => panic!("块 1 应为 ToolUse: {other:?}"),
    }
}

#[test]
fn sse_thinking_block_aggregates() {
    let mut agg = SseAggregator::default();
    feed(
        &mut agg,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"推理"}}"#,
    );
    feed(
        &mut agg,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
    );
    let resp = agg.finish().expect("聚合应成功");
    match &resp.content[0] {
        AnthropicResponseBlock::Thinking { thinking, .. } => {
            assert_eq!(thinking.as_deref(), Some("推理"))
        }
        other => panic!("应为 Thinking: {other:?}"),
    }
}

#[test]
fn sse_empty_stream_errors() {
    let agg = SseAggregator::default();
    assert!(agg.finish().is_err(), "空流应报错(端点空响应铁证形态)");
}
// -- ISSUE-IRONCLAW-007: config-level max_tokens fallback --

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

#[test]
fn provider_uses_config_max_tokens_as_fallback() {
    let provider =
        AnthropicOAuthProvider::new(&oauth_test_config(Some(65536)), 60).expect("provider builds");
    assert_eq!(provider.default_max_tokens, 65536);
}

#[test]
fn provider_falls_back_to_builtin_default_max_tokens() {
    let provider =
        AnthropicOAuthProvider::new(&oauth_test_config(None), 60).expect("provider builds");
    assert_eq!(provider.default_max_tokens, DEFAULT_MAX_TOKENS);
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
    let known = std::collections::HashMap::from([("llm_subject".to_string(), "known".to_string())]);
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
