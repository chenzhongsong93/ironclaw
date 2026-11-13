//! H2 mechanical captured-parent novelist exporter. Official callback only; no model/normal actor.
#![cfg(feature = "filesystem-goal-store")]
use ironclaw_filesystem::{CasExpectation, Entry, InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::{
    AgentId, MountAlias, MountGrant, MountPermissions, MountView, ProjectId, ResourceScope,
    ScopedPath, TenantId, ThreadId, UserId, VirtualPath,
};
use ironclaw_loop_host::{
    AwaitEdgeWriter, AwaitedChildSetRecord, SpawnSubagentMode, SubagentKindId,
};
use ironclaw_runner::subagent::await_edge::{
    resolver::AwaitEdgeResolver, store::FilesystemAwaitEdgeStore,
};
use ironclaw_runner::subagent::goal_store::{InMemoryBoundedSubagentGoalStore, SubagentGoal};
use ironclaw_threads::{InMemorySessionThreadService, SessionThreadService, ThreadScope};
use ironclaw_turns::runner::TurnRunTransitionPort;
use ironclaw_turns::{
    DefaultTurnCoordinator, DefaultTurnLifecycleEventBus, LifecyclePublicationErrorPort,
    LifecyclePublishingTurnStateStore, TurnCommittedEventObserver, TurnCoordinator, TurnEventSink,
    TurnLifecycleEventBus, TurnRunId, TurnScope, TurnSpawnTreePort, TurnStatus,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// 每op从真实ResourceScope产生typed动态目标；非固定tenant/view，不声称composition完整catalog。
fn fixture_mounts(scope: &ResourceScope) -> Result<MountView, ironclaw_host_api::HostApiError> {
    // roster touch/prune显式用system scope；只给其系统roster子树，不能拼保留sentinel到owner路径。
    if scope.is_system() {
        return MountView::new(vec![MountGrant::new(
            MountAlias::new("/turns/subagent-await-scopes")?,
            VirtualPath::new("/turns/subagent-await-scopes")?,
            MountPermissions::read_write_list_delete(),
        )]);
    }
    let agent = scope.agent_id.as_ref().map_or("__none__", |id| id.as_str());
    let project = scope
        .project_id
        .as_ref()
        .map_or("__none__", |id| id.as_str());
    let owner_root = format!(
        "/tenants/{}/users/{}/agents/{agent}/projects/{project}",
        scope.tenant_id.as_str(),
        scope.user_id.as_str()
    );
    MountView::new(vec![
        MountGrant::new(
            MountAlias::new("/engine")?,
            VirtualPath::new(format!("{owner_root}/engine"))?,
            MountPermissions::read_write_list_delete(),
        ),
        MountGrant::new(
            MountAlias::new("/turns")?,
            VirtualPath::new(format!("{owner_root}/turns"))?,
            MountPermissions::read_write_list_delete(),
        ),
    ])
}
fn scope(owner: &str, project: &str, thread: &str) -> TurnScope {
    TurnScope::new_with_owner(
        TenantId::new(owner).unwrap(),
        Some(AgentId::new("mechanical-exporter-agent").unwrap()),
        Some(ProjectId::new(project).unwrap()),
        ThreadId::new(thread).unwrap(),
        Some(UserId::new(owner).unwrap()),
    )
}
struct AllowResultWriter;
#[async_trait::async_trait]
impl ironclaw_loop_host::LoopCapabilityResultWriter for AllowResultWriter {
    async fn write_capability_result(
        &self,
        write: ironclaw_loop_host::CapabilityResultWrite<'_>,
    ) -> Result<
        ironclaw_loop_host::CapabilityWriteResult,
        ironclaw_turns::run_profile::AgentLoopHostError,
    > {
        Ok(
            ironclaw_loop_host::CapabilityWriteResult::without_output_digest(
                ironclaw_turns::LoopResultRef::new(format!("result:{}", write.capability_id))
                    .unwrap(),
                0,
            ),
        )
    }
    async fn update_capability_result(
        &self,
        _context: &ironclaw_turns::run_profile::LoopRunContext,
        _reference: &ironclaw_turns::LoopResultRef,
        output: Value,
    ) -> Result<u64, ironclaw_turns::run_profile::AgentLoopHostError> {
        Ok(serde_json::to_vec(&output)
            .map(|value| value.len() as u64)
            .unwrap_or(0))
    }
}
#[derive(PartialEq, Eq)]
struct WireRow {
    parent: Option<String>,
    kind: String,
    project: Option<String>,
    layer: Option<String>,
    terminal: Option<String>,
    hash: Option<String>,
    failure: Option<String>,
    spawned: chrono::DateTime<chrono::Utc>,
}
async fn read_row(client: &tokio_postgres::Client, child: TurnRunId) -> TestResult<WireRow> {
    let row=client.query_one("SELECT parent_run_id,subagent_type,project_id,layer,terminal_status,final_text_hash,failure_category,spawned_at FROM spawn_records WHERE child_run_id=$1",&[&child.to_string()]).await?;
    Ok(WireRow {
        parent: row.get(0),
        kind: row.get(1),
        project: row.get(2),
        layer: row.get(3),
        terminal: row.get(4),
        hash: row.get(5),
        failure: row.get(6),
        spawned: row.get(7),
    })
}
async fn run(
    client: &tokio_postgres::Client,
    phase: &AtomicU8,
    input: &Input,
) -> TestResult<Value> {
    let identity=client.query_one("SELECT current_database(),current_schema(),current_user,obj_description(to_regnamespace(current_schema()),'pg_namespace')",&[]).await?;
    assert!(
        identity.get::<_, String>(0) == input.database
            && identity.get::<_, String>(1) == input.schema
            && identity.get::<_, String>(2) == input.role
            && identity.get::<_, Option<String>>(3) == Some(input.namespace_comment.clone()),
        "namespace identity rejected"
    );
    let parent_identity=client.query_one("SELECT thread_id,project_id,owner_user_id,chapter_no,legacy_layer FROM tianquan_run_bindings WHERE run_id=$1",&[&input.parent_run]).await?;
    assert!(
        parent_identity.get::<_, String>(0) == input.thread
            && parent_identity.get::<_, String>(1) == input.project
            && parent_identity.get::<_, String>(2) == input.owner
            && parent_identity.get::<_, Option<i32>>(3).is_none()
            && parent_identity.get::<_, Option<String>>(4).is_none(),
        "official parent identity rejected"
    );
    let count = client
        .query_one(
            "SELECT count(*) FROM spawn_records WHERE parent_run_id=$1",
            &[&input.parent_run],
        )
        .await?;
    assert!(
        count.get::<_, i64>(0) == 0,
        "fresh parent must not adopt old child"
    );
    let raw_root = std::path::PathBuf::from(&input.raw_directory);
    assert!(
        raw_root.is_absolute() && raw_root.is_dir(),
        "explicit raw root required"
    );
    phase.store(4, Ordering::Relaxed); // FixtureMounts
    let root = Arc::new(InMemoryBackend::new());
    let fs = Arc::new(ScopedFilesystem::new(root.clone(), fixture_mounts));
    phase.store(5, Ordering::Relaxed); // FixtureIsolation
    // Fixture mounts are dynamic for the captured owner/project; not production mount qualification.
    phase.store(6, Ordering::Relaxed); // Services
    let edges = Arc::new(FilesystemAwaitEdgeStore::new(fs));
    let base = Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let threads = Arc::new(InMemorySessionThreadService::default());
    let goals = Arc::new(InMemoryBoundedSubagentGoalStore::new());
    let goal_port: Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore> = goals.clone();
    let state_port: Arc<dyn ironclaw_turns::TurnSpawnTreeStateStore> = base.clone();
    let resolver = Arc::new(AwaitEdgeResolver::new_unbound(
        edges.clone(),
        goal_port,
        state_port,
        Arc::new(AllowResultWriter),
        threads.clone(),
    ));
    let bus = Arc::new(DefaultTurnLifecycleEventBus::new());
    phase.store(7, Ordering::Relaxed); // LifecycleSubscriptions
    bus.subscribe_required(resolver.clone() as Arc<dyn TurnCommittedEventObserver>)?;
    let captured = Arc::new(ironclaw_turns::InMemoryTurnEventSink::default());
    bus.subscribe_best_effort(captured.clone() as Arc<dyn TurnEventSink>)?;
    let published = Arc::new(LifecyclePublishingTurnStateStore::new(
        base.clone(),
        bus.clone(),
    ));
    let error_port: Arc<dyn LifecyclePublicationErrorPort> = published.clone();
    let coordinator = Arc::new(
        DefaultTurnCoordinator::new(published.clone())
            .with_lifecycle_publication_error_port(error_port),
    );
    phase.store(8, Ordering::Relaxed); // BindCoordinator
    resolver.bind_coordinator(coordinator.clone() as Arc<dyn TurnCoordinator>)?;
    phase.store(9, Ordering::Relaxed); // Scopes
    let parent_scope = scope(&input.owner, &input.project, &input.thread);
    let child_scope = scope(
        &input.owner,
        &input.project,
        &format!("mechanical-child-{}", input.parent_run),
    );
    let actor = ironclaw_turns::TurnActor::new(UserId::new(&input.owner)?);
    phase.store(10, Ordering::Relaxed); // ParentSubmit
    let ironclaw_turns::SubmitTurnResponse::Accepted { run_id: parent, .. } = coordinator
        .submit_turn(ironclaw_turns::SubmitTurnRequest {
            requested_model: None,
            scope: parent_scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new("msg:synthetic-parent")?,
            source_binding_ref: ironclaw_turns::SourceBindingRef::new("source:synthetic-parent")?,
            reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                "reply:synthetic-parent",
            )?,
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:synthetic-parent")?,
            received_at: chrono::Utc::now(),
            requested_run_id: Some(input.parent_run.parse()?),
            parent_run_id: None,
            subagent_depth: 0,
            spawn_tree_root_run_id: None,
            product_context: None,
            llm_subject: None,
        })
        .await?;
    let parent_runner = ironclaw_turns::TurnRunnerId::new();
    let parent_lease = ironclaw_turns::TurnLeaseToken::new();
    phase.store(11, Ordering::Relaxed); // ParentClaim
    assert!(
        published
            .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
                runner_id: parent_runner,
                lease_token: parent_lease,
                scope_filter: Some(parent_scope.clone())
            })
            .await?
            .is_some(),
        "actual parent claim required"
    );
    assert!(
        parent.to_string() == input.parent_run,
        "coordinator must retain actual POST parent"
    );
    phase.store(13, Ordering::Relaxed); // ChildSubmit
    let ironclaw_turns::SubmitTurnResponse::Accepted { run_id: child, .. } = coordinator
        .submit_child_run(ironclaw_turns::SubmitChildRunRequest {
            parent_scope: parent_scope.clone(),
            parent_run_id: parent,
            child_scope: child_scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new("msg:synthetic-child")?,
            source_binding_ref: ironclaw_turns::SourceBindingRef::new("source:synthetic-child")?,
            reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                "reply:synthetic-child",
            )?,
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:synthetic-child")?,
            received_at: chrono::Utc::now(),
            requested_run_id: None,
            spawn_tree_descendant_cap: 16,
        })
        .await?;
    phase.store(14, Ordering::Relaxed); // AwaitRecordTypes
    let gate = ironclaw_turns::GateRef::new("gate:synthetic-child")?;
    let reference = ironclaw_turns::LoopResultRef::new("result:synthetic-child")?;
    let mut context =
        ironclaw_agent_loop::test_support::test_run_context("synthetic-exporter-parent");
    context.scope = parent_scope.clone();
    context.thread_id = parent_scope.thread_id.clone();
    context.run_id = parent;
    context.actor = Some(actor);
    let kind = SubagentKindId::new("novelist")?;
    phase.store(15, Ordering::Relaxed); // AwaitRecord
    edges
        .record_awaited_child(AwaitedChildSetRecord {
            gate_ref: gate.clone(),
            parent_run_context: context,
            tree_root_run_id: parent,
            child_scope: child_scope.clone(),
            child_run_id: child,
            child_thread_id: child_scope.thread_id.clone(),
            source_binding_ref: ironclaw_turns::SourceBindingRef::new(format!(
                "subagent-source:{child}"
            ))?,
            reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(format!(
                "subagent-reply:{child}"
            ))?,
            subagent_kind: kind.clone(),
            spawn_capability_id: ironclaw_host_api::CapabilityId::new(
                ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID,
            )?,
            result_ref: reference.clone(),
            mode: SpawnSubagentMode::Blocking,
        })
        .await?;
    phase.store(16, Ordering::Relaxed); // GoalStore
    goals.put(
        &child_scope,
        child,
        SubagentGoal {
            task: "synthetic callback storage only".into(),
            handoff: None,
        },
    )?;
    phase.store(17, Ordering::Relaxed); // ThreadScope
    let thread_scope = ThreadScope {
        tenant_id: parent_scope.tenant_id.clone(),
        agent_id: parent_scope.agent_id.clone().unwrap(),
        project_id: parent_scope.project_id.clone(),
        owner_user_id: Some(UserId::new(&input.owner)?),
        mission_id: None,
    };
    phase.store(18, Ordering::Relaxed); // EnsureThreads
    for thread in [&parent_scope.thread_id, &child_scope.thread_id] {
        threads
            .ensure_thread(ironclaw_threads::EnsureThreadRequest {
                scope: thread_scope.clone(),
                thread_id: Some(thread.clone()),
                created_by_actor_id: "synthetic-exporter".into(),
                title: None,
                metadata_json: None,
            })
            .await?;
    }
    phase.store(19, Ordering::Relaxed); // ToolResultReference
    threads
        .append_tool_result_reference(ironclaw_threads::AppendToolResultReferenceRequest {
            scope: thread_scope.clone(),
            thread_id: parent_scope.thread_id.clone(),
            turn_run_id: parent.to_string(),
            result_ref: reference.as_str().into(),
            safe_summary: ironclaw_threads::ToolResultSafeSummary::new(
                "synthetic subagent spawned",
            )?,
            provider_call: None,
            model_observation: None,
        })
        .await?;
    phase.store(20, Ordering::Relaxed); // FinalizedMessage
    threads
        .append_finalized_assistant_message(
            ironclaw_threads::AppendFinalizedAssistantMessageRequest {
                scope: thread_scope,
                thread_id: child_scope.thread_id.clone(),
                turn_run_id: child.to_string(),
                content: ironclaw_threads::MessageContent::text(&input.candidate_json),
            },
        )
        .await?;
    // 正式typed spawn出口；不直接构造ProvenanceRecord、不SQL写Spawned/Completed。
    phase.store(21, Ordering::Relaxed); // TypedSpawnStart
    edges
        .record_child_submitted(&child_scope, parent, child, &kind, chrono::Utc::now())
        .await;
    phase.store(22, Ordering::Relaxed); // StartRow
    let start = read_row(client, child).await?;
    assert!(
        start.parent.as_deref() == Some(parent.to_string().as_str())
            && start.kind == "novelist"
            && start.project.as_deref() == Some(input.project.as_str())
            && start.layer.is_none()
            && start.terminal.as_deref() == Some("Spawned")
            && start.hash.is_none(),
        "actual typed start PG identity required"
    );
    phase.store(23, Ordering::Relaxed); // ParentBlock
    published
        .block_run(ironclaw_turns::runner::BlockRunRequest {
            run_id: parent,
            runner_id: parent_runner,
            lease_token: parent_lease,
            checkpoint_id: ironclaw_turns::TurnCheckpointId::new(),
            state_ref: ironclaw_turns::run_profile::LoopCheckpointStateRef::new(
                "checkpoint:synthetic-parent",
            )?,
            reason: ironclaw_turns::BlockedReason::AwaitDependentRun { gate_ref: gate },
        })
        .await?;
    phase.store(24, Ordering::Relaxed); // ChildLease
    let child_runner = ironclaw_turns::TurnRunnerId::new();
    let child_lease = ironclaw_turns::TurnLeaseToken::new();
    phase.store(25, Ordering::Relaxed); // ChildClaim
    assert!(
        published
            .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
                runner_id: child_runner,
                lease_token: child_lease,
                scope_filter: Some(child_scope.clone())
            })
            .await?
            .is_some(),
        "actual child claim required"
    );
    phase.store(26, Ordering::Relaxed); // CompleteRun
    let committed = published
        .complete_run(ironclaw_turns::runner::CompleteRunRequest {
            run_id: child,
            runner_id: child_runner,
            lease_token: child_lease,
        })
        .await?;
    assert!(
        committed.status == TurnStatus::Completed,
        "actual committed child terminal required"
    );
    phase.store(27, Ordering::Relaxed); // TerminalRow
    let first = read_row(client, child).await?;
    let expected_hash = Sha256::digest(input.candidate_json.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(
        first.parent == start.parent
            && first.kind == start.kind
            && first.project == start.project
            && first.layer == start.layer
            && first.spawned == start.spawned
            && first.terminal.as_deref() == Some("Completed")
            && first.hash.as_deref() == Some(expected_hash.as_str())
            && first.failure.is_none(),
        "actual callback PG terminal and hash must match"
    );
    phase.store(28, Ordering::Relaxed); // CapturedEvent
    let original_event = captured
        .events()
        .into_iter()
        .find(|event| event.run_id == child && event.status == TurnStatus::Completed)
        .ok_or("actual committed lifecycle event missing")?;
    phase.store(29, Ordering::Relaxed); // ReplayEvent
    bus.publish_event(original_event).await?;
    phase.store(30, Ordering::Relaxed); // ReplayRow
    let after = read_row(client, child).await?;
    assert!(
        first == after,
        "same original committed callback replay must preserve exact PG identity/hash"
    );
    phase.store(31, Ordering::Relaxed); // EdgeClosed
    assert!(
        edges.peek(&child_scope, parent, child).await?.is_none(),
        "actual terminal callback must close its original await edge"
    );
    phase.store(32, Ordering::Relaxed); // GoalReleased
    assert!(
        goals.get(&child_scope, child).is_err(),
        "actual terminal callback must release its original goal"
    );
    phase.store(33, Ordering::Relaxed); // RawCanaryRead
    let raw = std::fs::read(raw_root.join("runs").join(format!("{child}.txt")))?;
    assert!(
        raw.as_slice() == input.candidate_json.as_bytes(),
        "actual synthetic raw bytes must remain original"
    );
    phase.store(34, Ordering::Relaxed); // SuccessProjection
    Ok(
        json!({"schemaVersion":"exporterCallbackStorageCompletedReplay/1","actualSubmitClaimCommit":true,"requiredLifecycleCallback":true,"actualTypedSpawnStart":true,
        "pgIdentityAndTerminalVerified":true,"rawHashMatchesInRam":true,"replayIdentityAndHashUnchanged":true,"fixtureResolver":true,"fixtureTwoTenantUserPathsIsolated":false,
        "productionCompositionMountQualified":false,"syntheticActor":true,"syntheticProvider":true,"modelCalls":0,"normalActorQualified":false,"g01Qualified":false,"gcInvoked":false,"privateBodyOrHashPersisted":false,"parentRun":parent.to_string(),"childRun":child.to_string(),"officialPostParentMatched":true}),
    )
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    case_selector: String,
    database_url: String,
    database: String,
    schema: String,
    role: String,
    namespace_comment: String,
    owner: String,
    project: String,
    thread: String,
    parent_run: String,
    raw_directory: String,
    candidate_json: String,
    deadline_unix_ms: i64,
}
struct ProcessExporterConfig;
impl Drop for ProcessExporterConfig {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("TIANQUAN_SPAWN_PG_URL");
            std::env::remove_var("TIANQUAN_SPAWN_PROSE_DIR");
        }
    }
}
fn main() {
    use std::io::Read;
    let mut bytes = Vec::new();
    assert!(
        std::io::stdin()
            .take(524289)
            .read_to_end(&mut bytes)
            .is_ok(),
        "private stdin rejected"
    );
    assert!(bytes.len() <= 524288, "private stdin budget");
    let input: Input =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("private input rejected"));
    bytes.fill(0);
    drop(bytes);
    assert!(
        input.case_selector == "h2-captured-parent-novelist-v1"
            && input.candidate_json.len() <= 262144,
        "fixed case rejected"
    );
    assert!(
        input.schema.starts_with("atomic_world_")
            && input
                .schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "namespace rejected"
    );
    let left = input.deadline_unix_ms - chrono::Utc::now().timestamp_millis();
    assert!(
        (5001..=120000).contains(&left),
        "original deadline rejected"
    );
    let config: tokio_postgres::Config = input
        .database_url
        .parse()
        .unwrap_or_else(|_| panic!("private PG config rejected"));
    // Single dedicated process; set official callback config before creating any runtime/thread.
    assert!(
        std::env::var_os("TIANQUAN_SPAWN_PG_URL").is_none()
            && std::env::var_os("TIANQUAN_SPAWN_PROSE_DIR").is_none(),
        "do not adopt ambient exporter config"
    );
    unsafe {
        std::env::set_var("TIANQUAN_SPAWN_PG_URL", &input.database_url);
        std::env::set_var("TIANQUAN_SPAWN_PROSE_DIR", &input.raw_directory);
    }
    let config_guard = ProcessExporterConfig;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| {
            unsafe {
                std::env::remove_var("TIANQUAN_SPAWN_PG_URL");
                std::env::remove_var("TIANQUAN_SPAWN_PROSE_DIR");
            }
            panic!("runtime rejected; exporter config cleared")
        });
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            // Re-evaluate the ORIGINAL Unix end after runtime construction, never now+old left.
            let remaining = input.deadline_unix_ms - chrono::Utc::now().timestamp_millis();
            if remaining <= 5000 {
                return Err(());
            }
            let end =
                tokio::time::Instant::now() + std::time::Duration::from_millis(remaining as u64);
            let operation_end = end - std::time::Duration::from_secs(5);
            let (client, connection) =
                tokio::time::timeout_at(operation_end, config.connect(tokio_postgres::NoTls))
                    .await
                    .map_err(|_| ())?
                    .map_err(|_| ())?;
            let handle = tokio::spawn(connection);
            let phase = AtomicU8::new(0);
            let client = Arc::new(client);
            let business_client = client.clone();
            let business_input = input.clone();
            let mut original =
                tokio::spawn(async move { run(&business_client, &phase, &business_input).await });
            let result = match tokio::time::timeout_at(operation_end, &mut original).await {
                Ok(Ok(value)) => value.map_err(|_| ()),
                Ok(Err(_)) => Err(()),
                Err(_) => {
                    let _original_settlement = tokio::time::timeout_at(
                        end - std::time::Duration::from_secs(3),
                        &mut original,
                    )
                    .await;
                    Err(())
                }
            };
            drop(client);
            let joined = tokio::time::timeout_at(end, handle).await;
            if !matches!(joined, Ok(Ok(Ok(())))) {
                return Err(());
            }
            if chrono::Utc::now().timestamp_millis() >= input.deadline_unix_ms {
                return Err(());
            }
            match result {
                Ok(proof) => Ok(proof),
                _ => Err(()),
            }
        })
    }));
    drop(runtime);
    drop(config_guard);
    match observed {
        Ok(Ok(proof)) => println!("H2_EXPORTER_PROOF={proof}"),
        _ => panic!("mechanical exporter failed; private error withheld"),
    }
}
