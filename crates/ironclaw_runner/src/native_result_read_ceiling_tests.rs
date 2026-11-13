//! 注册profile封闭原生权限；不创建端口、存储、网络或模型。
use super::*;
use ironclaw_turns::{RunProfileRequest, RunProfileResolutionRequest, RunProfileResolver};

async fn context(profile: &str) -> ironclaw_turns::run_profile::LoopRunContext {
    let resolved = default_planned_run_profile_resolver()
        .unwrap()
        .resolve_run_profile(
            RunProfileResolutionRequest::interactive_default()
                .with_requested_run_profile(RunProfileRequest::new(profile).unwrap()),
        )
        .await
        .unwrap();
    ironclaw_turns::run_profile::LoopRunContext::new(
        ironclaw_turns::TurnScope::new(
            ironclaw_host_api::TenantId::new("native-read-ceiling").unwrap(),
            None,
            None,
            ironclaw_host_api::ThreadId::new("native-read-ceiling").unwrap(),
        ),
        ironclaw_turns::TurnId::new(),
        ironclaw_turns::TurnRunId::new(),
        resolved,
    )
}

#[tokio::test]
async fn result_read_permission_is_sealed_to_registered_planned_profiles() {
    for profile in [
        PLANNED_DEFAULT_PROFILE_ID,
        SUBAGENT_PLANNED_PROFILE_ID,
        SUBAGENT_NOVELIST_PROFILE_ID,
    ] {
        let run = context(profile).await;
        let ceiling = registered_native_capability_ceiling(&run).await.unwrap();
        assert!(ceiling.permits_result_read());
        assert!(ceiling.permits_spawn() && ceiling.permits_skill_activation());
    }
    let run = context(ironclaw_turns::RunProfileId::scheduled_trigger().as_str()).await;
    let ceiling = registered_native_capability_ceiling(&run).await.unwrap();
    assert!(!ceiling.permits_result_read());
    assert!(!ceiling.permits_spawn() && !ceiling.permits_skill_activation());
    // "default" is the normal planned alias, not the explicit text-only profile.
    let alias = context(ironclaw_turns::RunProfileId::default_profile().as_str()).await;
    assert_eq!(
        alias.resolved_run_profile.profile_id.as_str(),
        PLANNED_DEFAULT_PROFILE_ID
    );
    assert_eq!(
        alias.resolved_run_profile.loop_driver.id.as_str(),
        PLANNED_DRIVER_DEFAULT_ID
    );
    assert!(
        registered_native_capability_ceiling(&alias)
            .await
            .unwrap()
            .permits_result_read()
    );
    let text = context(ironclaw_turns::RunProfileId::interactive_default().as_str()).await;
    assert_eq!(
        text.resolved_run_profile.profile_id.as_str(),
        "interactive_default"
    );
    assert_eq!(
        text.resolved_run_profile.loop_driver.id.as_str(),
        "lightweight_loop"
    );
    assert!(
        registered_native_capability_ceiling(&text)
            .await
            .map_or(true, |c| !c.permits_result_read())
    );
}

#[tokio::test]
async fn result_read_permission_rejects_unknown_or_drifted_registered_identity() {
    let original = context(PLANNED_DEFAULT_PROFILE_ID).await;
    let mut bad = Vec::new();
    let mut run = original.clone();
    run.resolved_run_profile.profile_id =
        ironclaw_turns::RunProfileId::new("unknown-read-ceiling").unwrap();
    bad.push(run);
    let mut run = original.clone();
    run.resolved_run_profile.profile_version = ironclaw_turns::RunProfileVersion::new(999);
    bad.push(run);
    let mut run = original.clone();
    run.resolved_run_profile.loop_driver.id =
        ironclaw_turns::run_profile::LoopDriverId::new("unknown:read-driver").unwrap();
    bad.push(run);
    let mut run = original.clone();
    run.resolved_run_profile.checkpoint_schema_id =
        ironclaw_turns::run_profile::CheckpointSchemaId::new("unknown-read-checkpoint").unwrap();
    bad.push(run);
    let mut run = original.clone();
    run.resolved_run_profile.checkpoint_schema_version =
        ironclaw_turns::RunProfileVersion::new(999);
    bad.push(run);
    let mut run = original.clone();
    run.resolved_run_profile.capability_surface_profile_id =
        ironclaw_turns::run_profile::CapabilitySurfaceProfileId::new("unknown-read-surface")
            .unwrap();
    bad.push(run);
    for run in bad {
        assert!(registered_native_capability_ceiling(&run).await.is_err());
    }
}
