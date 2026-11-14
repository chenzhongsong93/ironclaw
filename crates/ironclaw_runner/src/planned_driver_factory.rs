//! Production registration helpers for the default planned Reborn loop.

use std::{error::Error, fmt, sync::Arc};

use ironclaw_agent_loop::{
    executor::CanonicalAgentLoopExecutor,
    family::{LoopFamilyId, LoopFamilyRegistry},
    state::{CHECKPOINT_SCHEMA_ID, CHECKPOINT_SCHEMA_VERSION},
};
use ironclaw_turns::{
    AgentLoopDriver, AgentLoopDriverDescriptor, AgentLoopDriverError, RunProfileId,
    RunProfileVersion,
    run_profile::{
        CapabilitySurfaceProfileId, CheckpointSchemaId, InMemoryRunProfileRegistry,
        InMemoryRunProfileResolver, ModelProfileId, RunProfileDefinition, RunProfileRegistryError,
    },
};

use crate::{
    driver_registry::{
        DriverKind, DriverRegistry, DriverRegistryError, DriverRequirements, LoopDriverRegistryKey,
        RequirementLevel,
    },
    planned_driver::PlannedDriver,
    text_loop_driver::{TextOnlyModelReplyDriver, TextOnlyModelReplyDriverConfig},
};

pub const PLANNED_DRIVER_DEFAULT_ID: &str = "reborn:planned-default";
pub const PLANNED_DRIVER_DEFAULT_VERSION: u64 = 1;
pub const PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID: &str = CHECKPOINT_SCHEMA_ID;
pub const PLANNED_DRIVER_CHECKPOINT_SCHEMA_VERSION: u64 = CHECKPOINT_SCHEMA_VERSION;
pub const PLANNED_DEFAULT_PROFILE_ID: &str = "reborn-planned-default";
pub const SUBAGENT_PLANNED_DRIVER_ID: &str = "reborn:planned-subagent";
pub const SUBAGENT_PLANNED_PROFILE_ID: &str = "reborn-planned-subagent";
/// Dedicated run profile for the novelist subagent, which routes to the
/// `mission_model` profile (ModelSlot::Mission) so the novelist writer runs on
/// a cheaper dedicated provider (TianQuan: minimax) while the orchestrating
/// parent runs on the primary model (deepseek-v4-flash). See
/// `tianquan_subagents` resolver and the model-route wiring in
/// `ironclaw_reborn_composition::runtime`.
pub const SUBAGENT_NOVELIST_PROFILE_ID: &str = "reborn-planned-novelist";
/// Capability-surface profile id for the default interactive planned driver.
const INTERACTIVE_CAPABILITY_SURFACE_PROFILE_ID: &str = "interactive_tools";
pub const SUBAGENT_CAPABILITY_SURFACE_PROFILE_ID: &str = "subagent_tools";
/// Model profile bound to the mission model slot (cheaper/secondary provider).
pub const MISSION_MODEL_PROFILE_ID: &str = "mission_model";
/// Capability-surface profile id for scheduled-trigger fires (issue #5505).
/// Shared with `runtime.rs`, which keys its per-profile deny-map on this
/// string to strip the trigger mutator capabilities from a fire's
/// model-visible surface.
pub const SCHEDULED_TRIGGER_CAPABILITY_SURFACE_PROFILE_ID: &str = "scheduled_trigger";

pub struct DefaultPlannedDriverBuild {
    pub driver: Arc<dyn AgentLoopDriver>,
    pub descriptor: AgentLoopDriverDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultPlannedDriverRegistrationError {
    DriverBuild(AgentLoopDriverError),
    Registry(DriverRegistryError),
}

impl fmt::Display for DefaultPlannedDriverRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DriverBuild(error) => {
                write!(formatter, "default planned driver build failed: {error}")
            }
            Self::Registry(error) => write!(
                formatter,
                "default planned driver registration failed: {error}"
            ),
        }
    }
}

impl Error for DefaultPlannedDriverRegistrationError {}

impl From<AgentLoopDriverError> for DefaultPlannedDriverRegistrationError {
    fn from(error: AgentLoopDriverError) -> Self {
        Self::DriverBuild(error)
    }
}

impl From<DriverRegistryError> for DefaultPlannedDriverRegistrationError {
    fn from(error: DriverRegistryError) -> Self {
        Self::Registry(error)
    }
}

