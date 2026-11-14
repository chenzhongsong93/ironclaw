//! EXP-STORAGE-01：合成actor，无模型；真实coordinator/required callback/PG存储，不签normalactor/G01。
#![cfg(feature = "filesystem-goal-store")]
use std::sync::{Arc,atomic::{AtomicU8,Ordering}};
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem, Entry, CasExpectation};
use ironclaw_host_api::{AgentId,ProjectId,TenantId,ThreadId,UserId,ResourceScope,MountAlias,MountGrant,MountPermissions,MountView,VirtualPath,ScopedPath};
use ironclaw_loop_host::{AwaitEdgeWriter,AwaitedChildSetRecord,SpawnSubagentMode,SubagentKindId};
use ironclaw_runner::subagent::await_edge::{resolver::AwaitEdgeResolver,store::FilesystemAwaitEdgeStore};
use ironclaw_runner::subagent::goal_store::{InMemoryBoundedSubagentGoalStore,SubagentGoal};
use ironclaw_threads::{InMemorySessionThreadService,SessionThreadService,ThreadScope};
use ironclaw_turns::{DefaultTurnCoordinator,DefaultTurnLifecycleEventBus,LifecyclePublicationErrorPort,LifecyclePublishingTurnStateStore,TurnCommittedEventObserver,TurnCoordinator,TurnLifecycleEventBus,TurnEventSink,TurnRunId,TurnScope,TurnSpawnTreePort,TurnStatus};
use ironclaw_turns::runner::TurnRunTransitionPort;
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
type TestResult<T> = Result<T,Box<dyn std::error::Error+Send+Sync>>;
const RAW_CANARY:&str="synthetic exporter storage canary; no normal actor or model";
const PROJECT:&str="synthetic-exporter-project";
const LAYER:&str="world:static";

#[derive(Clone,Copy,PartialEq,Eq)]
enum FailedCallbackExpectation{Persisted,Rejected}
#[derive(Clone,Copy,PartialEq,Eq)]
enum HostTerminalKind{Failed,Cancelled}

