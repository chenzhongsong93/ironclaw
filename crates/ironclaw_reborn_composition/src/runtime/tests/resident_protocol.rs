//! Real runtime getter and owned Tokio handle controls. Fixtures are NOT production acceptance.
use super::*;
use crate::{RebornFacadeReadiness,RebornWorkerReadiness};
#[tokio::test]
async fn local_dev_runtime_keeps_operator_admission_unwired() {
    let root=tempfile::tempdir().unwrap();
    let runtime=runtime_fixture(root.path(),false).await;
    let local=runtime.services.local_runtime.as_ref().expect("local runtime");
    assert!(local.operator_admission.is_none());
    assert!(local.lifecycle_facade.is_none());
    runtime.shutdown().await.unwrap();
}
async fn runtime_fixture(root:&std::path::Path,trigger:bool)->crate::RebornRuntime {
    let gateway=Arc::new(RecordingGateway {reply:"unused-protocol-fixture".into(),requests:Arc::new(StdMutex::new(Vec::new()))});
    let mut input=RebornRuntimeInput::from_services(RebornBuildInput::local_dev("resident-owner",root.join("local-dev"))
        .with_runtime_policy(local_dev_runtime_policy()))
        .with_identity(RebornRuntimeIdentity {tenant_id:"resident-fixture-tenant".into(),agent_id:"resident-fixture-agent".into(),source_binding_id:"resident-source".into(),reply_target_binding_id:"resident-reply".into()})
        .with_model_gateway_override(gateway);
    if trigger {input=input.with_trigger_poller_settings(TriggerPollerSettings::enabled_with_tenant_scoped_authorizer_for_test());}
    build_reborn_runtime(input).await.unwrap()
}
// Private test metadata only: exercise the getter's conjunction with real worker handles.
// Never use this Local fixture to report current normal production readiness.
fn set_metadata_fixture(runtime:&mut crate::RebornRuntime) {
    runtime.services.readiness.profile=RebornCompositionProfile::Production;
    runtime.services.readiness.state=RebornReadinessState::ProductionValidated;
    runtime.services.readiness.facades=RebornFacadeReadiness {host_runtime:true,turn_coordinator:true,product_auth:true};
    runtime.services.readiness.diagnostics.clear();
}
#[tokio::test] async fn hosted_single_tenant_validated_snapshot_is_ready_while_execution_pending() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),false).await;
    runtime.services.readiness.profile=RebornCompositionProfile::HostedSingleTenant;
    runtime.services.readiness.state=RebornReadinessState::HostedSingleTenantValidated;
    runtime.services.readiness.facades=RebornFacadeReadiness {host_runtime:true,turn_coordinator:true,product_auth:true};
    runtime.services.readiness.workers=RebornWorkerReadiness {turn_runner:true,trigger_poller:false};
    runtime.services.readiness.diagnostics.clear();runtime.startup_execution=ironclaw_runner::startup_execution::StartupExecutionLatch::closed();
    let snapshot=runtime.requester_runtime_protocol_snapshot();assert!(snapshot.ready()&&!snapshot.unknown()&&snapshot.execution_pending());
    runtime.startup_execution.release();runtime.shutdown().await.unwrap();
}
#[tokio::test] async fn runtime_snapshot_uses_real_validated_scope_and_explicit_disabled_trigger() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),false).await;
    let local=runtime.requester_runtime_protocol_snapshot();assert!(!local.ready()&&local.unknown());
    assert_eq!(local.tenant_id().as_str(),"resident-fixture-tenant");
    set_metadata_fixture(&mut runtime);let snapshot=runtime.requester_runtime_protocol_snapshot();assert!(snapshot.ready()&&!snapshot.unknown());
    let value=serde_json::to_value(snapshot).unwrap();assert_eq!(value["tenantId"],"resident-fixture-tenant");assert_eq!(value["protocol"],"network-policy-deny-redirect/1");
    assert_eq!(value["turnRunner"],"running");assert_eq!(value["triggerPoller"],"notConfigured");assert_eq!(value["triggerConfigured"],false);
    for forbidden in ["pid","native","source","reason","secret","configuration"] {assert!(value.get(forbidden).is_none());}
    runtime.shutdown().await.unwrap();
}
#[tokio::test] async fn real_scheduler_lock_contention_is_unknown_then_recovers_and_missing_is_rejected() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),false).await;set_metadata_fixture(&mut runtime);
    assert!(runtime.requester_runtime_protocol_snapshot().ready());
    {let _guard=runtime.turn_scheduler.handle_mutex().lock().await;assert!(!runtime.turn_scheduler.is_stopped());
        let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["turnRunner"],"unknown");assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);}
    assert!(runtime.requester_runtime_protocol_snapshot().ready());
    let scheduler=runtime.turn_scheduler.handle_mutex().lock().await.take().unwrap();scheduler.shutdown().await;
    let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["turnRunner"],"missing");assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);
    runtime.shutdown().await.unwrap();
}
#[tokio::test] async fn actual_trigger_cancellation_and_enabled_missing_handle_are_unqualified() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),true).await;set_metadata_fixture(&mut runtime);
    assert!(runtime.requester_runtime_protocol_snapshot().ready());
    runtime.trigger_poller_handle.as_ref().unwrap().cancel_for_resident_test();
    let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["triggerPoller"],"cancelled");assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);
    let original=runtime.trigger_poller_handle.take().unwrap();original.shutdown(Duration::from_secs(1)).await;
    let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["triggerPoller"],"missing");assert_eq!(v["triggerConfigured"],true);assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);
    runtime.shutdown().await.unwrap();
}
#[tokio::test] async fn real_scheduler_shutdown_and_declared_flag_drift_cannot_claim_ready() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),false).await;set_metadata_fixture(&mut runtime);
    runtime.services.readiness.workers.turn_runner=false;
    assert!(!runtime.requester_runtime_protocol_snapshot().ready());assert!(runtime.requester_runtime_protocol_snapshot().unknown());
    runtime.services.readiness.workers=RebornWorkerReadiness {turn_runner:true,trigger_poller:false};
    runtime.turn_scheduler.stop_for_test().await;
    let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["turnRunner"],"cancelled");assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);
    runtime.shutdown().await.unwrap();
}

