//! Production normal authority binding; no startup request/grant DTO or LocalDev profile.
use super::local_dev::{
    extension_surface::ExtensionCapabilitySurfaceSource,
    refreshing_capability_port::{
        RefreshingCapabilityPortConfig, create_refreshing_capability_port,
    },
};
use ironclaw_host_api::{
    MountAlias, MountGrant, MountPermissions, MountView, ResourceScope, UserId, VirtualPath,
};
use ironclaw_loop_host::{
    LoopCapabilityInputResolver, LoopCapabilityPortFactory, LoopCapabilityResultWriter,
};
use ironclaw_turns::run_profile::{
    AgentLoopHostError, AgentLoopHostErrorKind, LoopCapabilityPort, LoopHostMilestoneSink,
    LoopRunContext,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

#[derive(Clone)]
pub(crate) struct ProductionNormalCapabilityBinding {
    pub(crate) policy: Arc<crate::builtin_capability_policy::BuiltinCapabilityPolicy>,
    pub(crate) extensions: ExtensionCapabilitySurfaceSource,
    pub(crate) project_service: Arc<dyn ironclaw_product_workflow::ProjectService>,
    pub(crate) approval_settings:
        Arc<dyn crate::profile_approval_authorization::ApprovalSettingsProvider>,
    pub(crate) approval_requests: Arc<dyn ironclaw_run_state::ApprovalRequestStore>,
    pub(crate) leases: Arc<dyn ironclaw_authorization::CapabilityLeaseStore>,
    pub(crate) context_filesystem: Arc<dyn ironclaw_filesystem::RootFilesystem>,
    pub(crate) skill_source: Arc<dyn ironclaw_loop_host::SkillBundleSource>,
}

pub(crate) fn actual_owner(run: &LoopRunContext) -> Result<UserId, AgentLoopHostError> {
    run.scope.explicit_owner_user_id().cloned().or_else(||run.actor().map(|a|a.user_id.clone()))
        .filter(|u|u.as_str()!=ironclaw_host_api::SYSTEM_RESERVED_ID)
        .ok_or_else(||AgentLoopHostError::new(AgentLoopHostErrorKind::ScopeMismatch,
            "Production normal capability binding requires actual thread owner or authenticated actor"))
}
fn mount(
    alias: &str,
    target: String,
    permissions: MountPermissions,
) -> Result<MountView, AgentLoopHostError> {
    MountView::new(vec![MountGrant::new(
        MountAlias::new(alias).map_err(scope_error)?,
        VirtualPath::new(target).map_err(scope_error)?,
        permissions,
    )])
    .map_err(scope_error)
}
fn scope_error(e: impl std::fmt::Display) -> AgentLoopHostError {
    AgentLoopHostError::new(AgentLoopHostErrorKind::ScopeMismatch, e.to_string())
}
pub(crate) struct ProductionNormalCapabilityFactory {
    context: crate::RebornProductionSurfaceContext,
    input: Arc<dyn LoopCapabilityInputResolver>,
    result: Arc<dyn LoopCapabilityResultWriter>,
    milestones: Arc<dyn LoopHostMilestoneSink>,
    external_tools: Arc<dyn ironclaw_turns::ExternalToolCatalog>,
}
impl ProductionNormalCapabilityFactory {
    pub(crate) fn new(
        context: crate::RebornProductionSurfaceContext,
        input: Arc<dyn LoopCapabilityInputResolver>,
        result: Arc<dyn LoopCapabilityResultWriter>,
        milestones: Arc<dyn LoopHostMilestoneSink>,
        external_tools: Arc<dyn ironclaw_turns::ExternalToolCatalog>,
    ) -> Self {
        Self {
            context,
            input,
            result,
            milestones,
            external_tools,
        }
    }
}
#[async_trait::async_trait]
impl LoopCapabilityPortFactory for ProductionNormalCapabilityFactory {
    async fn create_capability_port(
        &self,
        run: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        let owner = actual_owner(run)?;
        if run.scope.tenant_id != self.context.base_scope.tenant_id {
            return Err(scope_error(
                "Production normal factory tenant differs from configured runtime",
            ));
        }
        let mut scope: ResourceScope = run.scope.to_resource_scope();
        scope.user_id = owner.clone();
        let prefix = format!(
            "/tenants/{}/users/{}",
            scope.tenant_id.as_str(),
            owner.as_str()
        );
        let workspace = mount(
            "/workspace",
            format!("{prefix}/workspace"),
            MountPermissions::read_write(),
        )?;
        let memory = mount(
            "/memory",
            format!("{prefix}/memory"),
            MountPermissions::read_write_list_delete(),
        )?;
        let skills =
            crate::factory::production_skill_management_mount_view(&scope).map_err(scope_error)?;
        let lifecycle = crate::local_dev_mounts::system_extensions_lifecycle_mount_view()
            .map_err(scope_error)?;
        let binding = &self.context.normal_binding;
        create_refreshing_capability_port(RefreshingCapabilityPortConfig {
            runtime: self.context.host_runtime.clone(),
            run_context: run.clone(),
            provider_input_store: self.context.provider_input_store.clone(),
            fallback_user_id: owner,
            production_bound: true,
            policy: binding.policy.clone(),
            workspace_mounts: workspace,
            skill_mounts: skills,
            memory_mounts: memory,
            system_extensions_lifecycle_mounts: lifecycle,
            extension_surface_source: binding.extensions.clone(),
            input_resolver: self.input.clone(),
            result_writer: self.result.clone(),
            milestone_sink: self.milestones.clone(),
            skill_activation_source: self.context.skill_selector.clone(),
            project_service: binding.project_service.clone(),
            thread_service: self.context.thread_service.clone(),
            trajectory_observer: self.context.trajectory_observer.clone(),
            outbound_preferences_facade: self.context.outbound_preferences_facade.clone(),
            outbound_delivery_target_set_requires_approval: true,
            approval_settings: binding.approval_settings.clone(),
            approval_requests: binding.approval_requests.clone(),
            capability_leases: binding.leases.clone(),
            gate_record_store: self.context.gate_record_store.clone(),
            replay_payload_store: self.context.replay_payload_store.clone(),
            external_tool_catalog: self.external_tools.clone(),
            capability_execution_mount_overrides: HashMap::new(),
            additional_provider_trust: BTreeMap::new(),
            capability_id_filter: None,
            additional_capability_grants: Vec::new(),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::{TenantId, ThreadId};
    use ironclaw_turns::run_profile::InMemoryRunProfileResolver;
    use ironclaw_turns::{
        RunProfileResolutionRequest, RunProfileResolver, TurnActor, TurnId, TurnRunId, TurnScope,
    };
    async fn run(owner: Option<&str>, actor: Option<&str>) -> LoopRunContext {
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        let mut run = LoopRunContext::new(
            TurnScope::new_with_owner(
                TenantId::new("production-binding-test").unwrap(),
                None,
                None,
                ThreadId::new("actual-test-thread").unwrap(),
                owner.map(|v| UserId::new(v).unwrap()),
            ),
            TurnId::new(),
            TurnRunId::new(),
            profile,
        );
        if let Some(actor) = actor {
            run = run.with_actor(TurnActor::new(UserId::new(actor).unwrap()));
        }
        run
    }
    #[tokio::test]
    async fn production_owner_precedes_authenticated_actor() {
        let run = run(Some("thread-owner"), Some("real-actor")).await;
        assert_eq!(actual_owner(&run).unwrap().as_str(), "thread-owner");
        assert_eq!(run.actor().unwrap().user_id.as_str(), "real-actor");
    }
    #[tokio::test]
    async fn production_actor_is_actual_owner_when_thread_has_none() {
        let run = run(None, Some("real-actor")).await;
        assert_eq!(actual_owner(&run).unwrap().as_str(), "real-actor");
    }
    #[tokio::test]
    async fn production_unknown_owner_and_actor_rejected_before_grants() {
        let run = run(None, None).await;
        let error = actual_owner(&run).unwrap_err();
        assert_eq!(error.kind, AgentLoopHostErrorKind::ScopeMismatch);
        assert_eq!(
            error.safe_summary,
            "Production normal capability binding requires actual thread owner or authenticated actor"
        );
    }
}

/// A finite grant ceiling from the SAME policy and extension visibility rules as
/// the normal dispatch port. The underlying host still checks all obligations.
pub(crate) struct NormalProductionSurfaceResolver {
    pub(crate) context: crate::RebornProductionSurfaceContext,
}

fn validate_normal_surface_scope(
    run: &LoopRunContext,
    tenant: &ironclaw_host_api::TenantId,
) -> Result<UserId, AgentLoopHostError> {
    let owner = actual_owner(run)?;
    if &run.scope.tenant_id != tenant {
        return Err(scope_error(
            "Production normal surface tenant differs from configured runtime",
        ));
    }
    Ok(owner)
}

#[async_trait::async_trait]
impl ironclaw_loop_host::CapabilitySurfaceProfileResolver for NormalProductionSurfaceResolver {
    async fn resolve(
        &self,
        run: &LoopRunContext,
    ) -> Result<ironclaw_loop_host::CapabilityAllowSet, ironclaw_loop_host::CapabilityResolveError>
    {
        self.resolve_grants(run).await.map_err(|_| {
            ironclaw_loop_host::CapabilityResolveError::unavailable(
                "normal production capability surface is unavailable",
            )
        })
    }

    async fn resolve_for_capabilities(
        &self,
        run: &LoopRunContext,
        capabilities: &(dyn LoopCapabilityPort + Send + Sync),
    ) -> Result<ironclaw_loop_host::CapabilityAllowSet, ironclaw_loop_host::CapabilityResolveError>
    {
        let original = self.resolve_grants(run).await.map_err(|_| {
            ironclaw_loop_host::CapabilityResolveError::unavailable(
                "normal production capability surface is unavailable",
            )
        })?;
        let ceiling =
            ironclaw_runner::planned_driver_factory::registered_native_capability_ceiling(run)
                .await?;
        let surface = capabilities
            .visible_capabilities(ironclaw_turns::run_profile::VisibleCapabilityRequest {})
            .await
            .map_err(|_| {
                ironclaw_loop_host::CapabilityResolveError::unavailable(
                    "normal decorated surface unavailable",
                )
            })?;
        let mut permitted = match original {
            ironclaw_loop_host::CapabilityAllowSet::Allowlist(ids) => ids,
            _ => {
                return Err(ironclaw_loop_host::CapabilityResolveError::unavailable(
                    "normal policy must remain finite",
                ));
            }
        };
        // Visibility is only the availability intersection. The runner's sealed
        // registered profile supplies permission; no arbitrary descriptor is added.
        for descriptor in surface.descriptors {
            let id = descriptor.capability_id;
            if surface
                .callable_capability_ids
                .as_ref()
                .is_some_and(|ids| !ids.contains(&id))
            {
                continue;
            }
            let eligible = (ceiling.permits_spawn()
                && id.as_str() == ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID)
                || (ceiling.permits_skill_activation()
                    && self.context.skill_selector.is_some()
                    && id.as_str() == super::local_dev::SKILL_ACTIVATE_CAPABILITY_ID)
                || (ceiling.permits_result_read()
                    && id.as_str() == super::local_dev::RESULT_READ_CAPABILITY_ID);
            if eligible {
                permitted.insert(id);
            }
        }
        Ok(ironclaw_loop_host::CapabilityAllowSet::allowlist(permitted))
    }
}

impl NormalProductionSurfaceResolver {
    async fn resolve_grants(
        &self,
        run: &LoopRunContext,
    ) -> Result<ironclaw_loop_host::CapabilityAllowSet, AgentLoopHostError> {
        let owner = validate_normal_surface_scope(run, &self.context.base_scope.tenant_id)?;
        let mut scope = run.scope.to_resource_scope();
        scope.user_id = owner.clone();
        let prefix = format!(
            "/tenants/{}/users/{}",
            scope.tenant_id.as_str(),
            owner.as_str()
        );
        let workspace = mount(
            "/workspace",
            format!("{prefix}/workspace"),
            MountPermissions::read_write(),
        )?;
        let memory = mount(
            "/memory",
            format!("{prefix}/memory"),
            MountPermissions::read_write_list_delete(),
        )?;
        let skills =
            crate::factory::production_skill_management_mount_view(&scope).map_err(scope_error)?;
        let lifecycle = crate::local_dev_mounts::system_extensions_lifecycle_mount_view()
            .map_err(scope_error)?;
        let extensions = self
            .context
            .normal_binding
            .extensions
            .snapshot()
            .await
            .map_err(scope_error)?;
        let request = super::local_dev::visible_capability_request(
            run,
            &owner,
            super::local_dev::VisibleCapabilityInputs {
                workspace_mounts: &workspace,
                skill_mounts: &skills,
                memory_mounts: &memory,
                system_extensions_lifecycle_mounts: &lifecycle,
                policy: &self.context.normal_binding.policy,
                extension_surface: &extensions,
            },
        )?;
        Ok(ironclaw_loop_host::CapabilityAllowSet::allowlist(
            request
                .context
                .grants
                .grants
                .into_iter()
                .map(|grant| grant.capability),
        ))
    }
}

#[cfg(test)]
mod normal_surface_scope_tests {
    use super::*;
    use ironclaw_host_api::{TenantId, ThreadId};
    use ironclaw_turns::run_profile::InMemoryRunProfileResolver;
    use ironclaw_turns::{
        RunProfileResolutionRequest, RunProfileResolver, TurnId, TurnRunId, TurnScope,
    };

    async fn context(owner: Option<&str>) -> LoopRunContext {
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        LoopRunContext::new(
            TurnScope::new_with_owner(
                TenantId::new("owned-tenant").unwrap(),
                None,
                None,
                ThreadId::new("actual-thread").unwrap(),
                owner.map(|v| UserId::new(v).unwrap()),
            ),
            TurnId::new(),
            TurnRunId::new(),
            profile,
        )
    }

    // 覆盖:TQH-L1-02，非UUID合法owner不能在能力装配阶段被替换。
    #[tokio::test]
    async fn normal_surface_keeps_actual_non_uuid_owner() {
        let run = context(Some("owner_08b7f32555d349248702af0f98d8a448")).await;
        let owner =
            validate_normal_surface_scope(&run, &TenantId::new("owned-tenant").unwrap()).unwrap();
        assert_eq!(owner, run.scope.explicit_owner_user_id().unwrap().clone());
    }

    // 覆盖:TQH-L1-02，foreign tenant在registry读取与grant生成前拒绝。
    #[tokio::test]
    async fn normal_surface_rejects_foreign_tenant() {
        let run = context(Some("real-owner")).await;
        assert!(
            validate_normal_surface_scope(&run, &TenantId::new("foreign-tenant").unwrap()).is_err()
        );
    }

    // 覆盖:TQH-L1-02，未知owner不回落启动owner。
    #[tokio::test]
    async fn normal_surface_rejects_unknown_owner() {
        let run = context(None).await;
        assert!(
            validate_normal_surface_scope(&run, &TenantId::new("owned-tenant").unwrap()).is_err()
        );
    }
}

#[cfg(all(test, feature = "libsql", feature = "test-support"))]
#[path = "production_native_surface_controls.rs"]
mod production_native_surface_controls;