// 每op从真实ResourceScope产生typed动态目标；非固定tenant/view，不声称composition完整catalog。
fn fixture_mounts(scope:&ResourceScope)->Result<MountView,ironclaw_host_api::HostApiError>{
    // roster touch/prune显式用system scope；只给其系统roster子树，不能拼保留sentinel到owner路径。
    if scope.is_system() {
        return MountView::new(vec![MountGrant::new(
            MountAlias::new("/turns/subagent-await-scopes")?,
            VirtualPath::new("/turns/subagent-await-scopes")?,
            MountPermissions::read_write_list_delete())]);
    }
    let agent=scope.agent_id.as_ref().map_or("__none__",|id|id.as_str());
    let project=scope.project_id.as_ref().map_or("__none__",|id|id.as_str());
    let owner_root=format!("/tenants/{}/users/{}/agents/{agent}/projects/{project}",scope.tenant_id.as_str(),scope.user_id.as_str());
    MountView::new(vec![
        MountGrant::new(MountAlias::new("/engine")?,VirtualPath::new(format!("{owner_root}/engine"))?,MountPermissions::read_write_list_delete()),
        MountGrant::new(MountAlias::new("/turns")?,VirtualPath::new(format!("{owner_root}/turns"))?,MountPermissions::read_write_list_delete()),
    ])
}
fn scope_with_project(tenant:&str,user:&str,project:&str,thread:&str)->TurnScope{
    TurnScope::new_with_owner(TenantId::new(tenant).unwrap(),Some(AgentId::new("synthetic-exporter-agent").unwrap()),
        Some(ProjectId::new(project).unwrap()),ThreadId::new(thread).unwrap(),Some(UserId::new(user).unwrap()))
}
fn scope(tenant:&str,user:&str,thread:&str)->TurnScope{scope_with_project(tenant,user,PROJECT,thread)}
async fn fixture_isolation(root:&InMemoryBackend,fs:&ScopedFilesystem<InMemoryBackend>)->TestResult<()> {
    let a=scope("synthetic-tenant-a","synthetic-user-a","probe-a").to_resource_scope();
    let b=scope("synthetic-tenant-b","synthetic-user-b","probe-b").to_resource_scope();
    for alias in ["/engine","/turns"] {
        let path=ScopedPath::new(format!("{alias}/exporter-storage-probe/canary"))?;
        fs.put(&a,&path,Entry::bytes(b"canary-a".to_vec()),CasExpectation::Absent).await?;
        fs.put(&b,&path,Entry::bytes(b"canary-b".to_vec()),CasExpectation::Absent).await?;
        let pa=fs.resolve(&a,&path)?;let pb=fs.resolve(&b,&path)?;
        assert!(pa!=pb,"fixture actual root paths must isolate tenant/user");
        use ironclaw_filesystem::RootFilesystem;
        assert!(root.get(&pa).await?.is_some_and(|v|v.entry.body==b"canary-a"),"fixture root a must retain its canary");
        assert!(root.get(&pb).await?.is_some_and(|v|v.entry.body==b"canary-b"),"fixture root b must retain its canary");
    }
    Ok(())
}
struct AllowResultWriter;
#[async_trait::async_trait]
impl ironclaw_loop_host::LoopCapabilityResultWriter for AllowResultWriter {
    async fn write_capability_result(&self,write:ironclaw_loop_host::CapabilityResultWrite<'_>)->Result<ironclaw_loop_host::CapabilityWriteResult,ironclaw_turns::run_profile::AgentLoopHostError>{
        Ok(ironclaw_loop_host::CapabilityWriteResult::without_output_digest(ironclaw_turns::LoopResultRef::new(format!("result:{}",write.capability_id)).unwrap(),0))
    }
    async fn update_capability_result(&self,_context:&ironclaw_turns::run_profile::LoopRunContext,_reference:&ironclaw_turns::LoopResultRef,output:Value)->Result<u64,ironclaw_turns::run_profile::AgentLoopHostError>{
        Ok(serde_json::to_vec(&output).map(|value|value.len() as u64).unwrap_or(0))
    }
}
#[derive(PartialEq,Eq)]
struct WireRow{parent:Option<String>,kind:String,project:Option<String>,layer:Option<String>,terminal:Option<String>,hash:Option<String>,failure:Option<String>,spawned:chrono::DateTime<chrono::Utc>}
async fn read_row(client:&tokio_postgres::Client,child:TurnRunId)->TestResult<WireRow>{
    let row=client.query_one("SELECT parent_run_id,subagent_type,project_id,layer,terminal_status,final_text_hash,failure_category,spawned_at FROM spawn_records WHERE child_run_id=$1",&[&child.to_string()]).await?;
    Ok(WireRow{parent:row.get(0),kind:row.get(1),project:row.get(2),layer:row.get(3),terminal:row.get(4),hash:row.get(5),failure:row.get(6),spawned:row.get(7)})
}
async fn run(client:&tokio_postgres::Client,phase:&AtomicU8)->TestResult<Value>{
    phase.store(1,Ordering::Relaxed); // Identity
    let identity=client.query_one("SELECT current_database(),current_schema(),current_user,obj_description(to_regnamespace(current_schema()),'pg_namespace')",&[]).await?;
    assert!(identity.get::<_,String>(0)==std::env::var("TQ_EXPORTER_DATABASE")?,"owned database identity required");
    assert!(identity.get::<_,String>(1)==std::env::var("TQ_EXPORTER_SCHEMA")?,"owned schema identity required");
    assert!(identity.get::<_,String>(2)==std::env::var("TQ_EXPORTER_USER")?,"owned role identity required");
    assert!(identity.get::<_,Option<String>>(3)==Some(std::env::var("TQ_EXPORTER_OWNERSHIP_COMMENT")?),"owned schema comment required");
    phase.store(2,Ordering::Relaxed); // EmptyWireTables
    let empty=client.query_one("SELECT (SELECT count(*) FROM spawn_records),(SELECT count(*) FROM tianquan_run_bindings)",&[]).await?;
    assert!(empty.get::<_,i64>(0)==0 && empty.get::<_,i64>(1)==0,"fresh thin wire tables required, never adopt previous records");
    phase.store(3,Ordering::Relaxed); // RawDirectory
    let raw_root=std::path::PathBuf::from(std::env::var("TIANQUAN_SPAWN_PROSE_DIR")?);
    assert!(raw_root.is_absolute() && raw_root.is_dir(),"Root explicit fresh raw directory required");
    assert!(std::fs::read_dir(&raw_root)?.next().is_none(),"fresh raw directory must be empty");

    phase.store(4,Ordering::Relaxed); // FixtureMounts
    let root=Arc::new(InMemoryBackend::new());let fs=Arc::new(ScopedFilesystem::new(root.clone(),fixture_mounts));
    phase.store(5,Ordering::Relaxed); // FixtureIsolation
    fixture_isolation(root.as_ref(),fs.as_ref()).await?;
    phase.store(6,Ordering::Relaxed); // Services
    let edges=Arc::new(FilesystemAwaitEdgeStore::new(fs));
    let base=Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let threads=Arc::new(InMemorySessionThreadService::default());
    let goals=Arc::new(InMemoryBoundedSubagentGoalStore::new());
    let goal_port:Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore>=goals.clone();
    let state_port:Arc<dyn ironclaw_turns::TurnSpawnTreeStateStore>=base.clone();
    let resolver=Arc::new(AwaitEdgeResolver::new_unbound(edges.clone(),goal_port,state_port,Arc::new(AllowResultWriter),threads.clone()));
    let bus=Arc::new(DefaultTurnLifecycleEventBus::new());
    phase.store(7,Ordering::Relaxed); // LifecycleSubscriptions
    bus.subscribe_required(resolver.clone() as Arc<dyn TurnCommittedEventObserver>)?;
    let captured=Arc::new(ironclaw_turns::InMemoryTurnEventSink::default());
    bus.subscribe_best_effort(captured.clone() as Arc<dyn TurnEventSink>)?;
    let published=Arc::new(LifecyclePublishingTurnStateStore::new(base.clone(),bus.clone()));
    let error_port:Arc<dyn LifecyclePublicationErrorPort>=published.clone();
    let coordinator=Arc::new(DefaultTurnCoordinator::new(published.clone()).with_lifecycle_publication_error_port(error_port));
    phase.store(8,Ordering::Relaxed); // BindCoordinator
    resolver.bind_coordinator(coordinator.clone() as Arc<dyn TurnCoordinator>)?;
    phase.store(9,Ordering::Relaxed); // Scopes
    let parent_scope=scope("synthetic-exporter-tenant","synthetic-exporter-user","synthetic-parent-thread");
    let child_scope=scope("synthetic-exporter-tenant","synthetic-exporter-user","synthetic-child-thread");
    let actor=ironclaw_turns::TurnActor::new(UserId::new("synthetic-exporter-user")?);
    phase.store(10,Ordering::Relaxed); // ParentSubmit
    let ironclaw_turns::SubmitTurnResponse::Accepted{run_id:parent,..}=coordinator.submit_turn(ironclaw_turns::SubmitTurnRequest{
        requested_model:None,scope:parent_scope.clone(),actor:actor.clone(),accepted_message_ref:ironclaw_turns::AcceptedMessageRef::new("msg:synthetic-parent")?,
        source_binding_ref:ironclaw_turns::SourceBindingRef::new("source:synthetic-parent")?,reply_target_binding_ref:ironclaw_turns::ReplyTargetBindingRef::new("reply:synthetic-parent")?,
        requested_run_profile:None,idempotency_key:ironclaw_turns::IdempotencyKey::new("idem:synthetic-parent")?,received_at:chrono::Utc::now(),requested_run_id:None,
        parent_run_id:None,subagent_depth:0,spawn_tree_root_run_id:None,product_context:None,llm_subject:None}).await?;
    let parent_runner=ironclaw_turns::TurnRunnerId::new();let parent_lease=ironclaw_turns::TurnLeaseToken::new();
    phase.store(11,Ordering::Relaxed); // ParentClaim
    assert!(published.claim_next_run(ironclaw_turns::runner::ClaimRunRequest{runner_id:parent_runner,lease_token:parent_lease,scope_filter:Some(parent_scope.clone())}).await?.is_some(),"actual parent claim required");
    // 合成host binding canary；不是终态SQL，也不是TQ迁移/资产定义。
    phase.store(12,Ordering::Relaxed); // ParentBinding
    client.execute("INSERT INTO tianquan_run_bindings(run_id,project_id,legacy_layer) VALUES($1,$2,$3)",&[&parent.to_string(),&PROJECT,&LAYER]).await?;
    phase.store(13,Ordering::Relaxed); // ChildSubmit
    let ironclaw_turns::SubmitTurnResponse::Accepted{run_id:child,..}=coordinator.submit_child_run(ironclaw_turns::SubmitChildRunRequest{
        parent_scope:parent_scope.clone(),parent_run_id:parent,child_scope:child_scope.clone(),actor:actor.clone(),accepted_message_ref:ironclaw_turns::AcceptedMessageRef::new("msg:synthetic-child")?,
        source_binding_ref:ironclaw_turns::SourceBindingRef::new("source:synthetic-child")?,reply_target_binding_ref:ironclaw_turns::ReplyTargetBindingRef::new("reply:synthetic-child")?,requested_run_profile:None,
        idempotency_key:ironclaw_turns::IdempotencyKey::new("idem:synthetic-child")?,received_at:chrono::Utc::now(),requested_run_id:None,spawn_tree_descendant_cap:16}).await?;
    phase.store(14,Ordering::Relaxed); // AwaitRecordTypes
    let gate=ironclaw_turns::GateRef::new("gate:synthetic-child")?;let reference=ironclaw_turns::LoopResultRef::new("result:synthetic-child")?;
    let mut context=ironclaw_agent_loop::test_support::test_run_context("synthetic-exporter-parent");
    context.scope=parent_scope.clone();context.thread_id=parent_scope.thread_id.clone();context.run_id=parent;context.actor=Some(actor);
    let kind=SubagentKindId::new("worldsmith")?;
    phase.store(15,Ordering::Relaxed); // AwaitRecord
    edges.record_awaited_child(AwaitedChildSetRecord{gate_ref:gate.clone(),parent_run_context:context,tree_root_run_id:parent,child_scope:child_scope.clone(),child_run_id:child,
        child_thread_id:child_scope.thread_id.clone(),source_binding_ref:ironclaw_turns::SourceBindingRef::new(format!("subagent-source:{child}"))?,
        reply_target_binding_ref:ironclaw_turns::ReplyTargetBindingRef::new(format!("subagent-reply:{child}"))?,subagent_kind:kind.clone(),
        spawn_capability_id:ironclaw_host_api::CapabilityId::new(ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID)?,result_ref:reference.clone(),mode:SpawnSubagentMode::Blocking}).await?;
    phase.store(16,Ordering::Relaxed); // GoalStore
    goals.put(&child_scope,child,SubagentGoal{task:"synthetic callback storage only".into(),handoff:None})?;
    phase.store(17,Ordering::Relaxed); // ThreadScope
    let thread_scope=ThreadScope{tenant_id:parent_scope.tenant_id.clone(),agent_id:parent_scope.agent_id.clone().unwrap(),project_id:parent_scope.project_id.clone(),owner_user_id:Some(UserId::new("synthetic-exporter-user")?),mission_id:None};
    phase.store(18,Ordering::Relaxed); // EnsureThreads
    for thread in [&parent_scope.thread_id,&child_scope.thread_id]{threads.ensure_thread(ironclaw_threads::EnsureThreadRequest{scope:thread_scope.clone(),thread_id:Some(thread.clone()),created_by_actor_id:"synthetic-exporter".into(),title:None,metadata_json:None}).await?;}
    phase.store(19,Ordering::Relaxed); // ToolResultReference
    threads.append_tool_result_reference(ironclaw_threads::AppendToolResultReferenceRequest{scope:thread_scope.clone(),thread_id:parent_scope.thread_id.clone(),turn_run_id:parent.to_string(),result_ref:reference.as_str().into(),
        safe_summary:ironclaw_threads::ToolResultSafeSummary::new("synthetic subagent spawned")?,provider_call:None,model_observation:None}).await?;
    phase.store(20,Ordering::Relaxed); // FinalizedMessage
    threads.append_finalized_assistant_message(ironclaw_threads::AppendFinalizedAssistantMessageRequest{scope:thread_scope,thread_id:child_scope.thread_id.clone(),turn_run_id:child.to_string(),content:ironclaw_threads::MessageContent::text(RAW_CANARY)}).await?;
    // 正式typed spawn出口；不直接构造ProvenanceRecord、不SQL写Spawned/Completed。
    phase.store(21,Ordering::Relaxed); // TypedSpawnStart
    edges.record_child_submitted(&child_scope,parent,child,&kind,chrono::Utc::now()).await;
    phase.store(22,Ordering::Relaxed); // StartRow
    let start=read_row(client,child).await?;
    assert!(start.parent.as_deref()==Some(parent.to_string().as_str()) && start.kind=="worldsmith" && start.project.as_deref()==Some(PROJECT)
        && start.layer.as_deref()==Some(LAYER) && start.terminal.as_deref()==Some("Spawned") && start.hash.is_none(),"actual typed start PG identity required");
    phase.store(23,Ordering::Relaxed); // ParentBlock
    published.block_run(ironclaw_turns::runner::BlockRunRequest{run_id:parent,runner_id:parent_runner,lease_token:parent_lease,checkpoint_id:ironclaw_turns::TurnCheckpointId::new(),
        state_ref:ironclaw_turns::run_profile::LoopCheckpointStateRef::new("checkpoint:synthetic-parent")?,reason:ironclaw_turns::BlockedReason::AwaitDependentRun{gate_ref:gate}}).await?;
    phase.store(24,Ordering::Relaxed); // ChildLease
    let child_runner=ironclaw_turns::TurnRunnerId::new();let child_lease=ironclaw_turns::TurnLeaseToken::new();
    phase.store(25,Ordering::Relaxed); // ChildClaim
    assert!(published.claim_next_run(ironclaw_turns::runner::ClaimRunRequest{runner_id:child_runner,lease_token:child_lease,scope_filter:Some(child_scope.clone())}).await?.is_some(),"actual child claim required");
    phase.store(26,Ordering::Relaxed); // CompleteRun
    let committed=published.complete_run(ironclaw_turns::runner::CompleteRunRequest{run_id:child,runner_id:child_runner,lease_token:child_lease}).await?;
    assert!(committed.status==TurnStatus::Completed,"actual committed child terminal required");
    phase.store(27,Ordering::Relaxed); // TerminalRow
    let first=read_row(client,child).await?;let expected_hash=Sha256::digest(RAW_CANARY.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    assert!(first.parent==start.parent && first.kind==start.kind && first.project==start.project && first.layer==start.layer && first.spawned==start.spawned
        && first.terminal.as_deref()==Some("Completed") && first.hash.as_deref()==Some(expected_hash.as_str()) && first.failure.is_none(),"actual callback PG terminal and hash must match");
    phase.store(28,Ordering::Relaxed); // CapturedEvent
    let original_event=captured.events().into_iter().find(|event|event.run_id==child && event.status==TurnStatus::Completed).ok_or("actual committed lifecycle event missing")?;
    phase.store(29,Ordering::Relaxed); // ReplayEvent
    bus.publish_event(original_event).await?;
    phase.store(30,Ordering::Relaxed); // ReplayRow
    let after=read_row(client,child).await?;
    assert!(first==after,"same original committed callback replay must preserve exact PG identity/hash");
    phase.store(31,Ordering::Relaxed); // EdgeClosed
    assert!(edges.peek(&child_scope,parent,child).await?.is_none(),"actual terminal callback must close its original await edge");
    phase.store(32,Ordering::Relaxed); // GoalReleased
    assert!(goals.get(&child_scope,child).is_err(),"actual terminal callback must release its original goal");
    phase.store(33,Ordering::Relaxed); // RawCanaryRead
    let raw=std::fs::read(raw_root.join("runs").join(format!("{child}.txt")))?;
    assert!(raw.as_slice()==RAW_CANARY.as_bytes(),"actual synthetic raw bytes must remain original");
    phase.store(34,Ordering::Relaxed); // SuccessProjection
    Ok(json!({"schemaVersion":"exporterCallbackStorageCompletedReplay/1","actualSubmitClaimCommit":true,"requiredLifecycleCallback":true,"actualTypedSpawnStart":true,
        "pgIdentityAndTerminalVerified":true,"rawHashMatchesInRam":true,"replayIdentityAndHashUnchanged":true,"fixtureResolver":true,"fixtureTwoTenantUserPathsIsolated":true,
        "productionCompositionMountQualified":false,"syntheticActor":true,"syntheticProvider":true,"modelCalls":0,"normalActorQualified":false,"g01Qualified":false,"gcInvoked":false,"privateBodyOrHashPersisted":false}))
}

fn closed_diagnostic(stage:&'static str,error:&(dyn std::error::Error+'static))->Value {
    let mut sqlstate=None;let mut io_category=None;let mut host_category=None;
    let mut cause=Some(error);
    for _ in 0..8 {
        let Some(current)=cause else {break};
        if let Some(pg)=current.downcast_ref::<tokio_postgres::Error>() {
            sqlstate=pg.code().map(|code|match code.code(){
                "08001"=>"08001","08004"=>"08004","08006"=>"08006","08P01"=>"08P01",
                "28P01"=>"28P01","28000"=>"28000","3D000"=>"3D000","42501"=>"42501",
                "42704"=>"42704","22023"=>"22023","53300"=>"53300","57P01"=>"57P01",
                "57P03"=>"57P03",_=>"OtherSqlState"});
        }
        if let Some(io)=current.downcast_ref::<std::io::Error>() {
            io_category=Some(match io.kind(){
                std::io::ErrorKind::ConnectionRefused=>"ConnectionRefused",std::io::ErrorKind::ConnectionReset=>"ConnectionReset",
                std::io::ErrorKind::ConnectionAborted=>"ConnectionAborted",std::io::ErrorKind::TimedOut=>"TimedOut",
                std::io::ErrorKind::PermissionDenied=>"PermissionDenied",std::io::ErrorKind::InvalidInput=>"InvalidInput",
                std::io::ErrorKind::InvalidData=>"InvalidData",std::io::ErrorKind::UnexpectedEof=>"UnexpectedEof",
                std::io::ErrorKind::NotConnected=>"NotConnected",_=>"OtherIoKind"});
        }
        if let Some(host)=current.downcast_ref::<ironclaw_host_api::HostApiError>() {
            use ironclaw_host_api::HostApiError;
            host_category=Some(match host {
                HostApiError::InvalidId{..}=>"InvalidId",HostApiError::InvalidPath{..}=>"InvalidPath",
                HostApiError::InvalidCapability{..}=>"InvalidCapability",HostApiError::InvalidMount{..}=>"InvalidMount",
                HostApiError::InvalidNetworkTarget{..}=>"InvalidNetworkTarget",HostApiError::InvalidRuntimeCredentialTarget{..}=>"InvalidRuntimeCredentialTarget",
                HostApiError::InvalidSafeSummary{..}=>"InvalidSafeSummary",HostApiError::InvalidHostRemediation{..}=>"InvalidHostRemediation",
                HostApiError::InvariantViolation{..}=>"InvariantViolation"});
        }
        cause=current.source();
    }
    json!({"schemaVersion":"exporterRunFailure/1","stage":stage,"sqlstate":sqlstate,"ioCategory":io_category,
        "hostCategory":host_category,"originalConnectionJoined":true,"rawErrorOrUriOrDigestEmitted":false})
}
fn closed_phase(value:u8)->&'static str {
    match value {
        1=>"Identity",
        2=>"EmptyWireTables",
        3=>"RawDirectory",
        4=>"FixtureMounts",
        5=>"FixtureIsolation",
        6=>"Services",
        7=>"LifecycleSubscriptions",
        8=>"BindCoordinator",
        9=>"Scopes",
        10=>"ParentSubmit",
        11=>"ParentClaim",
        12=>"ParentBinding",
        13=>"ChildSubmit",
        14=>"AwaitRecordTypes",
        15=>"AwaitRecord",
        16=>"GoalStore",
        17=>"ThreadScope",
        18=>"EnsureThreads",
        19=>"ToolResultReference",
        20=>"FinalizedMessage",
        21=>"TypedSpawnStart",
        22=>"StartRow",
        23=>"ParentBlock",
        24=>"ChildLease",
        25=>"ChildClaim",
        26=>"CompleteRun",
        27=>"TerminalRow",
        28=>"CapturedEvent",
        29=>"ReplayEvent",
        30=>"ReplayRow",
        31=>"EdgeClosed",
        32=>"GoalReleased",
        33=>"RawCanaryRead",
        34=>"SuccessProjection",
        _=>"UnknownPhase"
    }
}

