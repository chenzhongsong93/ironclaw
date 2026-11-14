//! 正常 Production factory 到 runner decorators/profile/prompt/provider 的原生surface回归。
#![cfg(all(feature = "libsql", feature = "test-support"))]

use async_trait::async_trait;
use ironclaw_host_api::runtime_policy::{
    ApprovalPolicy, AuditMode, DeploymentMode, EffectiveRuntimePolicy, FilesystemBackendKind,
    NetworkMode, ProcessBackendKind, RuntimeProfile, SecretMode,
};
use ironclaw_host_runtime::{
    CommandExecutionOutput, CommandExecutionRequest, RuntimeProcessError, SandboxCommandTransport,
    TenantSandboxProcessPort,
};
use ironclaw_llm::{
    CompletionRequest, CompletionResponse, FinishReason, LlmError, LlmProvider,
    ToolCompletionRequest, ToolCompletionResponse,
};
use ironclaw_reborn_composition::{
    RebornBuildInput, RebornCompositionProfile, RebornProductionExecutionSurface,
    RebornProductionExecutionSurfaceFactory, RebornProductionSurfaceContext, RebornRuntimeError,
    RebornRuntimeIdentity, RebornRuntimeInput, RebornRuntimeProcessBinding, build_reborn_runtime,
    builtin_first_party_trust_policy,
};
use ironclaw_runner::model_gateway::{LlmModelProfilePolicy, LlmProviderModelGateway};
use ironclaw_turns::run_profile::ModelProfileId;
use rust_decimal::Decimal;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

struct NormalSurfaceFactory {
    identity_root: PathBuf,
}
#[async_trait]
impl RebornProductionExecutionSurfaceFactory for NormalSurfaceFactory {
    async fn assemble(
        &self,
        context: RebornProductionSurfaceContext,
    ) -> Result<RebornProductionExecutionSurface, RebornRuntimeError> {
        let sources = context.normal_context_sources(
            self.identity_root.clone(),
            self.identity_root.join("AGENTS.md"),
        )?;
        let resolver = context.normal_capability_surface_resolver();
        Ok(context
            .bind_normal_runtime_capability_factory(Arc::new(
                ironclaw_turns::InMemoryExternalToolCatalog::default(),
            ))
            .into_execution_surface(resolver, sources))
    }
}
struct NoProcessDispatch;
#[async_trait]
impl SandboxCommandTransport for NoProcessDispatch {
    async fn run_command(
        &self,
        _: CommandExecutionRequest,
    ) -> Result<CommandExecutionOutput, RuntimeProcessError> {
        panic!("surface test must not dispatch a process")
    }
}
#[derive(Default)]
struct ProviderCapture {
    requests: Mutex<Vec<serde_json::Value>>,
    spawn_once: std::sync::atomic::AtomicBool,
}
impl ProviderCapture {
    fn record(&self, messages: serde_json::Value, names: Vec<String>) {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::json!({"messages":messages,"tool_names":names}));
    }
}
#[async_trait]
impl LlmProvider for ProviderCapture {
    fn model_name(&self) -> &str {
        "normal-native-surface-fixture"
    }
    fn cost_per_token(&self) -> (Decimal, Decimal) {
        (Decimal::ZERO, Decimal::ZERO)
    }
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.record(serde_json::to_value(request.messages).unwrap(), vec![]);
        Ok(CompletionResponse {
            normalization_version: None,
            normalized_input_tokens: None,
            cache_details_observed: None,
            content: "Surface observed.".into(),
            input_tokens: 1,
            output_tokens: 1,
            finish_reason: FinishReason::Stop,
            reasoning: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }
    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let names = request.tools.iter().map(|tool| tool.name.clone()).collect();
        self.record(serde_json::to_value(request.messages).unwrap(), names);
        if self
            .spawn_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(ToolCompletionResponse {
                normalization_version: None,
                normalized_input_tokens: None,
                cache_details_observed: None,
                content: Some("Delegating one fixture child.".into()),
                tool_calls: vec![ironclaw_llm::ToolCall {
                    id: "normal-spawn-call".into(),
                    name: "builtin__spawn_subagent".into(),
                    arguments: serde_json::json!({"subagent_type":"worldsmith","task":"Reply briefly with child fixture done."}),
                    reasoning: None,
                    signature: None,
                    arguments_parse_error: None,
                }],
                input_tokens: 1,
                output_tokens: 1,
                finish_reason: FinishReason::ToolUse,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                reasoning: None,
                reasoning_details: None,
            });
        }
        Ok(ToolCompletionResponse {
            normalization_version: None,
            normalized_input_tokens: None,
            cache_details_observed: None,
            content: Some("Surface observed.".into()),
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 1,
            finish_reason: FinishReason::Stop,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning: None,
            reasoning_details: None,
        })
    }
}

