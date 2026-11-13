//! Normal provider caller tests; vendor boundary is a real local HTTP socket.
use super::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct LocalModel {
    origin: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<HashMap<String, String>>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LocalModel {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::clone(&stop);
        let captured = Arc::clone(&requests);
        let worker = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                            .unwrap();
                        let mut raw = Vec::new();
                        let mut chunk = [0; 2048];
                        let parsed = loop {
                            let count = socket.read(&mut chunk).unwrap();
                            if count == 0 {
                                // HTTP clients may abandon an unused pooled connection.
                                assert!(raw.is_empty(), "incomplete fixture HTTP request");
                                break None;
                            }
                            raw.extend_from_slice(&chunk[..count]);
                            assert!(raw.len() <= 65536);
                            if let Some(end) = raw.windows(4).position(|p| p == b"\r\n\r\n") {
                                let text = std::str::from_utf8(&raw[..end]).unwrap();
                                let headers: HashMap<String, String> = text
                                    .lines()
                                    .skip(1)
                                    .filter_map(|line| {
                                        line.split_once(':').map(|(name, value)| {
                                            (name.to_ascii_lowercase(), value.trim().to_owned())
                                        })
                                    })
                                    .collect();
                                let length = headers
                                    .get("content-length")
                                    .unwrap()
                                    .parse::<usize>()
                                    .unwrap();
                                if raw.len() >= end + 4 + length {
                                    let body: serde_json::Value =
                                        serde_json::from_slice(&raw[end + 4..end + 4 + length])
                                            .unwrap();
                                    break Some((headers, body));
                                }
                            }
                        };
                        let Some((headers, body)) = parsed else {
                            continue;
                        };
                        captured.lock().unwrap().push(headers);
                        let streaming = body.get("stream") == Some(&serde_json::Value::Bool(true));
                        let (content_type, response) = if streaming {
                            (
                                "text/event-stream",
                                concat!(
                                    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"fixture\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"fixture-model\",\"usage\":{\"input_tokens\":2,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
                                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"local-ok\"}}\n\n",
                                    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                                    "data: {\"type\":\"message_stop\"}\n\n"
                                ),
                            )
                        } else {
                            (
                                "application/json",
                                r#"{"id":"fixture","type":"message","role":"assistant","model":"fixture-model","content":[{"type":"text","text":"local-ok"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
                            )
                        };
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
                        socket.write_all(response.as_bytes()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => panic!("local fixture accept: {error}"),
                }
            }
        });
        Self {
            origin,
            stop,
            requests,
            worker: Some(worker),
        }
    }

    fn config(&self) -> RegistryProviderConfig {
        let mut config = RegistryProviderConfig::generic(
            crate::registry::ProviderProtocol::Anthropic,
            "local-fixture",
            None,
            self.origin.clone(),
            "fixture-model",
        );
        config.oauth_token = Some(SecretString::from("fixture-bearer"));
        config.extra_headers = vec![
            ("User-Agent".into(), "IronClaw-Local-Test/1".into()),
            ("x-conversation-id".into(), "{metadata:thread_id}".into()),
        ];
        config
    }

    fn provider(&self) -> AnthropicOAuthProvider {
        let mut provider = AnthropicOAuthProvider::new(&self.config(), 5).unwrap();
        provider.client = crate::config::hardened_client_builder(5)
            .no_proxy()
            .build()
            .unwrap();
        provider.cli_refresh_loader = Some(|| panic!("FixtureCredentialLoaderMustNotRun"));
        provider
    }

    fn captured(&self) -> Vec<HashMap<String, String>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for LocalModel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                if std::thread::panicking() {
                    eprintln!("fixture worker failed while the test was already unwinding");
                } else {
                    panic!("fixture worker failed");
                }
            }
        }
    }
}

fn request(thread: &str, run: &str) -> CompletionRequest {
    let mut request =
        CompletionRequest::new(vec![ChatMessage::user("fixture prompt")]).with_max_tokens(8);
    request.metadata.insert("thread_id".into(), thread.into());
    request.metadata.insert("run_id".into(), run.into());
    request
}

