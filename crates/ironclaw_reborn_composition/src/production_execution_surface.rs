//! 泛用Production真实服务注入首片；不从HTTP/model构造工具、资产或权限。
use crate::RebornRuntimeError;
use async_trait::async_trait;
use ironclaw_host_api::ResourceScope;
use ironclaw_host_runtime::HostRuntime;
use ironclaw_loop_host::{
    CapabilityAllowSet, CapabilityResolveError, CapabilitySurfaceProfileResolver,
    HostIdentityContextSource, HostUserProfileSource, LoopCapabilityInputResolver,
    LoopCapabilityPortFactory, LoopCapabilityResultWriter, ProviderToolCallInputStore,
};
use ironclaw_threads::SessionThreadService;
use ironclaw_turns::run_profile::LoopRunContext;
use std::sync::Arc;

/// 同当前Production store graph的真实上下文；无第二套runner/存储，不能Deserialize。
#[derive(Clone)]
pub struct RebornProductionSurfaceContext {
    pub host_runtime: Arc<dyn HostRuntime>,
    pub thread_service: Arc<dyn SessionThreadService>,
    pub base_scope: ResourceScope,
    pub(crate) gate_record_store: Arc<dyn ironclaw_run_state::GateRecordStore>,
    pub(crate) replay_payload_store: Arc<dyn ironclaw_capabilities::ReplayPayloadStore>,
    pub(crate) provider_input_store: ProviderToolCallInputStore,
    pub(crate) normal_milestones: Arc<dyn ironclaw_turns::run_profile::LoopHostMilestoneSink>,
    pub(crate) disclosure_protocol_active: bool,
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    pub(crate) normal_binding:
        crate::runtime::production_dynamic_capability::ProductionNormalCapabilityBinding,
    pub(crate) skill_selector: Option<crate::runtime::shared_skill_binding::RuntimeSkillSelector>,
    pub(crate) trajectory_observer: Option<Arc<dyn crate::RebornTrajectoryObserver>>,
    pub(crate) outbound_preferences_facade:
        Option<Arc<dyn ironclaw_product_workflow::OutboundPreferencesProductFacade>>,
}
/// 一次受信启动装配的一组正常ports，无具体StagedValue或lower stores导出。
/// factory须持有此装配结果跨claims使用，不在每轮消息重新构造IO。
pub struct RebornProductionBoundPorts {
    factory: Arc<dyn LoopCapabilityPortFactory>,
    input: Arc<dyn LoopCapabilityInputResolver>,
    result: Arc<dyn LoopCapabilityResultWriter>,
}
impl RebornProductionBoundPorts {
    pub fn into_execution_surface(
        self,
        scope: Arc<dyn CapabilitySurfaceProfileResolver>,
        context: RebornProductionContextSources,
    ) -> RebornProductionExecutionSurface {
        RebornProductionExecutionSurface {
            capability_factory: self.factory,
            input_resolver: self.input,
            result_writer: self.result,
            scope_resolver: scope,
            context_sources: context,
        }
    }
}

impl RebornProductionSurfaceContext {
    /// Pair the actual operator identity file with the SAME normal user-profile store.
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    pub fn normal_context_sources(
        &self,
        identity_root: std::path::PathBuf,
        identity_path: std::path::PathBuf,
    ) -> Result<RebornProductionContextSources, RebornRuntimeError> {
        crate::runtime::production_context_sources(
            self.normal_binding.context_filesystem.clone(),
            identity_root,
            identity_path,
            self.disclosure_protocol_active,
        )
    }

    /// Bind the original runtime milestone sink; a consumer cannot replace it with a noop.
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    pub fn bind_normal_runtime_capability_factory(
        &self,
        external_tools: Arc<dyn ironclaw_turns::ExternalToolCatalog>,
    ) -> RebornProductionBoundPorts {
        self.bind_normal_staged_capability_factory(self.normal_milestones.clone(), external_tools)
    }