#[tokio::test] async fn actual_readiness_blocking_unknown_diagnostic_is_redacted_and_rejected() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),false).await;set_metadata_fixture(&mut runtime);
    assert!(runtime.requester_runtime_protocol_snapshot().ready());
    let mut diagnostic=crate::RebornReadinessDiagnostic::disabled();diagnostic.blocks_production=true;
    diagnostic.status=crate::RebornReadinessDiagnosticStatus::Unknown("private-host-path-config-value".into());
    runtime.services.readiness.diagnostics.push(diagnostic);
    let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["blockingDiagnostics"],1);assert_eq!(v["unknownDiagnostics"],1);assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);
    assert!(!serde_json::to_string(&v).unwrap().contains("private-host-path-config-value"));runtime.shutdown().await.unwrap();
}

#[tokio::test] async fn actual_runtime_getter_rejects_a_finished_original_trigger_handle() {
    let root=tempfile::tempdir().unwrap();let mut runtime=runtime_fixture(root.path(),true).await;set_metadata_fixture(&mut runtime);
    let original=runtime.trigger_poller_handle.take().unwrap();original.shutdown(Duration::from_secs(1)).await;
    runtime.trigger_poller_handle=Some(crate::automation::trigger_poller::TriggerPollerRuntimeHandle::finished_for_resident_test().await);
    let v=serde_json::to_value(runtime.requester_runtime_protocol_snapshot()).unwrap();assert_eq!(v["triggerPoller"],"finished");assert_eq!(v["ready"],false);assert_eq!(v["unknown"],true);
    runtime.shutdown().await.unwrap();
}