// CH-HTTP-01: real complete entry point, same thread across runs, different thread isolated.
#[tokio::test(flavor = "current_thread")]
async fn configured_metadata_headers_plain_keep_thread_across_runs() {
    let fixture = LocalModel::start();
    let provider = fixture.provider();
    for (thread, run) in [
        ("thread-a", "run-1"),
        ("thread-a", "run-2"),
        ("thread-b", "run-3"),
    ] {
        provider.complete(request(thread, run)).await.unwrap();
    }
    let captured = fixture.captured();
    assert_eq!(captured.len(), 3);
    for (actual, expected) in captured.iter().zip(["thread-a", "thread-a", "thread-b"]) {
        assert_eq!(
            actual.get("x-conversation-id").map(String::as_str),
            Some(expected)
        );
        assert_eq!(
            actual.get("user-agent").map(String::as_str),
            Some("IronClaw-Local-Test/1")
        );
        assert_eq!(
            actual.get("authorization").map(String::as_str),
            Some("Bearer fixture-bearer")
        );
    }
}

// CH-HTTP-02: tool requests use the real SSE transport, not a helper-only assertion.
#[tokio::test(flavor = "current_thread")]
async fn configured_metadata_headers_tools_stream_keep_thread() {
    let fixture = LocalModel::start();
    fixture
        .provider()
        .complete_with_tools(ToolCompletionRequest::from_completion_request(
            request("thread-tools", "run-tools"),
            vec![],
        ))
        .await
        .unwrap();
    let captured = fixture.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get("x-conversation-id").map(String::as_str),
        Some("thread-tools")
    );
    assert_eq!(
        captured[0].get("accept").map(String::as_str),
        Some("text/event-stream")
    );
}

// CH-HTTP-01: concurrent requests share one provider but never its per-request identity.
#[tokio::test(flavor = "current_thread")]
async fn configured_metadata_headers_concurrent_requests_are_isolated() {
    let fixture = LocalModel::start();
    let provider = fixture.provider();
    let (a, b) = tokio::join!(
        provider.complete(request("thread-a", "run-a")),
        provider.complete(request("thread-b", "run-b"))
    );
    a.unwrap();
    b.unwrap();
    let mut threads: Vec<_> = fixture
        .captured()
        .iter()
        .map(|h| h.get("x-conversation-id").cloned())
        .collect();
    threads.sort();
    assert_eq!(
        threads,
        vec![Some("thread-a".into()), Some("thread-b".into())]
    );
}

// CH-DENY-01: missing/invalid metadata must fail before a socket is dispatched.
#[tokio::test(flavor = "current_thread")]
async fn configured_metadata_headers_missing_or_invalid_metadata_dispatches_nothing() {
    let fixture = LocalModel::start();
    let provider = fixture.provider();
    for value in [None, Some(""), Some("fixture-private\r\nx-extra: private")] {
        let mut input = request("unused", "run");
        input.metadata.remove("thread_id");
        if let Some(value) = value {
            input.metadata.insert("thread_id".into(), value.into());
        }
        let error = provider
            .complete(input)
            .await
            .expect_err("invalid metadata must not dispatch");
        assert!(!format!("{error:?}").contains("fixture-private"));
        assert!(!crate::retry::is_retryable(&error));
    }
    assert!(fixture.captured().is_empty());
}

// CH-AUTH-01/CH-DENY-01: extra headers cannot replace the subject credential or leak bad values.
#[test]
fn configured_metadata_headers_reject_credentials_and_malformed_templates() {
    let fixture = LocalModel::start();
    for (name, value) in [
        ("Authorization", "fixture-private"),
        ("x-api-key", "fixture-private"),
        ("Host", "fixture-private"),
        ("Cookie", "fixture-private"),
        ("x-conversation-id", "{metadata:thread_id"),
        ("x-conversation-id", "prefix-{metadata:thread_id}"),
        ("x-static", "fixture-private\r\nx-extra: private"),
    ] {
        let mut config = fixture.config();
        config.extra_headers = vec![(name.into(), value.into())];
        let error = AnthropicOAuthProvider::new(&config, 5)
            .err()
            .expect("invalid headers rejected");
        assert!(!format!("{error:?}").contains("fixture-private"));
        assert!(!crate::retry::is_retryable(&error));
    }
    assert!(fixture.captured().is_empty());
}