pub fn planned_driver_default_id() -> Result<ironclaw_turns::run_profile::LoopDriverId, String> {
    ironclaw_turns::run_profile::LoopDriverId::new(PLANNED_DRIVER_DEFAULT_ID)
}

pub fn planned_driver_checkpoint_schema_id() -> Result<CheckpointSchemaId, String> {
    CheckpointSchemaId::new(PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID)
}

pub fn planned_default_profile_id() -> Result<RunProfileId, String> {
    RunProfileId::new(PLANNED_DEFAULT_PROFILE_ID)
}

pub fn subagent_planned_profile_id() -> Result<RunProfileId, String> {
    RunProfileId::new(SUBAGENT_PLANNED_PROFILE_ID)
}

pub fn subagent_novelist_planned_profile_id() -> Result<RunProfileId, String> {
    RunProfileId::new(SUBAGENT_NOVELIST_PROFILE_ID)
}

pub(crate) fn is_subagent_planned_profile(
    profile: &ironclaw_turns::run_profile::ResolvedRunProfile,
) -> bool {
    profile.loop_driver.id.as_str() == SUBAGENT_PLANNED_DRIVER_ID
        && profile.loop_driver.version == planned_driver_default_version()
        && profile.capability_surface_profile_id.as_str() == SUBAGENT_CAPABILITY_SURFACE_PROFILE_ID
}

pub(crate) fn is_subagent_planned_run_profile(
    run_context: &ironclaw_turns::run_profile::LoopRunContext,
) -> bool {
    is_subagent_planned_profile(&run_context.resolved_run_profile)
}

pub fn planned_driver_default_version() -> RunProfileVersion {
    RunProfileVersion::new(PLANNED_DRIVER_DEFAULT_VERSION)
}

pub fn planned_driver_checkpoint_schema_version() -> RunProfileVersion {
    RunProfileVersion::new(PLANNED_DRIVER_CHECKPOINT_SCHEMA_VERSION)
}

pub fn planned_driver_descriptor() -> Result<AgentLoopDriverDescriptor, String> {
    AgentLoopDriverDescriptor::new(PLANNED_DRIVER_DEFAULT_ID, planned_driver_default_version())?
        .with_checkpoint_schema(
            PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID,
            planned_driver_checkpoint_schema_version(),
        )
}

pub fn subagent_planned_driver_descriptor() -> Result<AgentLoopDriverDescriptor, String> {
    AgentLoopDriverDescriptor::new(SUBAGENT_PLANNED_DRIVER_ID, planned_driver_default_version())?
        .with_checkpoint_schema(
            PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID,
            planned_driver_checkpoint_schema_version(),
        )
}

pub fn default_planned_driver(
    family_registry: Arc<LoopFamilyRegistry>,
) -> Result<DefaultPlannedDriverBuild, AgentLoopDriverError> {
    let family = family_registry.get(&LoopFamilyId::DEFAULT).ok_or_else(|| {
        AgentLoopDriverError::InvalidRequest {
            reason: "default loop family is not registered".to_string(),
        }
    })?;
    let descriptor = planned_driver_descriptor()
        .map_err(|reason| AgentLoopDriverError::InvalidRequest { reason })?;
    let executor = Arc::new(CanonicalAgentLoopExecutor);
    let driver = PlannedDriver::from_family_with_descriptor(family, executor, descriptor.clone())?;
    Ok(DefaultPlannedDriverBuild {
        driver: Arc::new(driver),
        descriptor,
    })
}

pub fn subagent_planned_driver(
    family_registry: Arc<LoopFamilyRegistry>,
) -> Result<DefaultPlannedDriverBuild, AgentLoopDriverError> {
    let family = family_registry
        .get(&LoopFamilyId::SUBAGENT)
        .ok_or_else(|| AgentLoopDriverError::InvalidRequest {
            reason: "subagent loop family is not registered".to_string(),
        })?;
    let descriptor = subagent_planned_driver_descriptor()
        .map_err(|reason| AgentLoopDriverError::InvalidRequest { reason })?;
    let executor = Arc::new(CanonicalAgentLoopExecutor);
    let driver = PlannedDriver::from_family_with_descriptor(family, executor, descriptor.clone())?;
    Ok(DefaultPlannedDriverBuild {
        driver: Arc::new(driver),
        descriptor,
    })
}

