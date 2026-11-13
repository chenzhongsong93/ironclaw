use std::sync::{Arc, OnceLock};

use ironclaw_loop_host::{
    CapabilityAllowSet, CapabilityResolveError, CapabilityResultWrite,
    CapabilitySurfaceProfileResolver, CapabilityWriteResult, HostIdentityContextBuildError,
    HostIdentityContextCandidate, HostIdentityContextSource, LoopCapabilityInputResolver,
    LoopCapabilityPortFactory, LoopCapabilityResultWriter,
};
use ironclaw_product_workflow::{
    ApprovalInteractionService, ListPendingApprovalsRequest, ListPendingApprovalsResponse,
    ProductWorkflowError, ResolveApprovalInteractionRequest, ResolveApprovalInteractionResponse,
};
use ironclaw_turns::run_profile::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityInputRef, LoopCapabilityPort,
    LoopRunContext, PromptMode,
};

#[derive(Default)]
pub(super) struct EmptyIdentityContextSource;

#[async_trait::async_trait]
impl HostIdentityContextSource for EmptyIdentityContextSource {
    async fn load_identity_candidates(
        &self,
        _run_context: &LoopRunContext,
        _mode: PromptMode,
    ) -> Result<Vec<HostIdentityContextCandidate>, HostIdentityContextBuildError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
pub(super) struct UnavailableCapabilityIo;

#[async_trait::async_trait]
impl LoopCapabilityInputResolver for UnavailableCapabilityIo {
    async fn resolve_capability_input(
        &self,
        _run_context: &LoopRunContext,
        _input_ref: &CapabilityInputRef,
    ) -> Result<serde_json::Value, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Unavailable,
            "capability input resolver is unavailable for production runtime launch",
        ))
    }
}

#[async_trait::async_trait]
impl LoopCapabilityResultWriter for UnavailableCapabilityIo {
    async fn write_capability_result(
        &self,
        _write: CapabilityResultWrite<'_>,
    ) -> Result<CapabilityWriteResult, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Unavailable,
            "capability result writer is unavailable for production runtime launch",
        ))
    }
}

#[derive(Clone, Default)]
pub(super) struct UnavailableCapabilityPortFactory;

#[async_trait::async_trait]
impl LoopCapabilityPortFactory for UnavailableCapabilityPortFactory {
    async fn create_capability_port(
        &self,
        _run_context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Unavailable,
            "capability port is unavailable for production runtime launch",
        ))
    }
}

/// Production launch is fail-closed until a real capability surface resolver is wired.
/// Every resolution returns an empty allowlist and emits an audit-friendly warning.
pub(super) struct EmptyCapabilitySurfaceResolver;

#[async_trait::async_trait]
impl CapabilitySurfaceProfileResolver for EmptyCapabilitySurfaceResolver {
    async fn resolve(
        &self,
        _run_context: &LoopRunContext,
    ) -> Result<CapabilityAllowSet, CapabilityResolveError> {
        static WARNED_EMPTY_PRODUCTION_CAPABILITY_SURFACE: OnceLock<()> = OnceLock::new();
        WARNED_EMPTY_PRODUCTION_CAPABILITY_SURFACE.get_or_init(|| {
            tracing::warn!(
                "production capability surface resolver is fail-closed; returning empty allowlist"
            );
        });
        tracing::debug!(
            "production capability surface resolver returned fail-closed empty allowlist"
        );
        Ok(CapabilityAllowSet::allowlist(Vec::new()))
    }
}

pub(super) struct UnavailableApprovalInteractionService;

#[async_trait::async_trait]
impl ApprovalInteractionService for UnavailableApprovalInteractionService {
    async fn list_pending(
        &self,
        _request: ListPendingApprovalsRequest,
    ) -> Result<ListPendingApprovalsResponse, ProductWorkflowError> {
        Err(ProductWorkflowError::BeforeInboundPolicyFailed {
            reason: "approval interaction service is not wired for production runtime launch"
                .to_string(),
            permanent: true,
        })
    }

    async fn resolve(
        &self,
        _request: ResolveApprovalInteractionRequest,
    ) -> Result<ResolveApprovalInteractionResponse, ProductWorkflowError> {
        Err(ProductWorkflowError::BeforeInboundPolicyFailed {
            reason: "approval interaction service is not wired for production runtime launch"
                .to_string(),
            permanent: true,
        })
    }
}

#[cfg(test)]
mod production_surface_behavior_red_tests {
    use super::*;
    use ironclaw_host_api::{CapabilityId, TenantId, ThreadId};
    use ironclaw_turns::run_profile::{InMemoryRunProfileResolver, RunProfileResolutionRequest};
    use ironclaw_turns::{RunProfileResolver, TurnId, TurnRunId, TurnScope};

    async fn actual_typed_context_fixture() -> LoopRunContext {
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        let scope = TurnScope::new(
            TenantId::new("production-surface-l1-fixture").unwrap(),
            None,
            None,
            ThreadId::new("surface-l1-thread").unwrap(),
        );
        LoopRunContext::new(scope, TurnId::new(), TurnRunId::new(), profile)
    }

    #[tokio::test]
    async fn current_production_port_unavailability_is_observed_from_real_factory() {
        let context = actual_typed_context_fixture().await;
        match UnavailableCapabilityPortFactory
            .create_capability_port(&context)
            .await
        {
            Ok(_) => panic!("当前明确Unavailable factory不应返回port"),
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("capability port is unavailable for production runtime launch")
            ),
        }
    }

    #[tokio::test]
    async fn required_production_surface_must_admit_registered_spawn_behavior_red() {
        let context = actual_typed_context_fixture().await;
        // 直接调用当前生产resolver；没有mock resolver/AllowAll/注入capability集合。
        let actual = EmptyCapabilitySurfaceResolver
            .resolve(&context)
            .await
            .unwrap();
        let spawn = CapabilityId::new("builtin.spawn_subagent").unwrap();
        println!(
            "ACTUAL_PRODUCTION_SPAWN_PERMITTED={}",
            actual.permits(&spawn)
        );
        assert!(
            actual.permits(&spawn),
            "Required生产世界宿主不能把注册spawn隐退为空surface"
        );
    }
}
