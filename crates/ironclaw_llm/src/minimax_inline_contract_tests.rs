//! Public loopback fixtures through the actual registry OpenAI-compatible factory.
use super::*;
use crate::provider::{
    ChatMessage, CompletionRequest, CompletionStreamSink, LlmProvider, ToolCompletionRequest,
    ToolDefinition,
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn fixture_provider(
    model: &str,
    replies: Vec<serde_json::Value>,
) -> (
    Arc<dyn LlmProvider>,
    tokio::task::JoinHandle<Vec<serde_json::Value>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture loopback");
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .expect("original fixture accept deadline")
                    .expect("fixture accept");
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let count = stream
                    .read(&mut chunk)
                    .await
                    .expect("original fixture read");
                assert!(count != 0 && bytes.len() + count <= 1024 * 1024);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let sizes: Vec<_> = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                .collect();
            assert_eq!(sizes.len(), 1);
            let size = sizes[0];
            assert!(size <= 1024 * 1024);
            while bytes.len() - header_end < size {
                let mut chunk = [0u8; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count != 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            assert_eq!(bytes.len() - header_end, size);
            requests.push(serde_json::from_slice(&bytes[header_end..]).unwrap());
            let body = serde_json::to_vec(&reply).unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.shutdown().await.unwrap();
        }
        requests
    });
    let config = RegistryProviderConfig::generic(
        ProviderProtocol::OpenAiCompletions,
        "openai",
        Some(secrecy::SecretString::from(
            "PUBLIC-FIXTURE-KEY".to_string(),
        )),
        format!("http://{address}/v1"),
        model,
    );
    let provider =
        create_registry_provider_inner(&config, 3).expect("actual normal registry factory");
    (provider, server)
}
fn reply(content: &str, calls: serde_json::Value) -> serde_json::Value {
    let has_tools = calls.as_array().is_some_and(|calls| !calls.is_empty());
    serde_json::json!({"id":"public-fixture", "object":"chat.completion", "created":0, "model":"MiniMax-M3",
        "choices":[{"index":0,"message":{"role":"assistant","content":content,"tool_calls":calls},
            "finish_reason":if has_tools {"tool_calls"} else {"stop"}}],
        "usage":{"prompt_tokens":2,"completion_tokens":2,"total_tokens":4}})
}
fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "echo".into(),
        description: "public fixture echo".into(),
        parameters: serde_json::json!({"type":"object","properties":{}}),
    }
}
#[tokio::test]
async fn minimax_native_inline_final_and_tool_history_round_trip_through_factory() {
    let raw = " \r\n<think>public private-channel fixture\r\n</think>前言";
    let calls = serde_json::json!([{"id":"provider-Original-ID","type":"function","function":{"name":"echo","arguments":"{}"}}]);
    let (provider, server) = fixture_provider(
        "MiniMax-M3",
        vec![
            reply(raw, calls),
            reply(
                "<think>next</think>正文<think>故事字面</think><suggestions>小说元素</suggestions>",
                serde_json::json!([]),
            ),
        ],
    )
    .await;
    let first = provider
        .complete_with_tools(ToolCompletionRequest::new(
            vec![ChatMessage::user("fixture")],
            vec![tool()],
        ))
        .await;
    let first = first.expect("first original provider closed");
    let history = vec![
        ChatMessage::user("fixture"),
        ChatMessage::assistant_with_tool_calls(first.content.clone(), first.tool_calls.clone())
            .with_reasoning_details(first.reasoning_details.clone()),
        ChatMessage::tool_result("provider-Original-ID", "echo", "public result"),
    ];
    let second = provider
        .complete_with_tools(ToolCompletionRequest::new(history, vec![tool()]))
        .await;
    let requests = server.await.expect("original fixture server joined");
    let second = second.expect("second original provider closed");
    // Assertions follow both closed requests and original fixture join, including on RED.
    assert_eq!(first.content.as_deref(), Some("前言"));
    assert_eq!(
        first.reasoning.as_deref(),
        Some(" \r\n<think>public private-channel fixture\r\n</think>")
    );
    let assistant = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(
        assistant["content"],
        serde_json::json!([{ "type":"text", "text":raw }])
    );
    assert_eq!(assistant["tool_calls"][0]["id"], "provider-Original-ID");
    assert_eq!(
        second.content.as_deref(),
        Some("正文<think>故事字面</think><suggestions>小说元素</suggestions>")
    );
    assert_eq!(first.input_tokens, 2);
    assert_eq!(second.output_tokens, 2);
}
#[tokio::test]
async fn minimax_text_completion_is_normalized_but_other_models_are_byte_preserved() {
    let raw = "<think>public fixture</think>正文\r\n含<think>字面</think>";
    for (model, expected) in [
        ("MiniMax-M3", "正文\r\n含<think>字面</think>"),
        ("other-openai-model", raw),
    ] {
        let (provider, server) =
            fixture_provider(model, vec![reply(raw, serde_json::json!([]))]).await;
        let result = provider
            .complete(CompletionRequest::new(vec![ChatMessage::user("fixture")]))
            .await;
        server.await.expect("original fixture join");
        let result = result.unwrap();
        assert_eq!(result.content, expected);
        assert_eq!(result.reasoning.is_some(), model == "MiniMax-M3");
    }
}
#[derive(Default)]
struct CaptureSink(Mutex<String>);
#[async_trait::async_trait]
impl CompletionStreamSink for CaptureSink {
    async fn text_delta(&self, delta: String) {
        self.0.lock().unwrap().push_str(&delta);
    }
}
#[tokio::test]
async fn minimax_existing_buffered_streaming_facade_never_emits_inline_thinking() {
    let (provider, server) = fixture_provider(
        "MiniMax-M3",
        vec![reply(
            "<think>private fixture</think>final",
            serde_json::json!([]),
        )],
    )
    .await;
    let sink = Arc::new(CaptureSink::default());
    let result = provider
        .complete_with_tools_streaming(
            ToolCompletionRequest::new(vec![ChatMessage::user("fixture")], vec![tool()]),
            sink.clone(),
        )
        .await;
    server.await.expect("original fixture join");
    let result = result.unwrap();
    assert_eq!(result.content.as_deref(), Some("final"));
    // Existing RigAdapter deliberately delegates this API to non-streaming. No
    // upstream streaming qualification is inferred from the empty interim sink.
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn minimax_no_tools_whitespace_or_typed_only_is_never_a_successful_final() {
    for response in [
        reply(
            "<think>public fixture only</think>\r\n \t",
            serde_json::json!([]),
        ),
        reply(" \r\n\t", serde_json::json!([])),
        serde_json::json!({"id":"typed-only-public-fixture", "object":"chat.completion", "created":0, "model":"MiniMax-M3",
            "choices":[{"index":0,"message":{"role":"assistant","content":null,"reasoning_content":"public typed-only fixture"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":2,"total_tokens":4}}),
    ] {
        let (provider, server) = fixture_provider("MiniMax-M3", vec![response]).await;
        let result = provider
            .complete_with_tools(ToolCompletionRequest::new(
                vec![ChatMessage::user("fixture")],
                vec![tool()],
            ))
            .await;
        server
            .await
            .expect("original no-tools fixture server joined");
        assert!(
            result.is_err(),
            "reasoning/whitespace must not become final content"
        );
    }
}