pub fn planned_driver_requirements() -> DriverRequirements {
    DriverRequirements {
        model: RequirementLevel::Required,
        prompt: RequirementLevel::Required,
        transcript: RequirementLevel::Required,
        checkpoint: RequirementLevel::Required,
        input_polling: RequirementLevel::Required,
        capabilities: RequirementLevel::Required,
        progress_events: RequirementLevel::Required,
    }
}

pub fn register_default_planned_driver(
    registry: &mut DriverRegistry,
    family_registry: Arc<LoopFamilyRegistry>,
) -> Result<LoopDriverRegistryKey, DefaultPlannedDriverRegistrationError> {
    let build = default_planned_driver(family_registry)?;
    registry
        .register_driver(
            build.driver,
            planned_driver_requirements(),
            DriverKind::Production,
        )
        .map_err(Into::into)
}

pub fn register_subagent_planned_driver(
    registry: &mut DriverRegistry,
    family_registry: Arc<LoopFamilyRegistry>,
) -> Result<LoopDriverRegistryKey, DefaultPlannedDriverRegistrationError> {
    let build = subagent_planned_driver(family_registry)?;
    registry
        .register_driver(
            build.driver,
            planned_driver_requirements(),
            DriverKind::Production,
        )
        .map_err(Into::into)
}

pub fn register_default_text_only_driver(
    registry: &mut DriverRegistry,
    config: TextOnlyModelReplyDriverConfig,
) -> Result<LoopDriverRegistryKey, DriverRegistryError> {
    registry.register_driver(
        Arc::new(TextOnlyModelReplyDriver::new(config)),
        DriverRequirements::all_optional(),
        DriverKind::Production,
    )
}

/// Shared scaffold behind `planned_default_profile_definition()`,
/// `subagent_planned_profile_definition()`, and
/// `scheduled_trigger_planned_profile_definition()`: wraps
/// `RunProfileDefinition::interactive_like` with the checkpoint schema id/
/// version that every planned profile shares, plus the common
/// `InvalidProfile` error mapping.
fn planned_like_profile_definition(
    profile_id: RunProfileId,
    descriptor: AgentLoopDriverDescriptor,
    capability_surface_profile_id: &str,
) -> Result<RunProfileDefinition, RunProfileRegistryError> {
    let checkpoint_schema_id = planned_driver_checkpoint_schema_id()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    let capability_surface_profile_id =
        CapabilitySurfaceProfileId::new(capability_surface_profile_id)
            .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    Ok(RunProfileDefinition::interactive_like(
        profile_id,
        descriptor,
        checkpoint_schema_id,
        planned_driver_checkpoint_schema_version(),
        capability_surface_profile_id,
    ))
}

pub fn planned_default_profile_definition() -> Result<RunProfileDefinition, RunProfileRegistryError>
{
    let descriptor = planned_driver_descriptor()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    let profile_id = planned_default_profile_id()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    planned_like_profile_definition(
        profile_id,
        descriptor,
        INTERACTIVE_CAPABILITY_SURFACE_PROFILE_ID,
    )
}

pub fn subagent_planned_profile_definition() -> Result<RunProfileDefinition, RunProfileRegistryError>
{
    let descriptor = subagent_planned_driver_descriptor()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    let profile_id = subagent_planned_profile_id()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    planned_like_profile_definition(
        profile_id,
        descriptor,
        SUBAGENT_CAPABILITY_SURFACE_PROFILE_ID,
    )
}

/// Run profile for the novelist subagent — same planned subagent driver and
/// capability surface as `reborn-planned-subagent`, but bound to the mission
/// model profile so the writer run resolves to `ModelSlot::Mission` and uses
/// the dedicated cheaper provider (TianQuan: minimax) while the parent
/// orchestrator stays on the primary model.
pub fn subagent_novelist_planned_profile_definition()
-> Result<RunProfileDefinition, RunProfileRegistryError> {
    let descriptor = subagent_planned_driver_descriptor()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    let profile_id = subagent_novelist_planned_profile_id()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    let model_profile_id = ModelProfileId::new(MISSION_MODEL_PROFILE_ID)
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    let definition = planned_like_profile_definition(
        profile_id,
        descriptor,
        SUBAGENT_CAPABILITY_SURFACE_PROFILE_ID,
    )?;
    Ok(definition.with_model_profile_id(model_profile_id))
}