#[tokio::test]
#[ignore = "Root fresh owned PG/wire tables/raw directory only; synthetic callback storage, not normal actor/G01"]
async fn owned_coordinator_callback_exports_completed_and_replay_is_immutable(){
    let original_deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(180);
    let url=std::env::var("TIANQUAN_SPAWN_PG_URL").expect("Root explicit private PG URL required");
    let config:tokio_postgres::Config=url.parse().expect("Root PG configuration syntax required");
    assert!(config.get_hosts().iter().all(|h|matches!(h,tokio_postgres::config::Host::Tcp(host) if host=="127.0.0.1")) && config.get_hosts().len()==1
        && config.get_ports()==&[55432] && config.get_dbname()==Some(std::env::var("TQ_EXPORTER_DATABASE").unwrap().as_str()),"exact Root owned loopback database required");
    let (client,connection)=tokio::time::timeout_at(original_deadline,config.connect(tokio_postgres::NoTls)).await.expect("original deadline").expect("Root owned PG connection required");
    let connection_handle=tokio::spawn(connection);
    let phase=AtomicU8::new(0);
    let observed=tokio::time::timeout_at(original_deadline,run(&client,&phase)).await;
    drop(client);
    // 原connection保同handle，真实EOF/join后才断言；不abort/TempDir删除/续预算。
    let joined=connection_handle.await;
    assert!(matches!(joined,Ok(Ok(()))),"original PG connection must join");
    let proof=match observed.expect("original whole deadline") {
        Ok(value)=>value,
        Err(error)=>{
            let failure=closed_diagnostic(closed_phase(phase.load(Ordering::Relaxed)),error.as_ref());
            eprintln!("EXPORTER_RUN_FAILURE_EVIDENCE={failure}");
            panic!("actual exporter storage caller oracle rejected; closed diagnostic emitted");
        }
    };
    println!("EXPORTER_STORAGE_COMPLETED_REPLAY_EVIDENCE={proof}");
}

