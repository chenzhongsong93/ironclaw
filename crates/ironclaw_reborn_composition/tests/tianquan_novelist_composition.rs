use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ironclaw_host_api::runtime_policy::{
    ApprovalPolicy, AuditMode, DeploymentMode, EffectiveRuntimePolicy, FilesystemBackendKind,
    NetworkMode, ProcessBackendKind, RuntimeProfile, SecretMode,
};
use ironclaw_loop_host::{
    HostManagedModelError, HostManagedModelErrorKind, HostManagedModelGateway,
    HostManagedModelMessageRole, HostManagedModelRequest, HostManagedModelResponse,
};
use ironclaw_reborn_composition::{
    RebornBuildInput, RebornRuntimeIdentity, RebornRuntimeInput, TurnStatus, build_reborn_runtime,
};
use ironclaw_turns::run_profile::{
    LoopCapabilityPort, ProviderToolCall, RegisterProviderToolCallRequest,
};

#[derive(Default)]
struct TianQuanSpawnCaptureGateway {
    calls: AtomicUsize,
    requests: Mutex<Vec<HostManagedModelRequest>>,
}

fn local_dev_test_policy() -> EffectiveRuntimePolicy {
    EffectiveRuntimePolicy {
        deployment: DeploymentMode::LocalSingleUser,
        requested_profile: RuntimeProfile::LocalDev,
        resolved_profile: RuntimeProfile::LocalDev,
        filesystem_backend: FilesystemBackendKind::HostWorkspace,
        process_backend: ProcessBackendKind::LocalHost,
        network_mode: NetworkMode::DirectLogged,
        secret_mode: SecretMode::ScrubbedEnv,
        approval_policy: ApprovalPolicy::AskDestructive,
        audit_mode: AuditMode::LocalMinimal,
    }
}

#[async_trait]
impl HostManagedModelGateway for TianQuanSpawnCaptureGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "TianQuan spawn verification requires the capability-aware host path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("request capture lock")
            .push(request);

        match call {
            0 => {
                let spawn = capabilities
                    .tool_definitions()
                    .expect("spawn tool definitions")
                    .into_iter()
                    .find(|tool| tool.capability_id.as_str() == "builtin.spawn_subagent")
                    .expect("the composition must expose the TianQuan subagent dispatcher");
                let provider_call = ProviderToolCall::from_parts(
                    "tianquan-test-provider",
                    "test-parent-model",
                    Some("tianquan-parent-turn".into()),
                    "call-spawn-novelist",
                    spawn.name.to_string(),
                    serde_json::json!({
                        "subagent_type": "novelist",
                        "task": "Write a short scene from the locked chapter context.",
                        "handoff": "Return only the authored scene.",
                    }),
                )
                .expect("valid provider tool call");
                let candidate = capabilities
                    .register_provider_tool_call(RegisterProviderToolCallRequest::new(
                        provider_call,
                    ))
                    .await
                    .expect("register novelist spawn call");
                Ok(HostManagedModelResponse::capability_calls(
                    vec![candidate],
                    "delegate to the novelist",
                ))
            }
            1 => Ok(HostManagedModelResponse::assistant_reply(
                "A scene written under the resolved TianQuan role.",
            )),
            _ => Ok(HostManagedModelResponse::assistant_reply(
                "The novelist completed the assigned scene.",
            )),
        }
    }
}

#[tokio::test]
#[ignore = "requires TIANQUAN_SOUL_BUNDLE_DIR pointing to the sibling TianQuan worktree"]
async fn production_composition_sends_pinned_novelist_soul_in_final_system_request() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(TianQuanSpawnCaptureGateway::default());
    let input = RebornRuntimeInput::from_services(
        RebornBuildInput::local_dev("novelist-composition-owner", root.path().join("local-dev"))
            .with_runtime_policy(local_dev_test_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "novelist-composition-tenant".into(),
        agent_id: "novelist-composition-agent".into(),
        source_binding_id: "novelist-composition-source".into(),
        reply_target_binding_id: "novelist-composition-reply".into(),
    })
    .with_poll_settings(ironclaw_reborn_composition::PollSettings {
        interval: std::time::Duration::from_millis(10),
        max_total: std::time::Duration::from_secs(20),
    })
    .with_model_gateway_override(gateway.clone());

    let runtime = build_reborn_runtime(input)
        .await
        .expect("TianQuan production composition builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        runtime.send_user_message(&conversation, "delegate a scene to the novelist"),
    )
    .await
    .expect("novelist composition run finishes")
    .expect("novelist composition run succeeds");
    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");

    let requests = {
        let mut captured = gateway.requests.lock().expect("request capture lock");
        std::mem::take(&mut *captured)
    };
    assert!(
        requests.len() >= 3,
        "parent, novelist child, and parent follow-up requests"
    );
    let novelist_request = &requests[1];
    let soul_system = novelist_request
        .messages
        .iter()
        .filter(|message| message.role == HostManagedModelMessageRole::System)
        .map(|message| message.content.as_str())
        .find(|content| content.contains("SOUL-BUNDLE novel-studio-souls/1.0.0 kind=novelist"))
        .expect(
            "the final child provider request must contain the pinned novelist SOUL system message",
        );
    assert!(soul_system.contains("roleSha256="));
    assert!(soul_system.contains("V17 L8 小说家 Agent"));
    assert!(
        novelist_request.messages.iter().any(|message| {
            message.role == HostManagedModelMessageRole::User
                && message
                    .content
                    .contains("Write a short scene from the locked chapter context.")
        }),
        "the child goal remains a user message, separate from the trusted SOUL system message"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}