/// Dedicated run profile for scheduled-trigger fires (issue #5505). Reuses
/// the default planned driver/family unchanged — only the capability
/// surface differs — so `runtime.rs`'s host deny-map can strip the trigger
/// mutator capabilities keyed on `capability_surface_profile_id`.
pub fn scheduled_trigger_planned_profile_definition()
-> Result<RunProfileDefinition, RunProfileRegistryError> {
    let descriptor = planned_driver_descriptor()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    planned_like_profile_definition(
        RunProfileId::scheduled_trigger(),
        descriptor,
        SCHEDULED_TRIGGER_CAPABILITY_SURFACE_PROFILE_ID,
    )
}

pub fn register_default_planned_profile(
    registry: &mut InMemoryRunProfileRegistry,
) -> Result<(), RunProfileRegistryError> {
    registry.register(planned_default_profile_definition()?)
}

pub fn register_subagent_planned_profile(
    registry: &mut InMemoryRunProfileRegistry,
) -> Result<(), RunProfileRegistryError> {
    registry.register(subagent_planned_profile_definition()?)
}

pub fn register_subagent_novelist_planned_profile(
    registry: &mut InMemoryRunProfileRegistry,
) -> Result<(), RunProfileRegistryError> {
    registry.register(subagent_novelist_planned_profile_definition()?)
}

pub fn register_scheduled_trigger_planned_profile(
    registry: &mut InMemoryRunProfileRegistry,
) -> Result<(), RunProfileRegistryError> {
    registry.register(scheduled_trigger_planned_profile_definition()?)
}

pub fn default_planned_run_profile_resolver()
-> Result<InMemoryRunProfileResolver, RunProfileRegistryError> {
    let registry = default_planned_run_profile_registry()?;
    planned_run_profile_resolver_from_registry(registry)
}

pub(crate) fn default_planned_run_profile_registry()
-> Result<InMemoryRunProfileRegistry, RunProfileRegistryError> {
    let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
    register_default_planned_profile(&mut registry)?;
    register_subagent_planned_profile(&mut registry)?;
    register_subagent_novelist_planned_profile(&mut registry)?;
    register_scheduled_trigger_planned_profile(&mut registry)?;
    Ok(registry)
}

pub(crate) fn planned_run_profile_resolver_from_registry(
    registry: InMemoryRunProfileRegistry,
) -> Result<InMemoryRunProfileResolver, RunProfileRegistryError> {
    let implicit_default = planned_default_profile_id()
        .map_err(|reason| RunProfileRegistryError::InvalidProfile { reason })?;
    Ok(InMemoryRunProfileResolver::new_with_implicit_default(
        registry,
        implicit_default,
    ))
}

/// Finite native authority declared by the registered runner profiles.
/// Constructed only here; descriptor visibility cannot mint this permission.
pub struct RegisteredNativeCapabilityCeiling {
    spawn: bool,
    skill_activation: bool,
    result_read: bool,
}
impl RegisteredNativeCapabilityCeiling {
    pub fn permits_spawn(&self) -> bool {
        self.spawn
    }
    pub fn permits_skill_activation(&self) -> bool {
        self.skill_activation
    }
    pub fn permits_result_read(&self) -> bool {
        self.result_read
    }
}

pub async fn registered_native_capability_ceiling(
    run: &ironclaw_turns::run_profile::LoopRunContext,
) -> Result<RegisteredNativeCapabilityCeiling, ironclaw_loop_host::CapabilityResolveError> {
    use ironclaw_turns::RunProfileResolver;
    let registry = default_planned_run_profile_resolver().map_err(|_| {
        ironclaw_loop_host::CapabilityResolveError::unavailable(
            "registered native profile unavailable",
        )
    })?;
    let requested =
        ironclaw_turns::RunProfileRequest::new(run.resolved_run_profile.profile_id.as_str())
            .map_err(|_| {
                ironclaw_loop_host::CapabilityResolveError::unavailable("invalid native profile")
            })?;
    let expected = registry
        .resolve_run_profile(
            ironclaw_turns::RunProfileResolutionRequest::interactive_default()
                .with_requested_run_profile(requested),
        )
        .await
        .map_err(|_| {
            ironclaw_loop_host::CapabilityResolveError::unavailable("unknown native profile")
        })?;
    let actual = &run.resolved_run_profile;
    if actual.profile_version != expected.profile_version
        || actual.loop_driver != expected.loop_driver
        || actual.checkpoint_schema_id != expected.checkpoint_schema_id
        || actual.checkpoint_schema_version != expected.checkpoint_schema_version
        || actual.capability_surface_profile_id != expected.capability_surface_profile_id
    {
        return Err(ironclaw_loop_host::CapabilityResolveError::unavailable(
            "native profile identity mismatch",
        ));
    }
    // Explicit normal planned contract; scheduled/text-only profiles do not gain
    // these permissions simply because their port advertises a native tool.
    let permitted = matches!(
        actual.profile_id.as_str(),
        PLANNED_DEFAULT_PROFILE_ID | SUBAGENT_PLANNED_PROFILE_ID | SUBAGENT_NOVELIST_PROFILE_ID
    );
    Ok(RegisteredNativeCapabilityCeiling {
        spawn: permitted,
        skill_activation: permitted,
        result_read: permitted,
    })
}

