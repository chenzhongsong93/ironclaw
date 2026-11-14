//! 原生产writer的跨线程/多run trace合同；无provider/HTTP/SQL。
use ironclaw_llm::{ChatMessage, CompletionRequest, ToolCall};
use serde_json::Value;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

struct TraceRoot {
    root: PathBuf,
    previous: Option<OsString>,
}
impl TraceRoot {
    fn new() -> Self {
        let root = match std::env::var_os("TIANQUAN_TRACE_TEST_ROOT") {
            Some(path) => {
                let path = PathBuf::from(path);
                assert!(path.is_absolute() && !path.exists());
                std::fs::create_dir(&path).unwrap();
                path
            }
            None => tempfile::Builder::new()
                .prefix("trace-base0-")
                .tempdir()
                .unwrap()
                .keep(),
        };
        let previous = std::env::var_os("TIANQUAN_TRACE_DIR");
        // 每个exact测试独立进程；创建worker前设置，所有worker已join后恢复。
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", &root) };
        Self { root, previous }
    }
}
impl Drop for TraceRoot {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("TIANQUAN_TRACE_DIR", value),
                None => std::env::remove_var("TIANQUAN_TRACE_DIR"),
            }
        }
        // 原测试trace与CAS证据保留，不删除目录。
    }
}

fn requests() -> (CompletionRequest, CompletionRequest) {
    let first = vec![
        ChatMessage::system("mechanical trace identity"),
        ChatMessage::user("mechanical input one"),
        ChatMessage::user("mechanical input two"),
        ChatMessage::user("mechanical input three"),
        ChatMessage::user("mechanical input four"),
    ];
    let mut after_tool = first.clone();
    after_tool.push(ChatMessage::assistant_with_tool_calls(
        None,
        vec![ToolCall {
            id: "WStage001".into(),
            name: "mechanical_tool".into(),
            arguments: serde_json::json!({"mechanical":true}),
            ..ToolCall::default()
        }],
    ));
    after_tool.push(ChatMessage::tool_result(
        "WStage001",
        "mechanical_tool",
        "mechanical result",
    ));
    (
        CompletionRequest::new(first)
            .with_model("trace-fixture-model")
            .with_max_tokens(512),
        CompletionRequest::new(after_tool)
            .with_model("trace-fixture-model")
            .with_max_tokens(512),
    )
}

fn emit(run: &str, request: &CompletionRequest) {
    let scope = format!("run={run}\nturn=trace-fixture-turn\nmodel_call=1");
    super::trace_model_request_event(
        Some(&scope),
        "trace-fixture-provider",
        &request.messages,
        request.model.as_deref(),
        request.max_tokens,
        &[],
    );
}

fn events(root: &Path, run: &str) -> Vec<Value> {
    std::fs::read_to_string(root.join(format!("{run}.jsonl")))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["kind"] == "model_request")
        .collect()
}

fn actual_cas_messages(data: &Value) -> Vec<Value> {
    data["new_messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| match message.get("c").and_then(Value::as_str) {
            Some(hash) => ironclaw_common::run_trace::load_message_content(hash)
                .expect("actual emitted CAS reference must resolve"),
            None => message.clone(),
        })
        .collect()
}

fn assert_actual_contract(root: &Path, run: &str, expected: &[CompletionRequest]) {
    let recorded = events(root, run);
    assert_eq!(recorded.len(), expected.len());
    eprintln!(
        "trace_fixture_root={} run={} requests={}",
        root.display(),
        run,
        recorded.len()
    );
    // RED先检查实际被引用的同run事件能否满足声明，而非先断base0镜像实现。
    // 这里只对本例实际独立base做CAS加载/长度边界，不复制A dossier重建算法。
    for (index, event) in recorded.iter().enumerate() {
        let data = &event["data"];
        let seq = usize::try_from(data["messages_base_seq"].as_u64().unwrap()).unwrap();
        let count = usize::try_from(data["messages_base_count"].as_u64().unwrap()).unwrap();
        if seq > 0 {
            assert!(
                seq <= index,
                "trace prefix reference must be an earlier same-run event"
            );
            let actual_base = &recorded[seq - 1]["data"];
            assert_eq!(
                actual_base["messages_base_seq"], 0,
                "this deterministic case references an actual independent base"
            );
            let actual_base_messages = actual_cas_messages(actual_base);
            assert!(
                count <= actual_base_messages.len(),
                "trace prefix count must fit its actual same-run base"
            );
        }
    }
    // GREEN再核每一次实际CAS消息全文/顺序与typed输入等值；base0只是辅助合同。
    for (event, request) in recorded.iter().zip(expected) {
        let data = &event["data"];
        assert_eq!(
            actual_cas_messages(data),
            serde_json::to_value(&request.messages)
                .unwrap()
                .as_array()
                .unwrap()
                .clone()
        );
        assert_eq!(data["messages_base_seq"], 0);
        assert_eq!(data["messages_base_count"], 0);
        assert_eq!(event["run_id"], run);
        assert_eq!(data["provider_id"], "trace-fixture-provider");
        assert_eq!(data["model"], "trace-fixture-model");
        assert_eq!(data["max_tokens"], 512);
    }
}