#[test]
fn exporter_closed_diagnostic_masks_private_host_fields(){
    let error=ironclaw_host_api::HostApiError::InvalidId{kind:"PRIVATE_KIND",value:"PRIVATE_CANARY_URI_BODY_HASH".into(),reason:"PRIVATE_REASON".into()};
    let proof=closed_diagnostic("Scopes",&error);
    assert_eq!(proof["hostCategory"],"InvalidId");
    assert_eq!(proof["stage"],"Scopes");
    assert_eq!(proof.as_object().unwrap().len(),7);
    assert!(!proof.to_string().contains("PRIVATE"));
}

#[tokio::test]
async fn exporter_fixture_dynamic_turns_and_system_roster_are_isolated(){
    let root=Arc::new(InMemoryBackend::new());
    let fs=ScopedFilesystem::new(root.clone(),fixture_mounts);
    fixture_isolation(root.as_ref(),&fs).await.unwrap();
    let system=ResourceScope::system();
    let owner=scope("synthetic-tenant-a","synthetic-user-a","probe-a").to_resource_scope();
    let mut other_agent=owner.clone();other_agent.agent_id=Some(AgentId::new("synthetic-other-agent").unwrap());
    let mut other_project=owner.clone();other_project.project_id=Some(ProjectId::new("synthetic-other-project").unwrap());
    for alias in ["/engine","/turns"] {
        let path=ScopedPath::new(format!("{alias}/all-owner-axes/canary")).unwrap();
        let original=fs.resolve(&owner,&path).unwrap();
        assert_ne!(original,fs.resolve(&other_agent,&path).unwrap(),"fixture roots isolate agent axis");
        assert_ne!(original,fs.resolve(&other_project,&path).unwrap(),"fixture roots isolate project axis");
    }
    let roster_path=ScopedPath::new("/turns/subagent-await-scopes/00/public-fixture.json").unwrap();
    let system_path=fs.resolve(&system,&roster_path).unwrap();
    let owner_path=fs.resolve(&owner,&roster_path).unwrap();
    assert_ne!(system_path,owner_path,"system roster must remain separate from owner turns");
    fs.put(&system,&roster_path,Entry::bytes(b"system-public-canary".to_vec()),CasExpectation::Absent).await.unwrap();
    assert!(fs.get(&owner,&roster_path).await.unwrap().is_none(),"owner cannot observe system roster through its turns mount");
    assert!(fs.resolve(&system,&ScopedPath::new("/turns/owner-private/canary").unwrap()).is_err(),"system fixture grant only permits roster subtree");
    assert!(fs.resolve(&system,&ScopedPath::new("/engine/owner-private/canary").unwrap()).is_err(),"system fixture grant must not expose owner engine");
}