#[cfg(test)]
mod tests {
    use ironclaw_turns::{
        RunProfileRequest, RunProfileResolutionRequest, RunProfileResolver,
        run_profile::{LoopDriverId, PersonalContextAuthority, PersonalContextPolicy},
    };

    use super::*;
    use crate::app_loop_family::build_loop_family_registry;

    #[test]
    fn descriptor_carries_checkpoint_schema() {
        let descriptor = planned_driver_descriptor().expect("static descriptor should validate");

        assert_eq!(descriptor.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
        assert_eq!(descriptor.version, planned_driver_default_version());
        assert_eq!(
            descriptor
                .checkpoint_schema_id
                .as_ref()
                .map(|id| id.as_str()),
            Some(PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID)
        );
        assert_eq!(
            descriptor.checkpoint_schema_version,
            Some(planned_driver_checkpoint_schema_version())
        );
    }

    #[test]
    fn register_default_planned_driver_uses_v1_schema() {
        let mut registry = DriverRegistry::new();
        let key = register_default_planned_driver(
            &mut registry,
            build_loop_family_registry().expect("family registry should build"),
        )
        .expect("planned driver should register");

        assert_eq!(key.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
        assert_eq!(key.version, planned_driver_default_version());
        assert_eq!(
            key.checkpoint_schema_id.as_ref().map(|id| id.as_str()),
            Some(PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID)
        );
        assert_eq!(
            key.checkpoint_schema_version,
            Some(planned_driver_checkpoint_schema_version())
        );
    }

    #[test]
    fn register_subagent_planned_driver_uses_subagent_identity() {
        let mut registry = DriverRegistry::new();
        let key = register_subagent_planned_driver(
            &mut registry,
            build_loop_family_registry().expect("family registry should build"),
        )
        .expect("subagent planned driver should register");

        assert_eq!(key.id.as_str(), SUBAGENT_PLANNED_DRIVER_ID);
        assert_eq!(key.version, planned_driver_default_version());
        assert_eq!(
            key.checkpoint_schema_id.as_ref().map(|id| id.as_str()),
            Some(PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID)
        );
    }

    #[test]
    fn key_collision_with_textonly_is_impossible() {
        let mut registry = DriverRegistry::new();
        let text_only_key = register_default_text_only_driver(
            &mut registry,
            TextOnlyModelReplyDriverConfig::default(),
        )
        .expect("text-only driver should register");
        let planned_key = register_default_planned_driver(
            &mut registry,
            build_loop_family_registry().expect("family registry should build"),
        )
        .expect("planned driver should register");

        assert_ne!(text_only_key, planned_key);
        assert_eq!(text_only_key.id.as_str(), "reborn:text-only-model-reply");
        assert_eq!(planned_key.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
    }

    #[test]
    fn default_and_subagent_planned_driver_keys_do_not_collide() {
        let mut registry = DriverRegistry::new();
        let families = build_loop_family_registry().expect("family registry should build");
        let default_key = register_default_planned_driver(&mut registry, Arc::clone(&families))
            .expect("default planned driver should register");
        let subagent_key = register_subagent_planned_driver(&mut registry, families)
            .expect("subagent planned driver should register");

        assert_ne!(default_key, subagent_key);
        assert_eq!(default_key.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
        assert_eq!(subagent_key.id.as_str(), SUBAGENT_PLANNED_DRIVER_ID);
    }

    #[tokio::test]
    async fn profile_resolves_to_planned_driver() {
        let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
        register_default_planned_profile(&mut registry).expect("profile should register");
        let resolver = InMemoryRunProfileResolver::new(registry);
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default().with_requested_run_profile(
                    RunProfileRequest::new(PLANNED_DEFAULT_PROFILE_ID).unwrap(),
                ),
            )
            .await
            .expect("profile should resolve");

        assert_eq!(snapshot.profile_id.as_str(), PLANNED_DEFAULT_PROFILE_ID);
        assert_eq!(snapshot.loop_driver.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
        assert_eq!(
            snapshot.personal_context_policy,
            PersonalContextPolicy::Excluded
        );
        assert_eq!(
            snapshot
                .loop_driver
                .checkpoint_schema_id
                .as_ref()
                .map(|id| id.as_str()),
            Some(PLANNED_DRIVER_CHECKPOINT_SCHEMA_ID)
        );
    }

    #[tokio::test]
    async fn subagent_profile_resolves_to_subagent_planned_driver() {
        let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
        register_subagent_planned_profile(&mut registry).expect("profile should register");
        let resolver = InMemoryRunProfileResolver::new(registry);
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default().with_requested_run_profile(
                    RunProfileRequest::new(SUBAGENT_PLANNED_PROFILE_ID).unwrap(),
                ),
            )
            .await
            .expect("profile should resolve");

        assert_eq!(snapshot.profile_id.as_str(), SUBAGENT_PLANNED_PROFILE_ID);
        assert_eq!(snapshot.loop_driver.id.as_str(), SUBAGENT_PLANNED_DRIVER_ID);
        assert_eq!(
            snapshot.capability_surface_profile_id.as_str(),
            SUBAGENT_CAPABILITY_SURFACE_PROFILE_ID
        );
    }

    #[tokio::test]
    async fn novelist_profile_resolves_to_mission_model_slot() {
        // Dual-model: the novelist subagent's dedicated profile must bind the
        // `mission_model` profile so the writer run routes to ModelSlot::Mission
        // (the dedicated cheaper provider, e.g. minimax) while the parent
        // orchestrator stays on the primary model.
        let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
        register_subagent_novelist_planned_profile(&mut registry)
            .expect("novelist profile should register");
        let resolver = InMemoryRunProfileResolver::new(registry);
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default().with_requested_run_profile(
                    RunProfileRequest::new(SUBAGENT_NOVELIST_PROFILE_ID).unwrap(),
                ),
            )
            .await
            .expect("novelist profile should resolve");

        assert_eq!(snapshot.profile_id.as_str(), SUBAGENT_NOVELIST_PROFILE_ID);
        assert_eq!(snapshot.model_profile_id.as_str(), MISSION_MODEL_PROFILE_ID);
        assert_eq!(snapshot.loop_driver.id.as_str(), SUBAGENT_PLANNED_DRIVER_ID);
        assert_eq!(
            snapshot.capability_surface_profile_id.as_str(),
            SUBAGENT_CAPABILITY_SURFACE_PROFILE_ID
        );
        // ISSUE-IRONCLAW-011: model slot 不得改变子运行身份；该判定还守 SOUL 包装与工具交集。
        assert!(is_subagent_planned_profile(&snapshot));
    }

    #[tokio::test]
    async fn scheduled_trigger_profile_resolves_with_denied_surface_id() {
        // Issue #5505: a scheduled-trigger fire must resolve a dedicated
        // profile (not ProfileUnavailable) that reuses the default planned
        // driver but carries a distinct capability_surface_profile_id so the
        // host deny-map (runtime.rs) can strip the trigger mutators.
        let resolver =
            default_planned_run_profile_resolver().expect("planned resolver should build");
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default().with_requested_run_profile(
                    RunProfileRequest::new(RunProfileId::scheduled_trigger().as_str()).unwrap(),
                ),
            )
            .await
            .expect("scheduled_trigger profile should resolve");

        assert_eq!(
            snapshot.profile_id.as_str(),
            RunProfileId::scheduled_trigger().as_str()
        );
        assert_eq!(snapshot.loop_driver.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
        assert_eq!(
            snapshot.capability_surface_profile_id.as_str(),
            SCHEDULED_TRIGGER_CAPABILITY_SURFACE_PROFILE_ID
        );
    }

    #[tokio::test]
    async fn subagent_predicate_requires_profile_and_driver_identity() {
        let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
        register_subagent_planned_profile(&mut registry).expect("profile should register");
        let resolver = InMemoryRunProfileResolver::new(registry);
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default().with_requested_run_profile(
                    RunProfileRequest::new(SUBAGENT_PLANNED_PROFILE_ID).unwrap(),
                ),
            )
            .await
            .expect("profile should resolve");

        assert!(is_subagent_planned_profile(&snapshot));

        let mut alternate_model_profile = snapshot.clone();
        alternate_model_profile.profile_id =
            RunProfileId::new("reborn-planned-alternate-model").expect("valid profile id");
        assert!(
            is_subagent_planned_profile(&alternate_model_profile),
            "model/profile selection must not remove subagent prompt or permissions"
        );

        let mut mismatched_surface = snapshot.clone();
        mismatched_surface.capability_surface_profile_id =
            CapabilitySurfaceProfileId::new(INTERACTIVE_CAPABILITY_SURFACE_PROFILE_ID)
                .expect("valid capability surface profile id");
        assert!(!is_subagent_planned_profile(&mismatched_surface));

        let mut mismatched_driver = snapshot.clone();
        mismatched_driver.loop_driver.id =
            LoopDriverId::new(PLANNED_DRIVER_DEFAULT_ID).expect("valid test driver id");

        assert!(!is_subagent_planned_profile(&mismatched_driver));

        let mut mismatched_version = snapshot;
        mismatched_version.loop_driver.version = RunProfileVersion::new(99);

        assert!(!is_subagent_planned_profile(&mismatched_version));
    }

    #[tokio::test]
    async fn implicit_default_resolves_to_planned_driver() {
        let resolver =
            default_planned_run_profile_resolver().expect("planned resolver should build");
        let snapshot = resolver
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .expect("implicit default should resolve");

        assert_eq!(snapshot.profile_id.as_str(), PLANNED_DEFAULT_PROFILE_ID);
        assert_eq!(snapshot.loop_driver.id.as_str(), PLANNED_DRIVER_DEFAULT_ID);
        assert_eq!(
            snapshot.personal_context_policy,
            PersonalContextPolicy::Excluded
        );
    }

    #[tokio::test]
    async fn explicit_text_only_profile_still_resolves_textonly() {
        let resolver =
            default_planned_run_profile_resolver().expect("planned resolver should build");
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default().with_requested_run_profile(
                    RunProfileRequest::new("interactive_default").unwrap(),
                ),
            )
            .await
            .expect("explicit text-only profile should resolve");

        assert_eq!(
            snapshot.loop_driver.id,
            LoopDriverId::new("lightweight_loop").unwrap()
        );
    }

