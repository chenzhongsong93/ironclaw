//! Host product driver registration. This is an in-process composition input.
use std::sync::Arc;

use ironclaw_host_api::CapabilityId;
use ironclaw_turns::{
    AgentLoopDriver, InMemoryRunProfileRegistry, RunProfileId, TurnRunState,
    run_profile::{CapabilitySurfaceProfileId, PrivilegedRunProfileDimension, RunProfileDefinition},
    runner::ClaimedTurnRun,
};

use crate::driver_registry::{DriverKind, DriverRegistry, DriverRequirements, RequirementLevel};

use super::DefaultPlannedRuntimeBuildError;

/// A host-supplied concrete driver and its privileged profile identity.
pub struct ProductRunDriver {
    pub profile_id: RunProfileId,
    pub capability_surface: CapabilitySurfaceProfileId,
    pub driver: Arc<dyn AgentLoopDriver>,
    pub requirements: DriverRequirements,
    /// Select a finite allowlist from the claimed durable run. None denies the run.
    pub select_capabilities: fn(&TurnRunState) -> Option<Vec<CapabilityId>>,
}

pub(crate) struct ProductCapabilityPolicy {
    profile_id: RunProfileId,
    select: fn(&TurnRunState) -> Option<Vec<CapabilityId>>,
}

impl ProductCapabilityPolicy {
    pub(crate) fn applies_to(&self, run: &ClaimedTurnRun) -> bool {
        run.resolved_run_profile.profile_id == self.profile_id
    }

    pub(crate) fn capabilities(&self, run: &ClaimedTurnRun) -> Option<Vec<CapabilityId>> {
        let actor = run.state.actor.as_ref()?;
        if run.state.scope.explicit_owner_user_id() != Some(&actor.user_id)
            || !run.resolved_run_profile.provenance.effective_privileges
                .contains(&PrivilegedRunProfileDimension::SpecialDriver)
        {
            return None;
        }
        (self.select)(&run.state)
    }
}

impl ProductRunDriver {
    pub(super) fn register(
        self,
        drivers: &mut DriverRegistry,
        profiles: &mut InMemoryRunProfileRegistry,
    ) -> Result<ProductCapabilityPolicy, DefaultPlannedRuntimeBuildError> {
        if self.requirements.capabilities != RequirementLevel::Required {
            return Err(DefaultPlannedRuntimeBuildError::RunProfile(
                "host product driver must use the profiled capability port".into(),
            ));
        }
        let policy = ProductCapabilityPolicy {
            profile_id: self.profile_id.clone(),
            select: self.select_capabilities,
        };
        let key = drivers.register_driver(self.driver, self.requirements, DriverKind::Production)?;
        let driver = drivers.get(&key).ok_or_else(|| {
            DefaultPlannedRuntimeBuildError::RunProfile(
                "host product driver registration did not produce a descriptor".into(),
            )
        })?;
        let profile = RunProfileDefinition::host_product_driver(
            self.profile_id,
            driver.descriptor().clone(),
            self.capability_surface,
        )
        .map_err(|error| DefaultPlannedRuntimeBuildError::RunProfile(error.to_string()))?;
        profiles
            .register(profile)
            .map_err(|error| DefaultPlannedRuntimeBuildError::RunProfile(error.to_string()))?;
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver_registry::{DriverRegistry, DriverRequirements, RequirementLevel};
    use crate::planned_driver_factory::{
        default_planned_run_profile_registry,
        planned_run_profile_resolver_from_registry,
    };
    use async_trait::async_trait;
    use ironclaw_turns::{
        AgentLoopDriver, AgentLoopDriverDescriptor, AgentLoopDriverError,
        AgentLoopDriverResumeRequest, AgentLoopDriverRunRequest, DefaultTurnCoordinator, LoopExit,
        RunProfileRequest, RunProfileResolutionRequest, RunProfileResolver, RunProfileVersion, TurnCoordinator,
        run_profile::{AgentLoopDriverHost, CapabilitySurfaceProfileId},
    };

    struct ProbeDriver {
        descriptor: AgentLoopDriverDescriptor,
    }

    #[async_trait]
    impl AgentLoopDriver for ProbeDriver {
        fn descriptor(&self) -> AgentLoopDriverDescriptor {
            self.descriptor.clone()
        }

        async fn run(
            &self,
            _request: AgentLoopDriverRunRequest,
            _host: &(dyn AgentLoopDriverHost + Send + Sync),
        ) -> Result<LoopExit, AgentLoopDriverError> {
            Err(AgentLoopDriverError::Unavailable {
                reason: "probe driver does not execute".into(),
            })
        }

        async fn resume(
            &self,
            _request: AgentLoopDriverResumeRequest,
            _host: &(dyn AgentLoopDriverHost + Send + Sync),
        ) -> Result<LoopExit, AgentLoopDriverError> {
            Err(AgentLoopDriverError::Unavailable {
                reason: "probe driver does not execute".into(),
            })
        }
    }

    #[tokio::test]
    async fn exact_product_profile_is_ready_after_registration() {
        let profile = RunProfileId::new("reborn-tianquan-novel-reviewer").unwrap();
        let version = RunProfileVersion::new(1);
        let descriptor = AgentLoopDriverDescriptor::new("tianquan:novel-reviewer", version)
            .unwrap()
            .with_checkpoint_schema("tianquan-novel-reviewer-checkpoint", version)
            .unwrap();
        let product = ProductRunDriver {
            profile_id: profile.clone(),
            capability_surface: CapabilitySurfaceProfileId::new("tianquan_novel_reviewer").unwrap(),
            driver: Arc::new(ProbeDriver { descriptor }),
            requirements: DriverRequirements {
                model: RequirementLevel::Unsupported,
                prompt: RequirementLevel::Unsupported,
                transcript: RequirementLevel::Required,
                checkpoint: RequirementLevel::Required,
                input_polling: RequirementLevel::Unsupported,
                capabilities: RequirementLevel::Required,
                progress_events: RequirementLevel::Optional,
            },
            select_capabilities: |_| Some(Vec::new()),
        };
        let mut drivers = DriverRegistry::new();
        let mut profiles = default_planned_run_profile_registry().unwrap();
        product.register(&mut drivers, &mut profiles).unwrap();
        let resolver = Arc::new(planned_run_profile_resolver_from_registry(profiles).unwrap());
        let coordinator = DefaultTurnCoordinator::new(Arc::new(
            ironclaw_turns::test_support::in_memory_turn_state_store(),
        ))
        .with_run_profile_resolver(resolver.clone());
        assert!(
            coordinator
                .product_profile_ready(&profile, version)
                .await
        );
        assert_eq!(
            resolver
                .resolve_run_profile(
                    RunProfileResolutionRequest::interactive_default()
                        .with_requested_run_profile(RunProfileRequest::new(profile.as_str()).unwrap())
                )
                .await
                .unwrap_err()
                .to_string(),
            "requested run profile requires product-surface authority"
        );
    }
}