async fn run_failed_callback(client:&tokio_postgres::Client,phase:&AtomicU8,expected:FailedCallbackExpectation,terminal_kind:HostTerminalKind)->TestResult<Value>{
    let expected_fixture=match expected{FailedCallbackExpectation::Persisted=>"owned_failed_update_persist_v1",FailedCallbackExpectation::Rejected=>"owned_failed_update_reject_v1"};
    assert!(std::env::var("TQ_EXPORTER_PG_FAULT")?.as_str()==expected_fixture,"Root explicit fresh failed-update fixture required");
    let full_tq_schema=std::env::var("TQ_EXPORTER_FULL_TQ_SCHEMA").ok().as_deref()==Some("1");
    assert!(!full_tq_schema || expected==FailedCallbackExpectation::Persisted,"full TianQuan schema mode only applies to persisted terminal");
    let terminal_mode=match terminal_kind{HostTerminalKind::Failed=>"failed",HostTerminalKind::Cancelled=>"cancelled"};
    assert_eq!(std::env::var("TQ_EXPORTER_TERMINAL_KIND")?,terminal_mode,"explicit host terminal kind required");
    let fixture_project=std::env::var("TQ_EXPORTER_PROJECT").unwrap_or_else(|_|PROJECT.into());
    let fixture_owner=std::env::var("TQ_EXPORTER_OWNER").unwrap_or_else(|_|"synthetic-exporter-user".into());
    let fixture_parent_thread=std::env::var("TQ_EXPORTER_PARENT_THREAD").unwrap_or_else(|_|"synthetic-parent-thread".into());
    let fixture_child_thread=std::env::var("TQ_EXPORTER_CHILD_THREAD").unwrap_or_else(|_|"synthetic-child-thread".into());
    let parent=if full_tq_schema{TurnRunId::parse(&std::env::var("TQ_EXPORTER_PARENT_RUN")?)?}else{TurnRunId::new()};
    let child=if full_tq_schema{TurnRunId::parse(&std::env::var("TQ_EXPORTER_CHILD_RUN")?)?}else{TurnRunId::new()};
    phase.store(1,Ordering::Relaxed); // Identity
    let identity=client.query_one("SELECT current_database(),current_schema(),current_user,obj_description(to_regnamespace(current_schema()),'pg_namespace')",&[]).await?;
    assert!(identity.get::<_,String>(0)==std::env::var("TQ_EXPORTER_DATABASE")?,"owned database identity required");
    assert!(identity.get::<_,String>(1)==std::env::var("TQ_EXPORTER_SCHEMA")?,"owned schema identity required");
    assert!(identity.get::<_,String>(2)==std::env::var("TQ_EXPORTER_USER")?,"owned role identity required");
    assert!(identity.get::<_,Option<String>>(3)==Some(std::env::var("TQ_EXPORTER_OWNERSHIP_COMMENT")?),"owned schema comment required");
    phase.store(2,Ordering::Relaxed); // Fresh thin tables or exact preexisting full-schema fixture
    if full_tq_schema {
        let project_row=client.query_one("SELECT EXISTS(SELECT 1 FROM tianquan_projects WHERE id=$1 AND owner_user_id=$2)",&[&fixture_project,&fixture_owner]).await?;
        let project:bool=project_row.get(0);
        let binding:Option<(String,String,String)>=client.query_opt("SELECT project_id,owner_user_id,thread_id FROM tianquan_run_bindings WHERE run_id=$1",&[&parent.to_string()]).await?.map(|row|(row.get(0),row.get(1),row.get(2)));
        let count_row=client.query_one("SELECT count(*) FROM spawn_records",&[]).await?;
        let count:i64=count_row.get(0);
        let start=read_row(client,child).await?;
        assert!(project && binding==Some((fixture_project.clone(),fixture_owner.clone(),fixture_parent_thread.clone())) && count==1,"full TianQuan fixture project/parent/one typed spawn start required");
        assert!(start.parent.as_deref()==Some(parent.to_string().as_str()) && start.project.as_deref()==Some(fixture_project.as_str()) && start.layer.as_deref()==Some(LAYER) && start.terminal.as_deref()==Some("Spawned") && start.hash.is_none(),"full TianQuan fixture child must remain Spawned before host failure");
    } else {
        let empty=client.query_one("SELECT (SELECT count(*) FROM spawn_records),(SELECT count(*) FROM tianquan_run_bindings)",&[]).await?;
        assert!(empty.get::<_,i64>(0)==0 && empty.get::<_,i64>(1)==0,"fresh thin wire tables required, never adopt previous records");
    }
    // 故障fixture只能由Root在本次fresh薄表创建后预建；不自建/改业务触发器、不setter终态。
    match expected {
        FailedCallbackExpectation::Rejected=>{
            let fixture=client.query_one("SELECT (SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname='spawn_records' AND t.tgname='exporter_reject_failed_update' AND NOT t.tgisinternal),last_value,is_called FROM exporter_failed_attempts",&[]).await?;
            assert!(fixture.get::<_,i64>(0)==1 && fixture.get::<_,i64>(1)==1 && !fixture.get::<_,bool>(2),"fresh explicit reject trigger and uncalled attempt sequence required");
        }
        FailedCallbackExpectation::Persisted=>{
            let clean=client.query_one("SELECT (SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname='spawn_records' AND t.tgname='exporter_reject_failed_update' AND NOT t.tgisinternal),to_regclass('exporter_failed_attempts')::text",&[]).await?;
            assert!(clean.get::<_,i64>(0)==0 && clean.get::<_,Option<String>>(1).is_none(),"persisted-terminal fixture must not contain reject trigger or attempt sequence");
        }
    }
    phase.store(3,Ordering::Relaxed); // RawDirectory
    let raw_root=std::path::PathBuf::from(std::env::var("TIANQUAN_SPAWN_PROSE_DIR")?);
    assert!(raw_root.is_absolute() && raw_root.is_dir(),"Root explicit fresh raw directory required");
    assert!(std::fs::read_dir(&raw_root)?.next().is_none(),"fresh raw directory must be empty");

    phase.store(4,Ordering::Relaxed); // FixtureMounts
    let root=Arc::new(InMemoryBackend::new());let fs=Arc::new(ScopedFilesystem::new(root.clone(),fixture_mounts));
    phase.store(5,Ordering::Relaxed); // FixtureIsolation
    fixture_isolation(root.as_ref(),fs.as_ref()).await?;
    phase.store(6,Ordering::Relaxed); // Services
    let edges=Arc::new(FilesystemAwaitEdgeStore::new(fs));
    let base=Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let threads=Arc::new(InMemorySessionThreadService::default());
    let goals=Arc::new(InMemoryBoundedSubagentGoalStore::new());
    let goal_port:Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore>=goals.clone();
    let state_port:Arc<dyn ironclaw_turns::TurnSpawnTreeStateStore>=base.clone();
    let resolver=Arc::new(AwaitEdgeResolver::new_unbound(edges.clone(),goal_port,state_port,Arc::new(AllowResultWriter),threads.clone()));
    let bus=Arc::new(DefaultTurnLifecycleEventBus::new());
    phase.store(7,Ordering::Relaxed); // LifecycleSubscriptions
    bus.subscribe_required(resolver.clone() as Arc<dyn TurnCommittedEventObserver>)?;
    let captured=Arc::new(ironclaw_turns::InMemoryTurnEventSink::default());
    bus.subscribe_best_effort(captured.clone() as Arc<dyn TurnEventSink>)?;
    let published=Arc::new(LifecyclePublishingTurnStateStore::new(base.clone(),bus.clone()));
    let error_port:Arc<dyn LifecyclePublicationErrorPort>=published.clone();
    let coordinator=Arc::new(DefaultTurnCoordinator::new(published.clone()).with_lifecycle_publication_error_port(error_port));
    phase.store(8,Ordering::Relaxed); // BindCoordinator
    resolver.bind_coordinator(coordinator.clone() as Arc<dyn TurnCoordinator>)?;
    phase.store(9,Ordering::Relaxed); // Scopes
    let parent_scope=scope_with_project("synthetic-exporter-tenant",&fixture_owner,&fixture_project,&fixture_parent_thread);
    let child_scope=scope_with_project("synthetic-exporter-tenant",&fixture_owner,&fixture_project,&fixture_child_thread);
    let actor=ironclaw_turns::TurnActor::new(UserId::new(&fixture_owner)?);
    phase.store(10,Ordering::Relaxed); // ParentSubmit
    let ironclaw_turns::SubmitTurnResponse::Accepted{run_id:submitted_parent,..}=coordinator.submit_turn(ironclaw_turns::SubmitTurnRequest{
        requested_model:None,scope:parent_scope.clone(),actor:actor.clone(),accepted_message_ref:ironclaw_turns::AcceptedMessageRef::new(format!("msg:synthetic-parent:{}",parent.as_uuid()))?,
        source_binding_ref:ironclaw_turns::SourceBindingRef::new(format!("source:synthetic-parent:{}",parent.as_uuid()))?,reply_target_binding_ref:ironclaw_turns::ReplyTargetBindingRef::new(format!("reply:synthetic-parent:{}",parent.as_uuid()))?,
        requested_run_profile:None,idempotency_key:ironclaw_turns::IdempotencyKey::new(format!("idem:synthetic-parent:{}",parent.as_uuid()))?,received_at:chrono::Utc::now(),requested_run_id:Some(parent.clone()),
        parent_run_id:None,subagent_depth:0,spawn_tree_root_run_id:None,product_context:None,llm_subject:None}).await?;
    assert_eq!(submitted_parent,parent,"trusted parent run id must match requested full-schema identity");
    let parent_runner=ironclaw_turns::TurnRunnerId::new();let parent_lease=ironclaw_turns::TurnLeaseToken::new();
    phase.store(11,Ordering::Relaxed); // ParentClaim
    assert!(published.claim_next_run(ironclaw_turns::runner::ClaimRunRequest{runner_id:parent_runner,lease_token:parent_lease,scope_filter:Some(parent_scope.clone())}).await?.is_some(),"actual parent claim required");
    // 合成host binding canary；不是终态SQL，也不是TQ迁移/资产定义。
    phase.store(12,Ordering::Relaxed); // ParentBinding
    if full_tq_schema {
        let exact_row=client.query_one("SELECT project_id,owner_user_id,thread_id FROM tianquan_run_bindings WHERE run_id=$1",&[&parent.to_string()]).await?;
        let exact:(String,String,String)=(exact_row.get(0),exact_row.get(1),exact_row.get(2));
        assert!(exact==(fixture_project.clone(),fixture_owner.clone(),fixture_parent_thread.clone()),"full-schema parent binding changed before host start");
    } else {
        client.execute("INSERT INTO tianquan_run_bindings(run_id,project_id,legacy_layer) VALUES($1,$2,$3)",&[&parent.to_string(),&PROJECT,&LAYER]).await?;
    }
    phase.store(13,Ordering::Relaxed); // ChildSubmit
    let ironclaw_turns::SubmitTurnResponse::Accepted{run_id:submitted_child,..}=coordinator.submit_child_run(ironclaw_turns::SubmitChildRunRequest{
        parent_scope:parent_scope.clone(),parent_run_id:parent.clone(),child_scope:child_scope.clone(),actor:actor.clone(),accepted_message_ref:ironclaw_turns::AcceptedMessageRef::new(format!("msg:synthetic-child:{}",child.as_uuid()))?,
        source_binding_ref:ironclaw_turns::SourceBindingRef::new(format!("source:synthetic-child:{}",child.as_uuid()))?,reply_target_binding_ref:ironclaw_turns::ReplyTargetBindingRef::new(format!("reply:synthetic-child:{}",child.as_uuid()))?,requested_run_profile:None,
        idempotency_key:ironclaw_turns::IdempotencyKey::new(format!("idem:synthetic-child:{}",child.as_uuid()))?,received_at:chrono::Utc::now(),requested_run_id:Some(child.clone()),spawn_tree_descendant_cap:16}).await?;
    assert_eq!(submitted_child,child,"trusted child run id must match requested full-schema identity");
    phase.store(14,Ordering::Relaxed); // AwaitRecordTypes
    let gate=ironclaw_turns::GateRef::new("gate:synthetic-child")?;let reference=ironclaw_turns::LoopResultRef::new("result:synthetic-child")?;
    let mut context=ironclaw_agent_loop::test_support::test_run_context("synthetic-exporter-parent");
    context.scope=parent_scope.clone();context.thread_id=parent_scope.thread_id.clone();context.run_id=parent;context.actor=Some(actor.clone());
    let kind=SubagentKindId::new("worldsmith")?;
    phase.store(15,Ordering::Relaxed); // AwaitRecord
    edges.record_awaited_child(AwaitedChildSetRecord{gate_ref:gate.clone(),parent_run_context:context,tree_root_run_id:parent,child_scope:child_scope.clone(),child_run_id:child,
        child_thread_id:child_scope.thread_id.clone(),source_binding_ref:ironclaw_turns::SourceBindingRef::new(format!("subagent-source:{child}"))?,
        reply_target_binding_ref:ironclaw_turns::ReplyTargetBindingRef::new(format!("subagent-reply:{child}"))?,subagent_kind:kind.clone(),
        spawn_capability_id:ironclaw_host_api::CapabilityId::new(ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID)?,result_ref:reference.clone(),mode:SpawnSubagentMode::Blocking}).await?;
    phase.store(16,Ordering::Relaxed); // GoalStore
    goals.put(&child_scope,child,SubagentGoal{task:"synthetic callback storage only".into(),handoff:None})?;
    phase.store(17,Ordering::Relaxed); // ThreadScope
    let thread_scope=ThreadScope{tenant_id:parent_scope.tenant_id.clone(),agent_id:parent_scope.agent_id.clone().unwrap(),project_id:parent_scope.project_id.clone(),owner_user_id:Some(UserId::new(&fixture_owner)?),mission_id:None};
    phase.store(18,Ordering::Relaxed); // EnsureThreads
    for thread in [&parent_scope.thread_id,&child_scope.thread_id]{threads.ensure_thread(ironclaw_threads::EnsureThreadRequest{scope:thread_scope.clone(),thread_id:Some(thread.clone()),created_by_actor_id:"synthetic-exporter".into(),title:None,metadata_json:None}).await?;}
    phase.store(19,Ordering::Relaxed); // ToolResultReference
    threads.append_tool_result_reference(ironclaw_threads::AppendToolResultReferenceRequest{scope:thread_scope.clone(),thread_id:parent_scope.thread_id.clone(),turn_run_id:parent.to_string(),result_ref:reference.as_str().into(),
        safe_summary:ironclaw_threads::ToolResultSafeSummary::new("synthetic subagent spawned")?,provider_call:None,model_observation:None}).await?;
    phase.store(20,Ordering::Relaxed); // FinalizedMessage
    threads.append_finalized_assistant_message(ironclaw_threads::AppendFinalizedAssistantMessageRequest{scope:thread_scope,thread_id:child_scope.thread_id.clone(),turn_run_id:child.to_string(),content:ironclaw_threads::MessageContent::text(RAW_CANARY)}).await?;
    // 正式typed spawn出口；不直接构造ProvenanceRecord、不SQL写Spawned/Completed。
    phase.store(21,Ordering::Relaxed); // TypedSpawnStart
    edges.record_child_submitted(&child_scope,parent,child,&kind,chrono::Utc::now()).await;
    phase.store(22,Ordering::Relaxed); // StartRow
    let start=read_row(client,child).await?;
    assert!(start.parent.as_deref()==Some(parent.to_string().as_str()) && start.kind=="worldsmith"
        && start.project.as_deref()==Some(fixture_project.as_str()) && start.layer.as_deref()==Some(LAYER) && start.terminal.as_deref()==Some("Spawned") && start.hash.is_none(),"actual typed start PG identity required");
    phase.store(23,Ordering::Relaxed); // ParentBlock
    published.block_run(ironclaw_turns::runner::BlockRunRequest{run_id:parent,runner_id:parent_runner,lease_token:parent_lease,checkpoint_id:ironclaw_turns::TurnCheckpointId::new(),
        state_ref:ironclaw_turns::run_profile::LoopCheckpointStateRef::new("checkpoint:synthetic-parent")?,reason:ironclaw_turns::BlockedReason::AwaitDependentRun{gate_ref:gate}}).await?;
    phase.store(24,Ordering::Relaxed); // ChildLease
    let child_runner=ironclaw_turns::TurnRunnerId::new();let child_lease=ironclaw_turns::TurnLeaseToken::new();
    phase.store(25,Ordering::Relaxed); // ChildClaim
    assert!(published.claim_next_run(ironclaw_turns::runner::ClaimRunRequest{runner_id:child_runner,lease_token:child_lease,scope_filter:Some(child_scope.clone())}).await?.is_some(),"actual child claim required");
    phase.store(26,Ordering::Relaxed); // actual Failed lease commit -> required callback
    let failure=ironclaw_turns::SanitizedFailure::new("owned_pg_fault_canary").map_err(|_|"public synthetic failure category invalid")?.with_detail("public synthetic detail retained in committed state only");
    let (committed,expected_status)=match terminal_kind{
        HostTerminalKind::Failed=>(
            match published.fail_run(ironclaw_turns::runner::FailRunRequest{run_id:child,runner_id:child_runner,lease_token:child_lease,failure}).await{
                Ok(value)=>value,
                Err(error)=>{eprintln!("W6_FAIL_RUN_DIRECT_ERROR={error:?}");return Err(Box::new(error));}
            },TurnStatus::Failed),
        HostTerminalKind::Cancelled=>{
            let requested=coordinator.cancel_run(ironclaw_turns::CancelRunRequest{scope:child_scope.clone(),actor:actor.clone(),run_id:child,
                reason:ironclaw_turns::SanitizedCancelReason::UserRequested,idempotency_key:ironclaw_turns::IdempotencyKey::new(format!("cancel:synthetic-child:{}",child.as_uuid()))?}).await;
            let requested=match requested{Ok(value)=>value,Err(error)=>{eprintln!("W6_CANCEL_REQUEST_DIRECT_ERROR={error:?}");return Err(Box::new(error));}};
            assert!(requested.status==TurnStatus::CancelRequested,"coordinator must durably request cancellation before completion");
            (match published.cancel_run(ironclaw_turns::runner::CancelRunCompletionRequest{run_id:child,runner_id:child_runner,lease_token:child_lease}).await{
                Ok(value)=>value,
                Err(error)=>{eprintln!("W6_CANCEL_COMPLETION_DIRECT_ERROR={error:?}");return Err(Box::new(error));}
            },TurnStatus::Cancelled)
        },
    };
    assert!(committed.status==expected_status,"actual committed child terminal required");
    if terminal_kind==HostTerminalKind::Failed {
        assert!(committed.failure.as_ref().is_some_and(|v|v.category()=="owned_pg_fault_canary" && v.detail()==Some("public synthetic detail retained in committed state only")),"committed sanitized category/detail must remain original");
    }
    phase.store(27,Ordering::Relaxed); // terminal PG write/rollback oracle
    let terminal=read_row(client,child).await?;
    let expected_hash=Sha256::digest(RAW_CANARY.as_bytes()).iter().map(|byte|format!("{byte:02x}")).collect::<String>();
    match expected {
        FailedCallbackExpectation::Rejected=>{
            assert!(terminal==start && terminal.terminal.as_deref()==Some("Spawned") && terminal.hash.is_none() && terminal.failure.is_none(),"PG rejection must preserve complete original start row; storage terminal is unverified");
            let attempts=client.query_one("SELECT last_value,is_called FROM exporter_failed_attempts",&[]).await?;
            assert!(attempts.get::<_,i64>(0)==1 && attempts.get::<_,bool>(1),"one actual failed UPDATE must reach PG reject trigger");
        }
        FailedCallbackExpectation::Persisted=>{
            assert!(terminal.parent==start.parent && terminal.kind==start.kind && terminal.project==start.project && terminal.layer==start.layer && terminal.spawned==start.spawned
                && terminal.terminal.as_deref()==Some(match terminal_kind{HostTerminalKind::Failed=>"Failed",HostTerminalKind::Cancelled=>"Cancelled"}) && terminal.hash.as_deref()==Some(expected_hash.as_str()) && terminal.failure.as_deref()==match terminal_kind{HostTerminalKind::Failed=>Some("owned_pg_fault_canary"),HostTerminalKind::Cancelled=>None},
                "actual failed callback must persist exact start identity, raw hash, and sanitized failure category");
        }
    }
    phase.store(28,Ordering::Relaxed); // actual sanitized committed event
    let original_event=captured.events().into_iter().find(|event|event.run_id==child && event.status==expected_status).ok_or("actual committed terminal lifecycle event missing")?;
    let expected_failure=match terminal_kind{HostTerminalKind::Failed=>Some("owned_pg_fault_canary"),HostTerminalKind::Cancelled=>original_event.sanitized_reason.as_deref()};
    if terminal_kind==HostTerminalKind::Failed {assert!(expected_failure==Some("owned_pg_fault_canary"),"event failure reason must equal original sanitized category only");}
    phase.store(29,Ordering::Relaxed); // original event replay
    bus.publish_event(original_event).await?;
    phase.store(30,Ordering::Relaxed); // replay row; reject count when that fixture is selected
    let replay=read_row(client,child).await?;
    match expected {
        FailedCallbackExpectation::Rejected=>{
            let attempts_after=client.query_one("SELECT last_value,is_called FROM exporter_failed_attempts",&[]).await?;
            assert!(replay==start && attempts_after.get::<_,i64>(0)==1 && attempts_after.get::<_,bool>(1),"closed original event replay cannot repair rejected PG terminal or replace original identity");
        }
        FailedCallbackExpectation::Persisted=>{
            assert!(replay==terminal,"same original failed event replay must preserve exact PG identity, hash, and category");
        }
    }
    phase.store(31,Ordering::Relaxed); // original await edge closed
    assert!(edges.peek(&child_scope,parent,child).await?.is_none(),"required failed callback must close its original await edge");
    phase.store(32,Ordering::Relaxed); // original goal released
    assert!(goals.get(&child_scope,child).is_err(),"required failed callback must release its original goal");
    if expected==FailedCallbackExpectation::Persisted {
        phase.store(33,Ordering::Relaxed); // failed final text raw canary persisted once
        let raw=std::fs::read(raw_root.join("runs").join(format!("{child}.txt")))?;
        assert!(raw.as_slice()==RAW_CANARY.as_bytes(),"failed callback final text raw bytes must remain original");
    }
    phase.store(34,Ordering::Relaxed); // closed evidence only
    Ok(match expected {
        FailedCallbackExpectation::Rejected=>json!({"schemaVersion":"exporterCallbackPGRejectedFailed/1","actualSubmitClaimFailCommit":true,"requiredLifecycleCallback":true,
            "actualTypedSpawnStart":true,"actualPGRejectObserved":true,"rejectAttempts":1,"originalSpawnedRowUnchanged":true,
            "pgStatementRollbackVerified":true,"storageVerified":false,"sanitizedFailureCategoryAndDetailPreserved":true,
            "committedEventSanitizedReasonPreserved":true,"originalReplayIdentityAndRejectCountUnchanged":true,
            "fixtureResolver":true,"productionCompositionMountQualified":false,"syntheticActor":true,"syntheticProvider":true,
            "modelCalls":0,"normalActorQualified":false,"g01Qualified":false,"gcInvoked":false,"privateBodyOrHashPersisted":false}),
        FailedCallbackExpectation::Persisted=>json!({"schemaVersion":"exporterCallbackTerminalPersisted/1","actualSubmitClaimFailCommit":true,"requiredLifecycleCallback":true,
            "actualTypedSpawnStart":true,"actualTerminalPersisted":true,"terminalStatus":match terminal_kind{HostTerminalKind::Failed=>"Failed",HostTerminalKind::Cancelled=>"Cancelled"},"finalTextHashVerified":true,
            "sanitizedFailureCategoryPersisted":true,"committedEventSanitizedReasonPreserved":true,"originalReplayIdentityUnchanged":true,
            "rawCanaryPersistedExactly":true,"failureCategoryDetail":false,"fixtureResolver":true,"productionCompositionMountQualified":false,
            "fullTianquanSchemaMode":full_tq_schema,"syntheticActor":true,"syntheticProvider":true,"modelCalls":0,"normalActorQualified":false,"g01Qualified":false,"gcInvoked":false})
    })
}