    /// Finite ceiling from the normal host policy and caller-visible extensions.
    /// No caller-authored grants or lower-store handles leave composition.
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    pub fn normal_capability_surface_resolver(&self) -> Arc<dyn CapabilitySurfaceProfileResolver> {
        Arc::new(crate::runtime::production_dynamic_capability::NormalProductionSurfaceResolver {
            context: self.clone(),
        })
    }

    /// Platform-owned normal authority; caller supplies no grants, trust or mounts.
    /// Retains the SAME IO/gate/replay/provider-input stores across all claims.
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    pub fn bind_normal_staged_capability_factory(
        &self,
        milestones: Arc<dyn ironclaw_turns::run_profile::LoopHostMilestoneSink>,
        external_tools: Arc<dyn ironclaw_turns::ExternalToolCatalog>,
    ) -> RebornProductionBoundPorts {
        let (input, result) = crate::runtime::production_staged_capability_io(
            self.thread_service.clone(),
            self.base_scope.user_id.clone(),
        );
        let factory = Arc::new(
            crate::runtime::production_dynamic_capability::ProductionNormalCapabilityFactory::new(
                self.clone(),
                input.clone(),
                result.clone(),
                milestones,
                external_tools,
            ),
        );
        RebornProductionBoundPorts {
            factory,
            input,
            result,
        }
    }
    /// 使用同runtime真实Thread服务+Owner，以及原durable result生命周期。
    /// VisibleCapabilityRequest仍须来自正常registry/scope，不构成权限自授。
    pub fn bind_staged_host_capability_factory(
        &self,
        request: ironclaw_host_runtime::VisibleCapabilityRequest,
        milestones: Arc<dyn ironclaw_turns::run_profile::LoopHostMilestoneSink>,
        mounts: ironclaw_host_api::MountView,
    ) -> RebornProductionBoundPorts {
        let (input, result) = crate::runtime::production_staged_capability_io(
            self.thread_service.clone(),
            self.base_scope.user_id.clone(),
        );
        let factory = self.bind_host_capability_factory(
            request,
            input.clone(),
            result.clone(),
            milestones,
            mounts,
        );
        RebornProductionBoundPorts {
            factory,
            input,
            result,
        }
    }

    /// 高层facade构造真实port factory；lower stores只由composition内部绑定。
    /// 外部TQ适配不能取得或写入底层store handle，也不能替换为Noop。
    ///
    /// ```compile_fail
    /// use ironclaw_reborn_composition::RebornProductionSurfaceContext;
    /// fn forbidden(context:&RebornProductionSurfaceContext) {
    ///     let _ = &context.gate_record_store;
    /// }
    /// ```
    /// ```compile_fail
    /// use ironclaw_reborn_composition::RebornProductionSurfaceContext;
    /// fn forbidden(context:&RebornProductionSurfaceContext) {
    ///     let _ = &context.replay_payload_store;
    ///     let _ = &context.provider_input_store;
    /// }
    /// ```
    pub fn bind_host_capability_factory(
        &self,
        request: ironclaw_host_runtime::VisibleCapabilityRequest,
        input: Arc<dyn LoopCapabilityInputResolver>,
        result: Arc<dyn LoopCapabilityResultWriter>,
        milestones: Arc<dyn ironclaw_turns::run_profile::LoopHostMilestoneSink>,
        mounts: ironclaw_host_api::MountView,
    ) -> Arc<dyn LoopCapabilityPortFactory> {
        Arc::new(
            ironclaw_loop_host::HostRuntimeLoopCapabilityPortFactory::new(
                self.host_runtime.clone(),
                request,
                input,
                result,
                milestones,
            )
            .with_gate_record_store(self.gate_record_store.clone())
            .with_replay_payload_store(self.replay_payload_store.clone())
            .with_provider_input_store(self.provider_input_store.clone())
            .with_execution_mounts(mounts),
        )
    }
}