    #[tokio::test]
    async fn shared_authority_downgrades_allowed_to_excluded() {
        let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
        let allowed_profile = planned_default_profile_definition()
            .expect("profile should build")
            .with_personal_context_policy(PersonalContextPolicy::Allowed);
        registry
            .register(allowed_profile)
            .expect("profile should register");
        let resolver = InMemoryRunProfileResolver::new(registry);
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default()
                    .with_requested_run_profile(
                        RunProfileRequest::new(PLANNED_DEFAULT_PROFILE_ID).unwrap(),
                    )
                    .with_personal_context_authority(PersonalContextAuthority::Shared),
            )
            .await
            .expect("profile should resolve");
        assert_eq!(
            snapshot.personal_context_policy,
            PersonalContextPolicy::Excluded
        );
    }

    #[tokio::test]
    async fn direct_authority_preserves_allowed() {
        let mut registry = InMemoryRunProfileRegistry::with_builtin_profiles();
        let allowed_profile = planned_default_profile_definition()
            .expect("profile should build")
            .with_personal_context_policy(PersonalContextPolicy::Allowed);
        registry
            .register(allowed_profile)
            .expect("profile should register");
        let resolver = InMemoryRunProfileResolver::new(registry);
        let snapshot = resolver
            .resolve_run_profile(
                RunProfileResolutionRequest::interactive_default()
                    .with_requested_run_profile(
                        RunProfileRequest::new(PLANNED_DEFAULT_PROFILE_ID).unwrap(),
                    )
                    .with_personal_context_authority(PersonalContextAuthority::Direct),
            )
            .await
            .expect("profile should resolve");
        assert_eq!(
            snapshot.personal_context_policy,
            PersonalContextPolicy::Allowed
        );
    }
}

#[cfg(test)]
#[path = "native_result_read_ceiling_tests.rs"]
mod native_result_read_ceiling_tests;