#[tokio::test]
#[ignore = "Root fresh owned PG/wire tables/raw directory only; synthetic callback storage, not normal actor/G01"]
async fn owned_coordinator_failed_callback_pg_reject_preserves_spawned_identity(){
    let original_deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(180);
    let url=std::env::var("TIANQUAN_SPAWN_PG_URL").expect("Root explicit private PG URL required");
    let config:tokio_postgres::Config=url.parse().expect("Root PG configuration syntax required");
    assert!(config.get_hosts().iter().all(|h|matches!(h,tokio_postgres::config::Host::Tcp(host) if host=="127.0.0.1")) && config.get_hosts().len()==1
        && config.get_ports()==&[55432] && config.get_dbname()==Some(std::env::var("TQ_EXPORTER_DATABASE").unwrap().as_str()),"exact Root owned loopback database required");
    let (client,connection)=tokio::time::timeout_at(original_deadline,config.connect(tokio_postgres::NoTls)).await.expect("original deadline").expect("Root owned PG connection required");
    let connection_handle=tokio::spawn(connection);
    let phase=AtomicU8::new(0);
    let observed=tokio::time::timeout_at(original_deadline,run_failed_callback(&client,&phase,FailedCallbackExpectation::Rejected,HostTerminalKind::Failed)).await;
    drop(client);
    // 原connection保同handle，真实EOF/join后才断言；不abort/TempDir删除/续预算。
    let joined=connection_handle.await;
    assert!(matches!(joined,Ok(Ok(()))),"original PG connection must join");
    let proof=match observed.expect("original whole deadline") {
        Ok(value)=>value,
        Err(error)=>{
            let failure=closed_diagnostic(closed_phase(phase.load(Ordering::Relaxed)),error.as_ref());
            eprintln!("EXPORTER_RUN_FAILURE_EVIDENCE={failure}");
            panic!("actual exporter storage caller oracle rejected; closed diagnostic emitted");
        }
    };
    println!("EXPORTER_STORAGE_PG_REJECTED_FAILED_EVIDENCE={proof}");
}