#[tokio::test] async fn actual_runtime_frame_is_bounded_serde_original_and_unique_per_runtime() {
    let root=tempfile::tempdir().unwrap();let runtime=runtime_fixture(root.path(),false).await;
    let id=runtime.requester_runtime_protocol_snapshot().runtime_instance_id().to_owned();assert!(uuid::Uuid::parse_str(&id).is_ok());
    let mut original=Vec::new();runtime.write_requester_runtime_protocol_frame(&mut original).unwrap();
    let mut fields=original.splitn(3,|b|*b==b' ');assert_eq!(fields.next().unwrap(),b"IRONCLAW_RESIDENT_PROTOCOL_V2");
    let declared:usize=std::str::from_utf8(fields.next().unwrap()).unwrap().parse().unwrap();let payload_with_newline=fields.next().unwrap();
    assert_eq!(payload_with_newline.last(),Some(&b'\n'));let payload=&payload_with_newline[..payload_with_newline.len()-1];assert_eq!(declared,payload.len());assert!(declared<=16*1024);
    let body:serde_json::Value=serde_json::from_slice(payload).unwrap();assert_eq!(body["schema"],"reborn.requester_runtime_protocol/2");assert_eq!(body["runtimeInstanceId"],id);assert!(body.get("pid").is_none());
    let second_root=tempfile::tempdir().unwrap();let second=runtime_fixture(second_root.path(),false).await;
    assert_ne!(id,second.requester_runtime_protocol_snapshot().runtime_instance_id());second.shutdown().await.unwrap();runtime.shutdown().await.unwrap();
}
#[tokio::test] async fn actual_runtime_frame_flush_failure_returns_safe_fixed_error() {
    struct FailFlush(Vec<u8>);
    impl std::io::Write for FailFlush {fn write(&mut self,b:&[u8])->std::io::Result<usize>{self.0.extend_from_slice(b);Ok(b.len())}
        fn flush(&mut self)->std::io::Result<()>{Err(std::io::Error::other("private-host-path-config-value"))}}
    let root=tempfile::tempdir().unwrap();let runtime=runtime_fixture(root.path(),false).await;let mut output=FailFlush(Vec::new());
    assert_eq!(runtime.write_requester_runtime_protocol_frame(&mut output).unwrap_err(),"residentSnapshotFrameFlushFailed");runtime.shutdown().await.unwrap();
}