// TQH-L1-10：真实normal装配的nativeports不能在profile ceiling中丢失。
#[tokio::test]
async fn production_normal_native_ports_reach_provider_and_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let identity = dir.path().join("identity");
    std::fs::create_dir(&identity).unwrap();
    std::fs::write(
        identity.join("AGENTS.md"),
        "You are a fixture assistant. Reply briefly.",
    )
    .unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("normal.db"))
            .build()
            .await
            .unwrap(),
    );
    let provider = Arc::new(ProviderCapture::default());
    let gateway = Arc::new(LlmProviderModelGateway::with_provider_identity(
        "native-surface-fixture",
        provider.clone(),
        LlmModelProfilePolicy::new()
            .allow_model_profile(ModelProfileId::new("interactive_model").unwrap(), None),
    ));
    let input = RebornRuntimeInput::from_services(
        RebornBuildInput::libsql(
            RebornCompositionProfile::Production,
            "native-surface-owner",
            db,
            dir.path().join("events.db").to_string_lossy(),
            None,
            ironclaw_secrets::SecretMaterial::from("01234567890123456789012345678901"),
        )
        .with_production_trust_policy(Arc::new(builtin_first_party_trust_policy().unwrap()))
        .with_runtime_policy(EffectiveRuntimePolicy {
            deployment: DeploymentMode::HostedMultiTenant,
            requested_profile: RuntimeProfile::SecureDefault,
            resolved_profile: RuntimeProfile::SecureDefault,
            filesystem_backend: FilesystemBackendKind::ScopedVirtual,
            process_backend: ProcessBackendKind::TenantSandbox,
            network_mode: NetworkMode::Deny,
            secret_mode: SecretMode::BrokeredHandles,
            approval_policy: ApprovalPolicy::AskAlways,
            audit_mode: AuditMode::Standard,
        })
        .with_runtime_process_binding(RebornRuntimeProcessBinding::tenant_sandbox(Arc::new(
            TenantSandboxProcessPort::new(Arc::new(NoProcessDispatch)),
        ))),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "native-surface-tenant".into(),
        agent_id: "native-surface-agent".into(),
        source_binding_id: "native-surface-source".into(),
        reply_target_binding_id: "native-surface-reply".into(),
    })
    .with_model_gateway_override(gateway)
    .with_production_execution_surface_factory(Arc::new(NormalSurfaceFactory {
        identity_root: identity,
    }));
    let runtime = build_reborn_runtime(input)
        .await
        .expect("normal Production runtime");
    let conversation = runtime.new_conversation().await.unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        runtime.send_user_message(
            &conversation,
            "Observe your visible surface and reply briefly.",
        ),
    )
    .await;
    // Close the original runtime before asserting the missing-surface RED.
    runtime.shutdown().await.expect("original runtime shutdown");
    outcome
        .expect("bounded normal caller")
        .expect("normal caller reply");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "fixture makes exactly one provider call");
    let names = requests[0]["tool_names"].as_array().unwrap();
    for expected in ["builtin__spawn_subagent", "builtin__skill_activate"] {
        assert!(
            names.iter().any(|name| name == expected),
            "normal installed native port missing from provider tool definitions: {expected}"
        );
    }
    let messages = requests[0]["messages"].as_array().unwrap();
    let capability_sections = messages
        .iter()
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| {
            content
                .split_once("Capabilities:")
                .map(|(_, section)| section)
        })
        .collect::<Vec<_>>();
    for expected in [
        "- id: builtin.spawn_subagent",
        "- id: builtin.skill_activate",
    ] {
        assert!(
            capability_sections
                .iter()
                .any(|section| section.lines().any(|line| line.trim() == expected)),
            "normal installed native port missing from prompt surface: {expected}"
        );
    }
}