#[tokio::test]
#[ignore = "Root fresh owned PG/wire tables/raw directory only; synthetic failed terminal callback, not normal actor/G01"]
async fn owned_coordinator_failed_callback_exports_failed_terminal(){
    let original_deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(180);
    let url=std::env::var("TIANQUAN_SPAWN_PG_URL").expect("Root explicit private PG URL required");
    let config:tokio_postgres::Config=url.parse().expect("Root PG configuration syntax required");
    assert!(config.get_hosts().iter().all(|h|matches!(h,tokio_postgres::config::Host::Tcp(host) if host=="127.0.0.1")) && config.get_hosts().len()==1
        && config.get_ports()==&[55432] && config.get_dbname()==Some(std::env::var("TQ_EXPORTER_DATABASE").unwrap().as_str()),"exact Root owned loopback database required");
    let (client,connection)=tokio::time::timeout_at(original_deadline,config.connect(tokio_postgres::NoTls)).await.expect("original deadline").expect("Root owned PG connection required");
    let connection_handle=tokio::spawn(connection);
    let phase=AtomicU8::new(0);
    let observed=tokio::time::timeout_at(original_deadline,run_failed_callback(&client,&phase,FailedCallbackExpectation::Persisted,HostTerminalKind::Failed)).await;
    drop(client);
    let joined=connection_handle.await;
    assert!(matches!(joined,Ok(Ok(()))),"original PG connection must join");
    let proof=match observed.expect("original whole deadline") {
        Ok(value)=>value,
        Err(error)=>{
            let failure=closed_diagnostic(closed_phase(phase.load(Ordering::Relaxed)),error.as_ref());
            eprintln!("EXPORTER_RUN_FAILURE_EVIDENCE={failure}");
            panic!("actual failed exporter storage caller oracle rejected; closed diagnostic emitted");
        }
    };
    println!("EXPORTER_STORAGE_FAILED_TERMINAL_EVIDENCE={proof}");
}