async fn late_fixture(root:&std::path::Path,restore:bool,selected_provider:&str)->(crate::RebornRuntime,tempfile::TempDir,super::super::late_admission::LateAdmissionPorts,Arc<ironclaw_extensions::SharedExtensionRegistry>,crate::extension_host::available_extensions::AvailableExtensionPackage) {
    let input=RebornRuntimeInput::from_services(RebornBuildInput::local_dev("lifecycle-owner",root.join("runtime"))
        .with_runtime_policy(local_dev_runtime_policy())).with_identity(RebornRuntimeIdentity {tenant_id:"resident-fixture-tenant".into(),agent_id:"resident-fixture-agent".into(),source_binding_id:"resident-source".into(),reply_target_binding_id:"resident-reply".into()})
        .with_model_gateway_override(Arc::new(RecordingGateway {reply:"unused".into(),requests:Arc::new(StdMutex::new(Vec::new()))}));
    let mut runtime=build_reborn_runtime(input).await.unwrap();set_metadata_fixture(&mut runtime);
    let store=Arc::new(crate::operator_late_admission::OperatorLateAdmissionStore::default());
    let signer=ironclaw_mcp::McpTrustedContextSigner::new(ironclaw_host_api::ExtensionId::new(selected_provider).unwrap(),b"unit-fixture-not-a-qualified-operator-proof".to_vec(),"http://127.0.0.1:39999/mcp/internal".into()).unwrap();
    store.set_actual_signer(&signer).unwrap();store.bind_runtime(runtime.requester_runtime_protocol_snapshot().runtime_instance_id(),runtime.resident_admission_observer()).unwrap();
    let (dir,manager,facade,registry,available)=crate::extension_host::extension_lifecycle::tests::late_runtime_mechanism_ports(store.clone(),restore);
    let ports=super::super::late_admission::LateAdmissionPorts {store,manager,facade};runtime.late_admission_fixture=Some(ports.clone());
    (runtime,dir,ports,registry,available)
}
fn fixture_bundle(runtime:&crate::RebornRuntime,available:crate::extension_host::available_extensions::AvailableExtensionPackage)->crate::RebornHostExtensionBundle {
    let current=runtime.requester_runtime_protocol_snapshot();crate::RebornHostExtensionBundle::late_mechanism_fixture(available,current.tenant_id().clone(),current.runtime_instance_id())
}
#[tokio::test] async fn high_facade_first_admit_and_existing_restore_normal_lifecycle_then_remove_rejects_dispatch() {
    use ironclaw_product_workflow::{LifecycleProductAction,LifecycleProductContext,LifecycleProductSurfaceContext,LifecyclePackageKind,LifecyclePackageRef,LifecyclePhase};
    for restore in [false,true] {
        let root=tempfile::tempdir().unwrap();let (runtime,_catalog_root,ports,registry,available)=late_fixture(root.path(),restore,"fixture").await;
        let provider=ironclaw_host_api::ExtensionId::new("fixture").unwrap();assert!(ports.store.requires_admission(&provider));assert!(ports.store.binding(&provider).is_none());
        let context=LifecycleProductContext::Surface(LifecycleProductSurfaceContext {tenant_id:runtime.thread_scope.tenant_id.clone(),user_id:runtime.actor_user_id.clone(),agent_id:Some(runtime.thread_scope.agent_id.clone()),project_id:None});
        let reference=LifecyclePackageRef::new(LifecyclePackageKind::Extension,"fixture").unwrap();
        if restore {let installed=ports.facade.execute(context.clone(),LifecycleProductAction::ExtensionInstall {package_ref:reference.clone()}).await.unwrap();assert_eq!(installed.phase,LifecyclePhase::Installed);
            let active=ports.facade.execute(context.clone(),LifecycleProductAction::ExtensionActivate {package_ref:reference.clone()}).await.unwrap();assert_eq!(active.phase,LifecyclePhase::Active);
            assert!(registry.snapshot().get_extension(&provider).is_some());
        }
        assert!(ports.manager.active_model_visible_capabilities().await.unwrap().is_empty());
        let result=runtime.admit_startup_host_extension_bundle(fixture_bundle(&runtime,available)).await.unwrap();assert_eq!(result.last().unwrap().phase,LifecyclePhase::Active);
        assert!(registry.snapshot().get_extension(&provider).is_some());assert!(ports.store.binding(&provider).is_some());assert!(!ports.manager.active_model_visible_capabilities().await.unwrap().is_empty());
        let removed=ports.facade.execute(context,LifecycleProductAction::ExtensionRemove {package_ref:reference}).await.unwrap();assert_eq!(removed.phase,LifecyclePhase::Removed);
        assert!(registry.snapshot().get_extension(&provider).is_none());assert!(ports.manager.active_model_visible_capabilities().await.unwrap().is_empty());
        // Ledger may keep original file holds/active bit; real sharedRegistry removal remains decisive at dispatch.
        assert!(ports.store.binding(&provider).is_some());
        use ironclaw_mcp::McpHostHttpEgressPlanner;
        let planner=crate::extension_host::mcp::RegistryMcpEgressPlanner::with_late_mechanism_store(registry.clone(),ports.store.clone());
        let capability=ironclaw_host_api::CapabilityId::new("fixture.search").unwrap();let scope=ironclaw_host_api::ResourceScope {tenant_id:runtime.thread_scope.tenant_id.clone(),user_id:runtime.actor_user_id.clone(),agent_id:Some(runtime.thread_scope.agent_id.clone()),project_id:None,mission_id:None,thread_id:None,invocation_id:ironclaw_host_api::InvocationId::new()};
        let denied=planner.plan(ironclaw_mcp::McpHostHttpEgressPlanRequest {provider:&provider,capability_id:&capability,scope:&scope,transport:"http",method:ironclaw_host_api::NetworkMethod::Post,url:"http://127.0.0.1:39999/mcp/internal",headers:&[],body:&[]});
        assert!(denied.network_policy.allowed_targets.is_empty());
        runtime.shutdown().await.unwrap();assert!(ports.store.binding(&provider).is_none());
    }
}
#[tokio::test] async fn high_facade_foreign_tenant_wrong_instance_and_worker_stop_never_install() {
    for case in [0,1,2] {let root=tempfile::tempdir().unwrap();let (runtime,_dir,ports,registry,available)=late_fixture(root.path(),false,"fixture").await;
        let current=runtime.requester_runtime_protocol_snapshot();let tenant=if case==0{ironclaw_host_api::TenantId::new("foreign-tenant").unwrap()}else{current.tenant_id().clone()};
        let instance=if case==1{"22222222-2222-4222-8222-222222222222"}else{current.runtime_instance_id()};
        let bundle=crate::RebornHostExtensionBundle::late_mechanism_fixture(available,tenant,instance);
        if case==2{runtime.turn_scheduler.stop_for_test().await;}
        assert!(runtime.admit_startup_host_extension_bundle(bundle).await.is_err());assert!(registry.snapshot().get_extension(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());
        assert!(ports.manager.active_model_visible_capabilities().await.unwrap().is_empty());runtime.shutdown().await.unwrap();}
}
#[tokio::test] async fn high_facade_failed_nonterminal_or_blocked_install_never_activates() {
    use ironclaw_product_workflow::{LifecycleProductFacade,LifecycleProductAction,LifecycleProductContext,LifecycleProductResponse,LifecyclePackageRef,LifecyclePhase,LifecycleReadinessBlocker};
    struct InstallPhase {phase:LifecyclePhase,blocked:bool,activates:std::sync::atomic::AtomicUsize,normal:Arc<dyn LifecycleProductFacade>}
    #[async_trait::async_trait] impl LifecycleProductFacade for InstallPhase {
        async fn execute(&self,_:LifecycleProductContext,action:LifecycleProductAction)->Result<LifecycleProductResponse,ironclaw_product_workflow::ProductWorkflowError> {
            match action {LifecycleProductAction::ExtensionInstall {package_ref}=>Ok(LifecycleProductResponse {package_ref:Some(package_ref),phase:self.phase.clone(),blockers:if self.blocked{vec![LifecycleReadinessBlocker::Runtime {ref_id:None}]}else{vec![]},message:None,
                    payload:Some(ironclaw_product_workflow::LifecycleProductPayload::ExtensionInstall {installed:true,visible_capability_ids:vec![],next_step:"fixture".into()})}),
                LifecycleProductAction::ExtensionActivate {..}=>{self.activates.fetch_add(1,Ordering::SeqCst);Err(ironclaw_product_workflow::ProductWorkflowError::BindingAccessDenied)},_=>Err(ironclaw_product_workflow::ProductWorkflowError::BindingAccessDenied)}
        }
        async fn project_package(&self,context:LifecycleProductContext,reference:LifecyclePackageRef)->Result<LifecycleProductResponse,ironclaw_product_workflow::ProductWorkflowError>{self.normal.project_package(context,reference).await}
    }
    for (phase,blocked) in [(LifecyclePhase::Failed,false),(LifecyclePhase::Installing,false),(LifecyclePhase::Installed,true)] {
        let root=tempfile::tempdir().unwrap();let (mut runtime,_dir,mut ports,registry,available)=late_fixture(root.path(),false,"fixture").await;
        let fake=Arc::new(InstallPhase {phase,blocked,activates:std::sync::atomic::AtomicUsize::new(0),normal:ports.facade.clone()});ports.facade=fake.clone();runtime.late_admission_fixture=Some(ports.clone());
        assert!(runtime.admit_startup_host_extension_bundle(fixture_bundle(&runtime,available)).await.is_err());assert_eq!(fake.activates.load(Ordering::SeqCst),0);
        assert!(ports.store.binding(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());assert!(registry.snapshot().get_extension(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());runtime.shutdown().await.unwrap();
    }
}

#[tokio::test] async fn unrelated_ordinary_http_keeps_normal_owner_filtered_surface() {
    use ironclaw_product_workflow::{LifecycleProductAction,LifecycleProductContext,LifecycleProductSurfaceContext,LifecyclePackageKind,LifecyclePackageRef,LifecyclePhase};
    let root=tempfile::tempdir().unwrap();let (runtime,_dir,ports,_registry,available)=late_fixture(root.path(),false,"root-bound-provider").await;
    let provider=ironclaw_host_api::ExtensionId::new("fixture").unwrap();assert!(!ports.store.requires_admission(&provider));
    ports.manager.revalidate_root_bundle(&fixture_bundle(&runtime,available)).await.unwrap();
    let context=LifecycleProductContext::Surface(LifecycleProductSurfaceContext {tenant_id:runtime.thread_scope.tenant_id.clone(),user_id:runtime.actor_user_id.clone(),agent_id:Some(runtime.thread_scope.agent_id.clone()),project_id:None});
    let reference=LifecyclePackageRef::new(LifecyclePackageKind::Extension,"fixture").unwrap();
    ports.facade.execute(context.clone(),LifecycleProductAction::ExtensionInstall {package_ref:reference.clone()}).await.unwrap();
    let active=ports.facade.execute(context,LifecycleProductAction::ExtensionActivate {package_ref:reference}).await.unwrap();assert_eq!(active.phase,LifecyclePhase::Active);
    assert!(!ports.manager.active_model_visible_capabilities().await.unwrap().is_empty());runtime.shutdown().await.unwrap();
}

