use super::*;
use crate::{ChatMessage, CompletionRequest, LlmProvider, ToolCompletionRequest};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

struct TraceEnvironment {
    root: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}
impl TraceEnvironment {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("TIANQUAN_TRACE_DIR");
        // This exact test is run alone, before starting runtime/server threads.
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", root.path()) };
        Self { root, previous }
    }
}
impl Drop for TraceEnvironment {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("TIANQUAN_TRACE_DIR", value),
                None => std::env::remove_var("TIANQUAN_TRACE_DIR"),
            }
        }
    }
}

// A bounded local vendor seam. Capture bytes received over HTTP, not the
// request-builder input, so serialization, stream flag and fallback all run.
fn two_request_server() -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut received = Vec::new();
        while received.len() < 2 {
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(value) => break value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "local provider request did not arrive"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("local listener failed: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut raw = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).unwrap();
                assert!(count > 0);
                raw.extend_from_slice(&chunk[..count]);
                assert!(raw.len() < MAX_OBSERVED_BODY_BYTES);
                if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&raw[..header_end]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse().unwrap())
                })
                .unwrap();
            while raw.len() < header_end + length {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).unwrap();
                assert!(count > 0);
                raw.extend_from_slice(&chunk[..count]);
            }
            let body = raw[header_end..header_end + length].to_vec();
            let value: Value = serde_json::from_slice(&body).unwrap();
            let response = if value["stream"] == true {
                concat!(
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                )
            } else {
                r#"{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#
            };
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", if value["stream"] == true {"text/event-stream"} else {"application/json"}, response.len(), response).unwrap();
            received.push(body);
        }
        received
    });
    (address, handle)
}

#[test]
fn provider_wire_trace_matches_http_and_keeps_credentials_out() {
    let env = TraceEnvironment::new();
    let (address, server) = two_request_server();
    let run = uuid::Uuid::new_v4().to_string();
    let exchanges = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    let mut config = crate::config::RegistryProviderConfig::generic(
        crate::registry::ProviderProtocol::Anthropic,
        "wire-fixture",
        None,
        address,
        "fixture-model",
    )
    .with_max_tokens(Some(1234));
    config.oauth_token = Some(secrecy::SecretString::from(
        "fixture-bearer-must-not-enter-cas",
    ));
    let provider = crate::anthropic_oauth::AnthropicOAuthProvider::new(&config, 5).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (index, exchange) in exchanges.iter().enumerate() {
            let mut request = CompletionRequest::new(vec![
                ChatMessage::system(format!("SOUL-BUNDLE novelist roleSha256={}", "abcdef0123456789".repeat(4))),
                ChatMessage::user(json!({"artifactRef":{"contentHash":"0123456789abcdef".repeat(4)},"sourceUnits":[{"id":"author-0","quote":"保持周宁所知边界，不把猜测写成事实。"}]}).to_string()),
            ]);
            request.metadata.insert("run_id".into(), run.clone());
            request.metadata.insert("ironclaw_trace_exchange_id".into(), exchange.clone());
            request.metadata.insert("private_metadata".into(), "must-not-enter-wire".into());
            if index == 0 { provider.complete(request).await.unwrap(); }
            else { provider.complete_with_tools(ToolCompletionRequest::from_completion_request(request, vec![])).await.unwrap(); }
        }
    });
    let received = server.join().unwrap();
    drop(runtime);
    let events: Vec<Value> = std::fs::read_to_string(env.root.path().join(format!("{run}.jsonl")))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "provider_wire_request")
        .collect();
    let responses: Vec<_> = events
        .iter()
        .filter(|event| event["kind"] == "provider_wire_response")
        .collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(responses.len(), 2);
    for (index, (event, wire)) in requests.iter().zip(&received).enumerate() {
        let data = &event["data"];
        let parsed: Value = serde_json::from_slice(wire).unwrap();
        assert_eq!(
            data["payload_status"], "stored",
            "ordinary hashes and Chinese evidence must not be suppressed"
        );
        assert_eq!(data["wire_sha256"], sha256_hex(wire));
        assert_eq!(data["wire_bytes"], wire.len());
        let saved = run_trace::load_message_content(data["body_ref"].as_str().unwrap()).unwrap();
        assert_eq!(saved, parsed);
        assert_eq!(saved["max_tokens"], 1234);
        assert_eq!(
            saved["stream"],
            if index == 1 { json!(true) } else { Value::Null }
        );
        assert_eq!(data["exchange_id"], exchanges[index]);
        assert_eq!(
            responses[index]["data"]["physical_request_id"],
            data["physical_request_id"]
        );
        assert_eq!(responses[index]["data"]["http_status"], 200);
        let saved_text = saved.to_string();
        for forbidden in [
            "fixture-bearer-must-not-enter-cas",
            "ironclaw_trace_exchange_id",
            "must-not-enter-wire",
            "authorization",
        ] {
            assert!(!saved_text.contains(forbidden));
        }
    }
    // The observer itself only selects body fields. A suspicious message is
    // withheld as a gap, never rewritten and presented as an exact request.
    let trace = ProviderWireTrace {
        run_id: run.clone(),
        turn_id: None,
        exchange_id: exchanges[0].clone(),
    };
    trace.observe_anthropic(&serde_json::to_vec(&json!({"model":"safe", "Authorization":"private-header", "metadata":{"private":"private-value"}})).unwrap());
    let suspect = format!("ghp_{}", "a1B2c3D4".repeat(5));
    trace.observe_anthropic(
        &serde_json::to_vec(
            &json!({"model":"safe", "messages":[{"role":"user","content":suspect}]}),
        )
        .unwrap(),
    );
    let lines = std::fs::read_to_string(env.root.path().join(format!("{run}.jsonl"))).unwrap();
    let last: Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
    assert_eq!(last["data"]["payload_status"], "withheld_sensitive_content");
    assert!(last["data"].get("body_ref").is_none());
    assert!(!lines.contains("private-header"));
    assert!(!lines.contains("private-value"));
}