#[tokio::test]
#[ignore = "Root fresh owned PG/wire tables/raw directory only; synthetic cancelled terminal callback, not normal actor/G01"]
async fn owned_coordinator_cancelled_callback_exports_cancelled_terminal(){
    let original_deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(180);
    let url=std::env::var("TIANQUAN_SPAWN_PG_URL").expect("Root explicit private PG URL required");
    let config:tokio_postgres::Config=url.parse().expect("Root PG configuration syntax required");
    assert!(config.get_hosts().iter().all(|h|matches!(h,tokio_postgres::config::Host::Tcp(host) if host=="127.0.0.1")) && config.get_hosts().len()==1
        && config.get_ports()==&[55432] && config.get_dbname()==Some(std::env::var("TQ_EXPORTER_DATABASE").unwrap().as_str()),"exact Root owned loopback database required");
    let (client,connection)=tokio::time::timeout_at(original_deadline,config.connect(tokio_postgres::NoTls)).await.expect("original deadline").expect("Root owned PG connection required");
    let connection_handle=tokio::spawn(connection);
    let phase=AtomicU8::new(0);
    let observed=tokio::time::timeout_at(original_deadline,run_failed_callback(&client,&phase,FailedCallbackExpectation::Persisted,HostTerminalKind::Cancelled)).await;
    drop(client);
    let joined=connection_handle.await;
    assert!(matches!(joined,Ok(Ok(()))),"original PG connection must join");
    let proof=match observed.expect("original whole deadline") {
        Ok(value)=>value,
        Err(error)=>{
            let failure=closed_diagnostic(closed_phase(phase.load(Ordering::Relaxed)),error.as_ref());
            eprintln!("EXPORTER_RUN_FAILURE_EVIDENCE={failure}");
            panic!("actual cancelled exporter storage caller oracle rejected; closed diagnostic emitted");
        }
    };
    println!("EXPORTER_STORAGE_CANCELLED_TERMINAL_EVIDENCE={proof}");
}