/// identity与user profile必须成对；None资料应由真实profile源返回，而非猜身份。
#[derive(Clone)]
pub struct RebornProductionContextSources {
    pub identity: Arc<dyn HostIdentityContextSource>,
    pub user_profile: Arc<dyn HostUserProfileSource>,
}
/// 由受信composition factory装配真实ports，不是capability字符串名录。
pub struct RebornProductionExecutionSurface {
    pub capability_factory: Arc<dyn LoopCapabilityPortFactory>,
    pub input_resolver: Arc<dyn LoopCapabilityInputResolver>,
    pub result_writer: Arc<dyn LoopCapabilityResultWriter>,
    pub scope_resolver: Arc<dyn CapabilitySurfaceProfileResolver>,
    pub context_sources: RebornProductionContextSources,
}
#[async_trait]
pub trait RebornProductionExecutionSurfaceFactory: Send + Sync {
    async fn assemble(
        &self,
        context: RebornProductionSurfaceContext,
    ) -> Result<RebornProductionExecutionSurface, RebornRuntimeError>;
}

/// Production注入不能用All绕正常HostRuntime/scope授权；只有实际受限集合。
pub(crate) struct BoundedProductionSurfaceResolver {
    inner: Arc<dyn CapabilitySurfaceProfileResolver>,
}
impl BoundedProductionSurfaceResolver {
    pub(crate) fn new(inner: Arc<dyn CapabilitySurfaceProfileResolver>) -> Self {
        Self { inner }
    }
}
#[async_trait]
impl CapabilitySurfaceProfileResolver for BoundedProductionSurfaceResolver {
    async fn resolve(
        &self,
        context: &LoopRunContext,
    ) -> Result<CapabilityAllowSet, CapabilityResolveError> {
        let actual = self.inner.resolve(context).await?;
        match actual {
            CapabilityAllowSet::Allowlist(_) => Ok(actual),
            _ => Err(CapabilityResolveError::unavailable(
                "unbounded production surface is not admitted",
            )),
        }
    }

    async fn resolve_for_capabilities(
        &self,
        context: &LoopRunContext,
        capabilities: &(dyn ironclaw_turns::run_profile::LoopCapabilityPort + Send + Sync),
    ) -> Result<CapabilityAllowSet, CapabilityResolveError> {
        match self.inner.resolve_for_capabilities(context, capabilities).await? {
            actual @ CapabilityAllowSet::Allowlist(_) => Ok(actual),
            _ => Err(CapabilityResolveError::unavailable("unbounded production surface is not admitted")),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::{CapabilityId, TenantId, ThreadId};
    use ironclaw_turns::run_profile::{InMemoryRunProfileResolver, RunProfileResolutionRequest};
    use ironclaw_turns::{RunProfileResolver, TurnId, TurnRunId, TurnScope};
    struct FixtureResolver {
        all: bool,
    }
    #[async_trait]
    impl CapabilitySurfaceProfileResolver for FixtureResolver {
        async fn resolve(
            &self,
            _: &LoopRunContext,
        ) -> Result<CapabilityAllowSet, CapabilityResolveError> {
            Ok(if self.all {
                CapabilityAllowSet::All
            } else {
                CapabilityAllowSet::allowlist(
                    [CapabilityId::new("builtin.spawn_subagent").unwrap()],
                )
            })
        }
    }
    async fn context() -> LoopRunContext {
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        LoopRunContext::new(
            TurnScope::new(
                TenantId::new("surface-mechanism-test").unwrap(),
                None,
                None,
                ThreadId::new("surface-test").unwrap(),
            ),
            TurnId::new(),
            TurnRunId::new(),
            profile,
        )
    }
    #[tokio::test]
    async fn production_surface_rejects_unbounded_all_instead_of_self_granting() {
        let resolver =
            BoundedProductionSurfaceResolver::new(Arc::new(FixtureResolver { all: true }));
        assert!(resolver.resolve(&context().await).await.is_err());
    }
    #[tokio::test]
    async fn production_surface_preserves_exact_bound_and_does_not_add_capabilities() {
        let resolver =
            BoundedProductionSurfaceResolver::new(Arc::new(FixtureResolver { all: false }));
        let actual = resolver.resolve(&context().await).await.unwrap();
        assert!(actual.permits(&CapabilityId::new("builtin.spawn_subagent").unwrap()));
        assert!(!actual.permits(&CapabilityId::new("builtin.shell").unwrap()));
    }
}