#[tokio::test] async fn duplicate_actual_signer_setter_discovery_and_reserve_do_not_form_a_lock_cycle() {
    let root=tempfile::tempdir().unwrap();let (runtime,_dir,ports,_registry,available)=late_fixture(root.path(),false,"fixture").await;
    let current=runtime.requester_runtime_protocol_snapshot();let instance=current.runtime_instance_id().to_owned();let tenant=current.tenant_id().clone();
    let store=ports.store.clone();let signer=ironclaw_mcp::McpTrustedContextSigner::new(ironclaw_host_api::ExtensionId::new("fixture").unwrap(),b"unit-fixture-not-qualified".to_vec(),"http://127.0.0.1:39999/mcp/internal".into()).unwrap();
    let discovery_store=store.clone();let provider=ironclaw_host_api::ExtensionId::new("fixture").unwrap();
    let discovery=tokio::task::spawn_blocking(move||{for _ in 0..1000{let _=discovery_store.discovery(&provider);}});
    let setter_store=store.clone();let setter=tokio::task::spawn_blocking(move||{for _ in 0..1000{assert!(setter_store.set_actual_signer(&signer).is_err());}});
    let bundle=Arc::new(fixture_bundle(&runtime,available));let gate=runtime.resident_admission_observer();
    let reserver=tokio::task::spawn_blocking(move||store.reserve(bundle,&tenant,&instance,gate));
    tokio::time::timeout(Duration::from_secs(2),async {discovery.await.unwrap();setter.await.unwrap();reserver.await.unwrap().unwrap();}).await.expect("no signer/state lock cycle");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn high_startup_admit_releases_only_success_and_stopped_pending_denies() {
    let root=tempfile::tempdir().unwrap();
    let (mut runtime,_dir,ports,_registry,available)=late_fixture(root.path(),false,"fixture").await;
    runtime.startup_execution=ironclaw_runner::startup_execution::StartupExecutionLatch::closed();
    assert!(runtime.requester_runtime_protocol_snapshot().execution_pending());
    runtime.admit_startup_host_extension_bundle_before(fixture_bundle(&runtime,available),tokio::time::Instant::now()+std::time::Duration::from_secs(1)).await.unwrap();
    assert!(!runtime.requester_runtime_protocol_snapshot().execution_pending());runtime.shutdown().await.unwrap();
    let root=tempfile::tempdir().unwrap();
    let (mut runtime,_dir,ports,_registry,available)=late_fixture(root.path(),false,"fixture").await;
    runtime.startup_execution=ironclaw_runner::startup_execution::StartupExecutionLatch::closed();
    let bundle=fixture_bundle(&runtime,available);
    runtime.request_private_startup_admission_stop();
    assert!(runtime.admit_startup_host_extension_bundle(bundle).await.is_err());
    assert!(runtime.startup_execution.pending());assert!(runtime.startup_execution.stopped());
    assert!(ports.store.binding(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn high_startup_past_deadline_never_installs_or_releases() {
    let root=tempfile::tempdir().unwrap();let (mut runtime,_dir,ports,registry,available)=late_fixture(root.path(),false,"fixture").await;
    runtime.startup_execution=ironclaw_runner::startup_execution::StartupExecutionLatch::closed();
    let bundle=fixture_bundle(&runtime,available);
    assert!(runtime.admit_startup_host_extension_bundle_before(bundle,tokio::time::Instant::now()).await.is_err());
    assert!(runtime.startup_execution.pending());assert!(ports.store.binding(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());
    assert!(registry.snapshot().get_extension(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn high_startup_actual_normal_admit_crosses_deadline_keeps_execution_closed() {
    use ironclaw_product_workflow::{LifecycleProductFacade,LifecycleProductContext,LifecycleProductAction,LifecycleProductResponse,LifecyclePackageRef,ProductWorkflowError};
    struct DelayedNormal {inner:Arc<dyn LifecycleProductFacade>}
    #[async_trait::async_trait] impl LifecycleProductFacade for DelayedNormal {
        async fn execute(&self,c:LifecycleProductContext,a:LifecycleProductAction)->Result<LifecycleProductResponse,ProductWorkflowError>{
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;self.inner.execute(c,a).await
        }
        async fn project_package(&self,c:LifecycleProductContext,p:LifecyclePackageRef)->Result<LifecycleProductResponse,ProductWorkflowError>{self.inner.project_package(c,p).await}
    }
    let root=tempfile::tempdir().unwrap();let (mut runtime,_dir,mut ports,_registry,available)=late_fixture(root.path(),false,"fixture").await;
    runtime.startup_execution=ironclaw_runner::startup_execution::StartupExecutionLatch::closed();
    ports.facade=Arc::new(DelayedNormal {inner:ports.facade.clone()});runtime.late_admission_fixture=Some(ports.clone());
    let result=runtime.admit_startup_host_extension_bundle_before(fixture_bundle(&runtime,available),tokio::time::Instant::now()+std::time::Duration::from_millis(1)).await;
    assert!(result.is_err());assert!(runtime.startup_execution.pending());assert!(runtime.startup_execution.stopped());
    assert!(ports.store.binding(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_none());runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn high_facade_real_filesystem_restart_resumes_without_reinstall_and_public_repeat_stays_rejected() {
    use ironclaw_product_workflow::{LifecycleProductAction,LifecycleProductContext,LifecycleProductSurfaceContext,LifecyclePackageKind,LifecyclePackageRef,LifecyclePhase};
    let root=tempfile::tempdir().unwrap();
    let (mut first,_first_dir,_ports,_registry,_available)=late_fixture(root.path(),false,"fixture").await;
    let first_store=Arc::new(crate::operator_late_admission::OperatorLateAdmissionStore::default());
    let signer=ironclaw_mcp::McpTrustedContextSigner::new(ironclaw_host_api::ExtensionId::new("fixture").unwrap(),b"mechanism-only-persisted-restart-signer".to_vec(),"http://127.0.0.1:39999/mcp/internal".into()).unwrap();
    first_store.set_actual_signer(&signer).unwrap();first_store.bind_runtime(first.requester_runtime_protocol_snapshot().runtime_instance_id(),first.resident_admission_observer()).unwrap();
    let (_disk_owner,filesystem,manager,facade,registry,available)=crate::extension_host::extension_lifecycle::tests::late_runtime_persistent_ports(first_store.clone(),None).await;
    first.late_admission_fixture=Some(super::super::late_admission::LateAdmissionPorts {store:first_store,manager,facade:facade.clone()});
    first.admit_startup_host_extension_bundle(fixture_bundle(&first,available)).await.unwrap();
    let context=LifecycleProductContext::Surface(LifecycleProductSurfaceContext {tenant_id:first.thread_scope.tenant_id.clone(),user_id:first.actor_user_id.clone(),agent_id:Some(first.thread_scope.agent_id.clone()),project_id:None});
    let reference=LifecyclePackageRef::new(LifecyclePackageKind::Extension,"fixture").unwrap();
    assert!(facade.execute(context.clone(),LifecycleProductAction::ExtensionInstall {package_ref:reference.clone()}).await.is_err());
    assert!(registry.snapshot().get_extension(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_some());
    let first_instance=first.runtime_instance_id;first.shutdown().await.unwrap();drop(facade);
    let second_root=tempfile::tempdir().unwrap();let (mut second,_second_dir,_unused,_unused_registry,_unused_available)=late_fixture(second_root.path(),false,"fixture").await;
    assert_ne!(first_instance,second.runtime_instance_id);
    let second_store=Arc::new(crate::operator_late_admission::OperatorLateAdmissionStore::default());
    second_store.set_actual_signer(&signer).unwrap();second_store.bind_runtime(second.requester_runtime_protocol_snapshot().runtime_instance_id(),second.resident_admission_observer()).unwrap();
    let (_other_disk_owner,_same_filesystem,manager,facade,registry,available)=crate::extension_host::extension_lifecycle::tests::late_runtime_persistent_ports(second_store.clone(),Some(filesystem)).await;
    second.late_admission_fixture=Some(super::super::late_admission::LateAdmissionPorts {store:second_store.clone(),manager,facade:facade.clone()});
    let before=facade.project_package(context.clone(),reference.clone()).await.unwrap();assert_eq!(before.phase,LifecyclePhase::Active);
    second.admit_startup_host_extension_bundle(fixture_bundle(&second,available)).await.unwrap();
    assert!(registry.snapshot().get_extension(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_some());
    assert!(second_store.binding(&ironclaw_host_api::ExtensionId::new("fixture").unwrap()).is_some());
    assert!(facade.execute(context,LifecycleProductAction::ExtensionInstall {package_ref:reference}).await.is_err());
    second.shutdown().await.unwrap();
}