#[test]
fn same_run_cross_thread_prefix_is_satisfiable() {
    let root = TraceRoot::new();
    let (first, after_tool) = requests();
    let first_thread = first.clone();
    std::thread::spawn(move || emit("trace-cross-thread-child", &first_thread))
        .join()
        .unwrap();
    let second_thread = after_tool.clone();
    std::thread::spawn(move || {
        emit("trace-cross-thread-child", &second_thread);
        emit("trace-cross-thread-child", &second_thread);
    })
    .join()
    .unwrap();
    assert_actual_contract(
        &root.root,
        "trace-cross-thread-child",
        &[first, after_tool.clone(), after_tool],
    );
}

#[test]
fn interleaved_runs_keep_actual_prefix_scope() {
    let root = TraceRoot::new();
    let (first, after_tool) = requests();
    let child_first = first.clone();
    let child_later = after_tool.clone();
    std::thread::spawn(move || {
        let parent = CompletionRequest::new(vec![ChatMessage::user("parent input")]);
        emit("trace-interleaved-parent", &parent);
        emit("trace-interleaved-child", &child_first);
        emit("trace-interleaved-child", &child_later);
        emit("trace-interleaved-parent", &parent);
        emit("trace-interleaved-child", &child_later);
    })
    .join()
    .unwrap();
    assert_actual_contract(
        &root.root,
        "trace-interleaved-child",
        &[first, after_tool.clone(), after_tool],
    );
}

#[tokio::test]
async fn provider_tool_definition_is_addressed_and_verifiable() {
    use ironclaw_host_api::{CapabilityId, ProviderToolName, Resolution, ResolutionBatch};
    use ironclaw_turns::run_profile::{
        AgentLoopHostError, CapabilityBatchInvocation, CapabilityInvocation, LoopCapabilityPort,
        ProviderToolDefinition, VisibleCapabilityRequest, VisibleCapabilitySurface,
    };

    struct Surface;
    #[async_trait::async_trait]
    impl LoopCapabilityPort for Surface {
        fn tool_definitions(&self) -> Result<Vec<ProviderToolDefinition>, AgentLoopHostError> {
            Ok(vec![ProviderToolDefinition {
                capability_id: CapabilityId::new("fixture.lookup").unwrap(),
                name: ProviderToolName::new("fixture_lookup").unwrap(),
                description: "public fixture".into(),
                parameters: serde_json::json!({"type":"object","properties":{"item":{"type":"string"}}}),
            }])
        }

        async fn visible_capabilities(
            &self,
            _: VisibleCapabilityRequest,
        ) -> Result<VisibleCapabilitySurface, AgentLoopHostError> {
            unreachable!("the provider request reads the resolved tool definitions")
        }

        async fn invoke_capability(
            &self,
            _: CapabilityInvocation,
        ) -> Result<Resolution, AgentLoopHostError> {
            unreachable!("the provider returns a final answer")
        }

        async fn invoke_capability_batch(
            &self,
            _: CapabilityBatchInvocation,
        ) -> Result<ResolutionBatch, AgentLoopHostError> {
            unreachable!("the provider returns a final answer")
        }
    }

    #[derive(Default)]
    struct Capture(std::sync::Mutex<Vec<ironclaw_llm::ToolDefinition>>);
    #[async_trait::async_trait]
    impl ironclaw_llm::LlmProvider for Capture {
        fn model_name(&self) -> &str {
            "fixture"
        }
        fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
            (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
        }
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
            unreachable!("expected a tool-capable provider request")
        }
        async fn complete_with_tools(
            &self,
            req: ironclaw_llm::ToolCompletionRequest,
        ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
            *self.0.lock().unwrap() = req.tools;
            Ok(ironclaw_llm::ToolCompletionResponse {
                content: Some("done".into()),
                tool_calls: Vec::new(),
                normalization_version: None,
                normalized_input_tokens: None,
                cache_details_observed: None,
                input_tokens: 0,
                output_tokens: 0,
                finish_reason: ironclaw_llm::FinishReason::Stop,
                reasoning: None,
                reasoning_details: None,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })
        }
    }

    let owned = TraceRoot::new();
    let provider = Capture::default();
    super::complete_model_request(
        &provider,
        CompletionRequest::new(vec![ChatMessage::user("fixture")]),
        Some(std::sync::Arc::new(Surface)),
        Some("run=trace-tools".into()),
        None,
        super::ProviderReplayIdentity::new("fixture", "fixture").unwrap(),
        None,
    )
    .await
    .unwrap();
    let tools = provider.0.lock().unwrap();
    assert_eq!(tools.len(), 1);
    let expected = serde_json::to_value(&tools[0]).unwrap();
    let rows = events(&owned.root, "trace-tools");
    assert_eq!(rows.len(), 1);
    let definition = &rows[0]["data"]["tool_definitions"][0];
    let reference = definition["definition_ref"].as_str().unwrap();
    let stored =
        std::fs::read(owned.root.join("content").join(format!("{reference}.json"))).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&stored).unwrap(), expected);
    assert_eq!(
        definition["definition_sha256"],
        ironclaw_common::hashing::sha256_hex(&stored)
    );
    assert_eq!(
        definition["schema_sha256"],
        ironclaw_common::hashing::sha256_hex(expected["parameters"].to_string().as_bytes())
    );
}
