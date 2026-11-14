//! 正常 Production factory 到 runner decorators/profile/prompt/provider 的原生surface回归。
#![cfg(all(feature = "libsql", feature = "test-support"))]

use crate::{
    RebornBuildInput, RebornCompositionProfile, RebornProductionExecutionSurface,
    RebornProductionExecutionSurfaceFactory, RebornProductionSurfaceContext, RebornRuntimeError,
    RebornRuntimeIdentity, RebornRuntimeInput, RebornRuntimeProcessBinding, build_reborn_runtime,
    builtin_first_party_trust_policy,
};
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
use ironclaw_runner::model_gateway::{LlmModelProfilePolicy, LlmProviderModelGateway};
use ironclaw_turns::run_profile::ModelProfileId;
use rust_decimal::Decimal;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "production_result_read_controls.rs"]
mod result_read_controls;

struct NormalSurfaceFactory {
    identity_root: PathBuf,
}
#[async_trait]
impl RebornProductionExecutionSurfaceFactory for NormalSurfaceFactory {
    async fn assemble(
        &self,
        mut context: RebornProductionSurfaceContext,
    ) -> Result<RebornProductionExecutionSurface, RebornRuntimeError> {
        let native_context = context.clone();
        context.skill_selector = None;
        let sources = context.normal_context_sources(
            self.identity_root.clone(),
            self.identity_root.join("AGENTS.md"),
        )?;
        let resolver = context.normal_capability_surface_resolver();
        let surface = context
            .bind_normal_runtime_capability_factory(Arc::new(
                ironclaw_turns::InMemoryExternalToolCatalog::default(),
            ))
            .into_execution_surface(resolver, sources);
        use ironclaw_turns::RunProfileResolver;
        let resolved =
            ironclaw_runner::planned_driver_factory::default_planned_run_profile_resolver()
                .unwrap()
                .resolve_run_profile(
                    ironclaw_turns::RunProfileResolutionRequest::interactive_default(),
                )
                .await
                .unwrap();
        let run = |tenant: &str, owner: Option<&str>| {
            ironclaw_turns::run_profile::LoopRunContext::new(
                ironclaw_turns::TurnScope::new_with_owner(
                    ironclaw_host_api::TenantId::new(tenant).unwrap(),
                    Some(ironclaw_host_api::AgentId::new("native-surface-agent").unwrap()),
                    None,
                    ironclaw_host_api::ThreadId::new("native-surface-controls").unwrap(),
                    owner.map(|v| ironclaw_host_api::UserId::new(v).unwrap()),
                ),
                ironclaw_turns::TurnId::new(),
                ironclaw_turns::TurnRunId::new(),
                resolved.clone(),
            )
        };
        assert!(
            surface
                .capability_factory
                .create_capability_port(&run(
                    "foreign-native-surface-tenant",
                    Some("native-surface-owner")
                ))
                .await
                .is_err(),
            "real normal factory must reject foreign tenant before assembly"
        );
        assert!(
            surface
                .capability_factory
                .create_capability_port(&run("native-surface-tenant", None))
                .await
                .is_err(),
            "real normal factory must reject absent owner and actor"
        );
        let normal_run = run("native-surface-tenant", Some("native-surface-owner"));
        let native_bound = native_context.bind_normal_runtime_capability_factory(Arc::new(
            ironclaw_turns::InMemoryExternalToolCatalog::default(),
        ));
        let native_sources = native_context.normal_context_sources(
            self.identity_root.clone(),
            self.identity_root.join("AGENTS.md"),
        )?;
        let native_resolver = native_context.normal_capability_surface_resolver();
        let native_surface =
            native_bound.into_execution_surface(native_resolver.clone(), native_sources);
        let actual_port = native_surface
            .capability_factory
            .create_capability_port(&normal_run)
            .await
            .unwrap();
        assert!(
            actual_port
                .tool_definitions()
                .unwrap()
                .iter()
                .any(|definition| definition.capability_id.as_str() == "builtin.skill_activate"),
            "positive control: actual selector port advertised"
        );
        result_read_controls::assert_normal_result_read(
            &native_context,
            &normal_run,
            actual_port.clone(),
            native_resolver.clone(),
        )
        .await;
        let mut scheduled_run = normal_run.clone();
        scheduled_run.resolved_run_profile =
            ironclaw_runner::planned_driver_factory::default_planned_run_profile_resolver()
                .unwrap()
                .resolve_run_profile(
                    ironclaw_turns::RunProfileResolutionRequest::interactive_default()
                        .with_requested_run_profile(
                            ironclaw_turns::RunProfileRequest::new(
                                ironclaw_turns::RunProfileId::scheduled_trigger().as_str(),
                            )
                            .unwrap(),
                        ),
                )
                .await
                .unwrap();
        let denied = native_resolver
            .resolve_for_capabilities(&scheduled_run, actual_port.as_ref())
            .await
            .unwrap();
        assert!(
            !denied
                .permits(&ironclaw_host_api::CapabilityId::new("builtin.skill_activate").unwrap()),
            "advertised native activation must not mint permission for denied profile"
        );
        let mut unknown_run = normal_run;
        unknown_run.resolved_run_profile.profile_id =
            ironclaw_turns::RunProfileId::new("unknown-native-profile").unwrap();
        assert!(
            native_resolver
                .resolve_for_capabilities(&unknown_run, actual_port.as_ref())
                .await
                .is_err(),
            "unknown registered profile must fail closed even with actual advertised port"
        );
        Ok(surface)
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
async fn production_native_surface_controls_missing_selector_and_scope() {
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
    assert!(
        !names.iter().any(|name| name == "builtin__skill_activate"),
        "missing real selector must not advertise activation"
    );
    // NRR-L1-01: actual normal Production factory -> finite profile -> provider.
    // The original runtime has already shut down above; this must be a business
    // assertion RED on the old finite ceiling, never a missing-symbol RED.
    assert_eq!(
        names
            .iter()
            .filter(|name| name.as_str() == Some("builtin__result_read"))
            .count(),
        1,
        "normal planned provider must retain the installed result_read handler"
    );
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
    assert!(
        !capability_sections.iter().any(|section| section
            .lines()
            .any(|line| line.trim() == "- id: builtin.skill_activate")),
        "missing selector must remain absent from prompt capabilities"
    );
}