// TQH-L1-11：真实normal ToolUse执行基准；只有vendor response为fixture。
#[tokio::test]
async fn production_normal_spawn_dispatch_persists_child_reference_and_lineage() {
    let bundle_dir = std::env::var("TIANQUAN_SOUL_BUNDLE_DIR")
        .expect("formal SOUL bundle is required; configure the paired TianQuan asset directory");
    tianquan_subagents::directions::SoulBundle::load_from_dir(std::path::Path::new(&bundle_dir))
        .expect("formal SOUL catalog and every referenced file must pass the real loader");
    let dir = tempfile::tempdir().unwrap();
    let identity = dir.path().join("identity");
    std::fs::create_dir(&identity).unwrap();
    std::fs::write(
        identity.join("AGENTS.md"),
        "You are a fixture assistant. Reply briefly.",
    )
    .unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("normal.db"))
            .build()
            .await
            .unwrap(),
    );
    let provider = Arc::new(ProviderCapture::default());
    provider
        .spawn_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let gateway = Arc::new(LlmProviderModelGateway::with_provider_identity(
        "native-surface-fixture",
        provider.clone(),
        LlmModelProfilePolicy::new()
            .allow_model_profile(ModelProfileId::new("interactive_model").unwrap(), None),
    ));
    let input = RebornRuntimeInput::from_services(
        RebornBuildInput::libsql(
            RebornCompositionProfile::Production,
            "native-surface-owner",
            db,
            dir.path().join("events.db").to_string_lossy(),
            None,
            ironclaw_secrets::SecretMaterial::from("01234567890123456789012345678901"),
        )
        .with_production_trust_policy(Arc::new(builtin_first_party_trust_policy().unwrap()))
        .with_runtime_policy(EffectiveRuntimePolicy {
            deployment: DeploymentMode::HostedMultiTenant,
            requested_profile: RuntimeProfile::SecureDefault,
            resolved_profile: RuntimeProfile::SecureDefault,
            filesystem_backend: FilesystemBackendKind::ScopedVirtual,
            process_backend: ProcessBackendKind::TenantSandbox,
            network_mode: NetworkMode::Deny,
            secret_mode: SecretMode::BrokeredHandles,
            approval_policy: ApprovalPolicy::AskAlways,
            audit_mode: AuditMode::Standard,
        })
        .with_runtime_process_binding(RebornRuntimeProcessBinding::tenant_sandbox(Arc::new(
            TenantSandboxProcessPort::new(Arc::new(NoProcessDispatch)),
        ))),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "native-surface-tenant".into(),
        agent_id: "native-surface-agent".into(),
        source_binding_id: "native-surface-source".into(),
        reply_target_binding_id: "native-surface-reply".into(),
    })
    .with_model_gateway_override(gateway)
    .with_production_execution_surface_factory(Arc::new(NormalSurfaceFactory {
        identity_root: identity,
    }));
    let runtime = build_reborn_runtime(input)
        .await
        .expect("normal Production runtime");
    let conversation = runtime.new_conversation().await.unwrap();
    let thread_service = runtime.session_thread_service();
    let coordinator = runtime.webui_turn_coordinator_for_test();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        runtime.send_user_message(
            &conversation,
            "Delegate one registered worldsmith child and finish briefly.",
        ),
    )
    .await;
    runtime
        .shutdown()
        .await
        .expect("original runtime shutdown before assertions");
    let reply = outcome
        .expect("bounded original caller")
        .expect("original caller result");
    assert_eq!(
        reply.status,
        ironclaw_turns::TurnStatus::Completed,
        "normal ToolUse must dispatch and complete, category={:?}",
        reply.failure_category
    );
    let scope = ironclaw_threads::ThreadScope {
        tenant_id: ironclaw_host_api::TenantId::new("native-surface-tenant").unwrap(),
        agent_id: ironclaw_host_api::AgentId::new("native-surface-agent").unwrap(),
        project_id: None,
        owner_user_id: Some(ironclaw_host_api::UserId::new("native-surface-owner").unwrap()),
        mission_id: None,
    };
    let history = thread_service
        .list_thread_history(ironclaw_threads::ThreadHistoryRequest {
            scope: scope.clone(),
            thread_id: conversation.0.clone(),
        })
        .await
        .unwrap();
    let result = history
        .messages
        .iter()
        .find(|message| {
            message.kind == ironclaw_threads::MessageKind::ToolResultReference
                && message.status == ironclaw_threads::MessageStatus::Finalized
        })
        .expect("real finalized parent result reference");
    let reference = result
        .tool_result_ref
        .clone()
        .expect("parent durable result ref");
    let envelope: serde_json::Value =
        serde_json::from_str(result.content.as_deref().unwrap()).unwrap();
    assert_eq!(envelope["result_ref"], reference);
    // A completed parent may have recovered from a native invocation failure.
    // Such a provider-error reference has no durable blob by contract.
    eprintln!(
        "normal spawn fixture parent result metadata: {}",
        serde_json::json!({
            "parent_run_id":reply.run_id,"result_ref":reference,
            "safe_summary":envelope["safe_summary"],"model_observation":envelope["model_observation"],
        })
    );
    assert!(
        !reference.starts_with("result:provider-error-"),
        "native spawn returned a recoverable provider error instead of a durable child result: {}",
        envelope
    );
    let chunk = thread_service
        .read_tool_result_record(ironclaw_threads::ReadToolResultRecordRequest {
            scope: scope.clone(),
            thread_id: conversation.0.clone(),
            result_ref: reference,
            offset: 0,
            max_bytes: 4096,
        })
        .await
        .unwrap()
        .expect("actual durable result body");
    assert!(
        chunk.next_offset.is_none(),
        "fixture child body must be fully read"
    );
    let body: serde_json::Value = serde_json::from_slice(&chunk.content).unwrap();
    let child_run = ironclaw_turns::TurnRunId::from_uuid(
        body["child_run_id"].as_str().unwrap().parse().unwrap(),
    );
    let child_thread =
        ironclaw_host_api::ThreadId::new(body["child_thread_id"].as_str().unwrap()).unwrap();
    let child_scope = ironclaw_turns::TurnScope::new_with_owner(
        scope.tenant_id.clone(),
        Some(scope.agent_id.clone()),
        None,
        child_thread.clone(),
        scope.owner_user_id.clone(),
    );
    let child = coordinator
        .get_run_state(ironclaw_turns::GetRunStateRequest {
            scope: child_scope,
            run_id: child_run,
        })
        .await
        .unwrap();
    assert_eq!(child.status, ironclaw_turns::TurnStatus::Completed);
    assert_eq!(child.scope.thread_id, child_thread);
    assert_eq!(
        child.source_binding_ref.as_str(),
        format!("subagent-source:{}:{}", reply.run_id, child_run),
        "native child binding must preserve actual parent lineage"
    );
    assert_eq!(body["flavor"], "worldsmith");
}
