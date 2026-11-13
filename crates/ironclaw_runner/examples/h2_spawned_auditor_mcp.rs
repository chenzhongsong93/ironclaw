//! H2 mechanical independent Spawned actor to production signed MCP. No model/normal actor.
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
    let positive = input.case_selector == "h2-c-auditor-blocked-v1";
    let role = if input.case_selector == "h2-c-writer-denied-v1" {
        "novelist"
    } else {
        "auditor"
    };
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
    let producers=client.query("SELECT child_run_id FROM spawn_records WHERE parent_run_id=$1 AND project_id=$2 AND subagent_type='novelist' AND layer IS NULL AND terminal_status='Completed'",&[&input.parent_run,&input.project]).await?;
    if producers.len() != 1 || producers[0].get::<_, String>(0) != input.producer_child_run {
        return Err("original unique producer required".into());
    }
    let active=client.query_one("SELECT count(*) FROM spawn_records WHERE parent_run_id=$1 AND terminal_status='Spawned'",&[&input.parent_run]).await?;
    if active.get::<_, i64>(0) != 0 {
        return Err("do not adopt another active child".into());
    }
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
    // All bounds derive once from the ORIGINAL Unix end, including failure cleanup.
    let remaining = input.deadline_unix_ms - chrono::Utc::now().timestamp_millis();
    if remaining <= 25000 {
        return Err("owned lifecycle reserve missing".into());
    }
    let original_end =
        tokio::time::Instant::now() + std::time::Duration::from_millis(remaining as u64);
    let business_end = original_end - std::time::Duration::from_secs(20);
    let child_cleanup_end = original_end - std::time::Duration::from_secs(8);
    let parent_cleanup_end = original_end - std::time::Duration::from_secs(6);
    let parent_runner = ironclaw_turns::TurnRunnerId::new();
    let parent_lease = ironclaw_turns::TurnLeaseToken::new();
    let child_runner = ironclaw_turns::TurnRunnerId::new();
    let child_lease = ironclaw_turns::TurnLeaseToken::new();
    let mirror_parent: TurnRunId = input.parent_run.parse()?;
    let planned_child = TurnRunId::new();
    let mut owned_child = None;
    let mut child_claimed = false;
    use futures_util::FutureExt;
    let outcome = {
        let business = async {
            phase.store(10, Ordering::Relaxed); // ParentSubmit
            let ironclaw_turns::SubmitTurnResponse::Accepted { run_id: parent, .. } = coordinator
                .submit_turn(ironclaw_turns::SubmitTurnRequest {
                    requested_model: None,
                    scope: parent_scope.clone(),
                    actor: actor.clone(),
                    accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new(
                        "msg:synthetic-parent",
                    )?,
                    source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                        "source:synthetic-parent",
                    )?,
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
            phase.store(11, Ordering::Relaxed); // ParentClaim
            if !(published
                .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
                    runner_id: parent_runner,
                    lease_token: parent_lease,
                    scope_filter: Some(parent_scope.clone()),
                })
                .await?
                .is_some())
            {
                return Err("actual parent claim required".into());
            }
            if !(parent.to_string() == input.parent_run) {
                return Err("coordinator must retain actual POST parent".into());
            }
            owned_child = Some(planned_child);
            phase.store(13, Ordering::Relaxed); // ChildSubmit
            let ironclaw_turns::SubmitTurnResponse::Accepted { run_id: child, .. } = coordinator
                .submit_child_run(ironclaw_turns::SubmitChildRunRequest {
                    parent_scope: parent_scope.clone(),
                    parent_run_id: parent,
                    child_scope: child_scope.clone(),
                    actor: actor.clone(),
                    accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new(
                        "msg:synthetic-child",
                    )?,
                    source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                        "source:synthetic-child",
                    )?,
                    reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                        "reply:synthetic-child",
                    )?,
                    requested_run_profile: None,
                    idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:synthetic-child")?,
                    received_at: chrono::Utc::now(),
                    requested_run_id: Some(planned_child),
                    spawn_tree_descendant_cap: 16,
                })
                .await?;
            owned_child = Some(child);
            phase.store(14, Ordering::Relaxed); // AwaitRecordTypes
            let gate = ironclaw_turns::GateRef::new("gate:synthetic-child")?;
            let reference = ironclaw_turns::LoopResultRef::new("result:synthetic-child")?;
            let mut context =
                ironclaw_agent_loop::test_support::test_run_context("synthetic-exporter-parent");
            context.scope = parent_scope.clone();
            context.thread_id = parent_scope.thread_id.clone();
            context.run_id = parent;
            context.actor = Some(actor.clone());
            let kind = SubagentKindId::new(role)?;
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
            // 正式typed spawn出口；不直接构造ProvenanceRecord、不SQL写Spawned/Completed。
            phase.store(21, Ordering::Relaxed); // TypedSpawnStart
            edges
                .record_child_submitted(&child_scope, parent, child, &kind, chrono::Utc::now())
                .await;
            phase.store(22, Ordering::Relaxed); // StartRow
            let start = read_row(client, child).await?;
            if !(start.parent.as_deref() == Some(parent.to_string().as_str())
                && start.kind == role
                && start.project.as_deref() == Some(input.project.as_str())
                && start.layer.is_none()
                && start.terminal.as_deref() == Some("Spawned")
                && start.hash.is_none())
            {
                return Err("actual typed start PG identity required".into());
            }
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
            phase.store(25, Ordering::Relaxed); // ChildClaim
            if !(published
                .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
                    runner_id: child_runner,
                    lease_token: child_lease,
                    scope_filter: Some(child_scope.clone()),
                })
                .await?
                .is_some())
            {
                return Err("actual child claim required".into());
            }
            child_claimed = true;
            // PG is still the official Spawned row while production MCP authenticates.
            let still_spawned = read_row(client, child).await?;
            if still_spawned.terminal.as_deref() != Some("Spawned") {
                return Err("signed call requires original Spawned actor".into());
            }
            let exchange = signed_audit(input, &child_scope, child).await;
            // A protocol failure never skips the official terminal/callback cleanup below.
            let accepted = exchange.as_ref().is_ok_and(|value| value.accepted);
            let output = exchange
                .as_ref()
                .ok()
                .and_then(|value| value.output.clone());
            let protocol = exchange
                .as_ref()
                .ok()
                .map(|value| value.protocol.clone())
                .unwrap_or_default();
            let final_text = serde_json::to_string(
                &output
                    .clone()
                    .unwrap_or_else(|| json!({"mechanicalProtocolRejectedOrUnknown":true})),
            )?;
            let appended = threads
                .append_finalized_assistant_message(
                    ironclaw_threads::AppendFinalizedAssistantMessageRequest {
                        scope: thread_scope,
                        thread_id: child_scope.thread_id.clone(),
                        turn_run_id: child.to_string(),
                        content: ironclaw_threads::MessageContent::text(&final_text),
                    },
                )
                .await;
            phase.store(26, Ordering::Relaxed); // Official terminal even for rejected control
            let committed = if positive && accepted && appended.is_ok() {
                published
                    .complete_run(ironclaw_turns::runner::CompleteRunRequest {
                        run_id: child,
                        runner_id: child_runner,
                        lease_token: child_lease,
                    })
                    .await?
            } else {
                coordinator
                    .cancel_run(ironclaw_turns::CancelRunRequest {
                        scope: child_scope.clone(),
                        actor: actor.clone(),
                        run_id: child,
                        reason: ironclaw_turns::SanitizedCancelReason::UserRequested,
                        idempotency_key: ironclaw_turns::IdempotencyKey::new("h2-c-child-cleanup")?,
                    })
                    .await?;
                published
                    .cancel_run(ironclaw_turns::runner::CancelRunCompletionRequest {
                        run_id: child,
                        runner_id: child_runner,
                        lease_token: child_lease,
                    })
                    .await?
            };
            let expected_terminal = if positive && accepted && appended.is_ok() {
                TurnStatus::Completed
            } else {
                TurnStatus::Cancelled
            };
            if committed.status != expected_terminal {
                return Err("official terminal mismatch".into());
            }
            phase.store(27, Ordering::Relaxed); // TerminalRow
            let first = read_row(client, child).await?;
            let expected_hash = Sha256::digest(final_text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if !(first.parent == start.parent
                && first.kind == start.kind
                && first.project == start.project
                && first.layer == start.layer
                && first.spawned == start.spawned
                && first.terminal.as_deref()
                    == Some(if expected_terminal == TurnStatus::Completed {
                        "Completed"
                    } else {
                        "Cancelled"
                    })
                && first.hash.as_deref() == Some(expected_hash.as_str())
                && (expected_terminal != TurnStatus::Completed || first.failure.is_none()))
            {
                return Err("actual callback PG terminal and hash must match".into());
            }
            phase.store(28, Ordering::Relaxed); // CapturedEvent
            let original_event = captured
                .events()
                .into_iter()
                .find(|event| event.run_id == child && event.status == expected_terminal)
                .ok_or("actual committed lifecycle event missing")?;
            phase.store(29, Ordering::Relaxed); // ReplayEvent
            bus.publish_event(original_event).await?;
            phase.store(30, Ordering::Relaxed); // ReplayRow
            let after = read_row(client, child).await?;
            if !(first == after) {
                return Err(
                    "same original committed callback replay must preserve exact PG identity/hash"
                        .into(),
                );
            }
            phase.store(31, Ordering::Relaxed); // EdgeClosed
            if !(edges.peek(&child_scope, parent, child).await?.is_none()) {
                return Err("actual terminal callback must close its original await edge".into());
            }
            phase.store(32, Ordering::Relaxed); // GoalReleased
            if !(goals.get(&child_scope, child).is_err()) {
                return Err("actual terminal callback must release its original goal".into());
            }
            phase.store(33, Ordering::Relaxed); // RawCanaryRead
            let raw = std::fs::read(raw_root.join("runs").join(format!("{child}.txt")))?;
            if !(raw.as_slice() == final_text.as_bytes()) {
                return Err("actual synthetic raw bytes must remain original".into());
            }
            if !accepted || appended.is_err() {
                return Err("original MCP control failed; actor terminal retained".into());
            }
            Ok(
                json!({"schemaVersion":"H2OfficialSpawnedMcpActor/1","caseSelector":input.case_selector,
        "parentRun":parent.to_string(),"childRun":child.to_string(),"role":role,
        "terminal":if expected_terminal==TurnStatus::Completed {"Completed"}else{"Cancelled"},
        "actualTypedSpawnStart":true,"actualClaim":true,"requiredLifecycleCallback":true,
        "originalSpawnedDuringSignedMcp":true,"productionMcpClientAndSigner":true,
        "protocol":protocol,"mcpOutput":output,"callbackReplayUnchanged":true,"parentMirrorCancelled":true,
        "normalActorQualified":false,"humanApproval":false,"signedReceiptIssued":false}),
            )
        };
        tokio::time::timeout_at(
            business_end,
            std::panic::AssertUnwindSafe(business).catch_unwind(),
        )
        .await
    };
    // Preserve the original Result/panic/timeout. These finally paths run even
    // after read/claim/terminal callback failures; none rewrites a prior unknown.
    let child_cleanup = async {
        if let Some(child) = owned_child {
            let requested = coordinator
                .cancel_run(ironclaw_turns::CancelRunRequest {
                    scope: child_scope.clone(),
                    actor: actor.clone(),
                    run_id: child,
                    reason: ironclaw_turns::SanitizedCancelReason::UserRequested,
                    idempotency_key: ironclaw_turns::IdempotencyKey::new("h2-c-finally-child")?,
                })
                .await?;
            if !matches!(
                requested.status,
                TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::Failed
            ) {
                if !child_claimed {
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                        "child lease unknown during cleanup".into(),
                    );
                }
                published
                    .cancel_run(ironclaw_turns::runner::CancelRunCompletionRequest {
                        run_id: child,
                        runner_id: child_runner,
                        lease_token: child_lease,
                    })
                    .await?;
            }
            let state = coordinator
                .get_run_state(ironclaw_turns::GetRunStateRequest {
                    scope: child_scope.clone(),
                    run_id: child,
                })
                .await?;
            if !matches!(
                state.status,
                TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::Failed
            ) {
                return Err("child terminal unknown".into());
            }
            let event = captured
                .events()
                .into_iter()
                .find(|event| event.run_id == child && event.status == state.status)
                .ok_or("original terminal callback event unavailable")?;
            bus.publish_event(event).await?; // same committed official event, never a fabricated terminal
            let row = read_row(client, child).await?;
            let expected = match state.status {
                TurnStatus::Completed => "Completed",
                TurnStatus::Cancelled => "Cancelled",
                _ => "Failed",
            };
            if row.parent.as_deref() != Some(input.parent_run.as_str())
                || row.project.as_deref() != Some(input.project.as_str())
                || row.kind != role
                || row.layer.is_some()
                || row.terminal.as_deref() != Some(expected)
            {
                return Err("official callback terminal unobserved".into());
            }
            if edges
                .peek(&child_scope, mirror_parent, child)
                .await?
                .is_some()
                || goals.get(&child_scope, child).is_ok()
            {
                return Err("official await edge/goal closure unknown".into());
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };
    let child_closed = matches!(
        tokio::time::timeout_at(
            child_cleanup_end,
            std::panic::AssertUnwindSafe(child_cleanup).catch_unwind()
        )
        .await,
        Ok(Ok(Ok(())))
    );
    // Parent cleanup is separately attempted even if child callback cleanup failed.
    let parent_cleanup = async {
        let requested = coordinator
            .cancel_run(ironclaw_turns::CancelRunRequest {
                scope: parent_scope.clone(),
                actor: actor.clone(),
                run_id: mirror_parent,
                reason: ironclaw_turns::SanitizedCancelReason::UserRequested,
                idempotency_key: ironclaw_turns::IdempotencyKey::new("h2-c-finally-parent")?,
            })
            .await?;
        if !matches!(
            requested.status,
            TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::Failed
        ) {
            published
                .cancel_run(ironclaw_turns::runner::CancelRunCompletionRequest {
                    run_id: mirror_parent,
                    runner_id: parent_runner,
                    lease_token: parent_lease,
                })
                .await?;
        }
        let state = coordinator
            .get_run_state(ironclaw_turns::GetRunStateRequest {
                scope: parent_scope.clone(),
                run_id: mirror_parent,
            })
            .await?;
        if !matches!(
            state.status,
            TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::Failed
        ) {
            return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                "parent mirror terminal unknown".into(),
            );
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    };
    let parent_closed = matches!(
        tokio::time::timeout_at(
            parent_cleanup_end,
            std::panic::AssertUnwindSafe(parent_cleanup).catch_unwind()
        )
        .await,
        Ok(Ok(Ok(())))
    );
    match outcome {
        Ok(Ok(Ok(proof))) if child_closed && parent_closed => Ok(proof),
        Ok(Ok(Err(original))) => Err(original),
        Ok(Err(original_panic)) => std::panic::resume_unwind(original_panic),
        Err(_) => Err("original owned business deadline unknown; cleanup cannot upgrade it".into()),
        _ => Err("official actor cleanup unknown; no closed proof".into()),
    }
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
    producer_child_run: String,
    api_origin: String,
    mcp_secret: String,
    target_ref: Value,
    policy_ref: Value,
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
        matches!(
            input.case_selector.as_str(),
            "h2-c-auditor-blocked-v1"
                | "h2-c-writer-denied-v1"
                | "h2-c-wrong-parent-v1"
                | "h2-c-wrong-target-v1"
        ) && input.mcp_secret.len() >= 32
            && input.mcp_secret.len() <= 256
            && serde_json::to_vec(&input.target_ref).is_ok_and(|value| value.len() <= 2048)
            && serde_json::to_vec(&input.policy_ref).is_ok_and(|value| value.len() <= 2048),
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
        Ok(Ok(proof)) => println!("H2_AUDITOR_PROOF={proof}"),
        _ => panic!("mechanical exporter failed; private error withheld"),
    }
}

// Mechanical transport only: no TianQuan schema, policy catalog or result signer.
// JSONRPC/session/HMAC handling belongs to the unchanged production MCP client.
#[derive(Clone)]
struct FixedMcpHttp {
    client: reqwest::Client,
    audience: String,
    end: tokio::time::Instant,
    acks: Arc<std::sync::Mutex<Vec<Value>>>,
}
#[async_trait::async_trait]
impl ironclaw_mcp::McpHostHttp for FixedMcpHttp {
    async fn request(
        &self,
        request: ironclaw_host_api::CapabilityHostHttpRequest,
    ) -> Result<ironclaw_host_api::RuntimeHttpEgressResponse, ironclaw_mcp::McpHostHttpError> {
        let exchange = async {
            if request.url != self.audience
                || !request.credential_injections.is_empty()
                || request.body.len() > 65536
            {
                return Err("fixed MCP request rejected");
            }
            let rpc: Value =
                serde_json::from_slice(&request.body).map_err(|_| "MCP request shape")?;
            let method = rpc["method"].as_str().ok_or("MCP method missing")?;
            if !matches!(
                method,
                "initialize" | "notifications/initialized" | "tools/call"
            ) {
                return Err("fixed MCP method rejected");
            }
            for name in [
                "x-tianquan-execution-context",
                "x-tianquan-execution-signature",
            ] {
                if !request
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case(name))
                {
                    return Err("production signer headers absent");
                }
            }
            let request_bytes = request.body.len() as u64;
            let mut builder = self.client.post(&self.audience).body(request.body);
            for (key, value) in request.headers {
                builder = builder.header(key, value);
            }
            let response = builder
                .send()
                .await
                .map_err(|_| "fixed MCP transport failed")?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(key, value)| {
                    value
                        .to_str()
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "MCP response header rejected")?;
            use futures_util::StreamExt;
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(next) = stream.next().await {
                let bytes = next.map_err(|_| "MCP original body EOF unknown")?;
                if body
                    .len()
                    .checked_add(bytes.len())
                    .is_none_or(|size| size > 2 * 1024 * 1024)
                {
                    return Err("MCP response budget rejected");
                }
                body.extend_from_slice(&bytes);
            }
            self.acks
                .lock()
                .map_err(|_| "MCP ack lock rejected")?
                .push(json!({
                "method":method,"status":status,"bodyBytes":body.len(),"responseEof":true,
                "signedByProductionClient":true}));
            Ok(ironclaw_host_api::RuntimeHttpEgressResponse {
                status,
                headers,
                response_bytes: body.len() as u64,
                body,
                saved_body: None,
                request_bytes,
                redaction_applied: false,
            })
        };
        tokio::time::timeout_at(self.end, exchange)
            .await
            .map_err(|_| ironclaw_mcp::McpHostHttpError::Egress {
                reason: "fixed_mcp_deadline_unknown".into(),
            })?
            .map_err(|_| ironclaw_mcp::McpHostHttpError::Egress {
                reason: "fixed_mcp_exchange_rejected_or_unknown".into(),
            })
    }
}
struct AuditExchange {
    accepted: bool,
    output: Option<Value>,
    protocol: Vec<Value>,
}
async fn signed_audit(
    input: &Input,
    child_scope: &TurnScope,
    child: TurnRunId,
) -> TestResult<AuditExchange> {
    use ironclaw_mcp::{
        McpClient, McpClientRequest, McpHostHttpClient, McpHostHttpEgressPlan,
        McpTrustedContextSigner, McpTrustedExecutionContext, StaticMcpHostHttpEgressPlanner,
    };
    let url = reqwest::Url::parse(&input.api_origin)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("fixed loopback origin required".into());
    }
    let audience = format!("{}/mcp/internal", input.api_origin.trim_end_matches('/'));
    let remaining = input.deadline_unix_ms - chrono::Utc::now().timestamp_millis() - 25000;
    if remaining <= 0 {
        return Err("MCP reserved cleanup horizon missing".into());
    }
    let end = tokio::time::Instant::now() + std::time::Duration::from_millis(remaining as u64);
    let acks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let http = FixedMcpHttp {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .build()?,
        audience: audience.clone(),
        end,
        acks: acks.clone(),
    };
    let client = McpHostHttpClient::new(
        http,
        StaticMcpHostHttpEgressPlanner::new(McpHostHttpEgressPlan::default()),
    )
    .with_trusted_context_signer(McpTrustedContextSigner::new(
        ironclaw_host_api::ExtensionId::new("tianquan-graph")?,
        input.mcp_secret.as_bytes().to_vec(),
        audience.clone(),
    )?);
    let mut target = input.target_ref.clone();
    if input.case_selector == "h2-c-wrong-target-v1" {
        let hash = target["contentHash"]
            .as_str()
            .ok_or("original target hash missing")?;
        if hash.len() != 64 {
            return Err("original target hash length".into());
        }
        target["contentHash"] = json!(format!(
            "{}{}",
            if hash.starts_with('0') { '1' } else { '0' },
            &hash[1..]
        ));
    }
    let mut arguments = json!({"project_id":input.project,"layer":"loop:prose",
        "request":{"atomicReview":{"schemaVersion":"2.0","targetRef":target,"policyRef":input.policy_ref}}});
    if input.case_selector == "h2-c-wrong-parent-v1" {
        // Outer coordinate narrows a real actor; it never constructs another PG parent.
        arguments["workflowRunId"] = json!(if input.parent_run
            == "00000000-0000-0000-0000-000000000000"
        {
            "00000000-0000-0000-0000-000000000001"
        } else {
            "00000000-0000-0000-0000-000000000000"
        });
    }
    let request = McpClientRequest {
        provider: ironclaw_host_api::ExtensionId::new("tianquan-graph")?,
        capability_id: ironclaw_host_api::CapabilityId::new("tianquan-graph.run_auditor")?,
        scope: child_scope.to_resource_scope(),
        transport: "http".into(),
        command: None,
        args: vec![],
        url: Some(audience),
        input: arguments,
        max_output_bytes: 2 * 1024 * 1024,
        trusted_context: Some(McpTrustedExecutionContext {
            authenticated_actor_user_id: Some(UserId::new(&input.owner)?),
            run_id: Some(ironclaw_host_api::RunId::parse(&child.to_string())?),
        }),
    };
    let observed = client.call_tool(request).await;
    let protocol = acks.lock().map_err(|_| "MCP ack read rejected")?.clone();
    drop(client);
    let expected = ["initialize", "notifications/initialized", "tools/call"];
    let complete = protocol.len() == 3
        && protocol.iter().zip(expected).all(|(ack, method)| {
            ack["method"] == method
                && ack["responseEof"] == true
                && ack["signedByProductionClient"] == true
        })
        && protocol[0]["status"] == 200
        && protocol[1]["status"] == 202;
    let positive = input.case_selector == "h2-c-auditor-blocked-v1";
    let output = observed.as_ref().ok().map(|value| value.output.clone());
    let accepted = complete
        && if positive {
            observed
                .as_ref()
                .is_ok_and(|result| result.output.get("isError") != Some(&Value::Bool(true)))
                && protocol[2]["status"] == 200
        } else if input.case_selector == "h2-c-wrong-target-v1" {
            observed
                .as_ref()
                .err()
                .is_some_and(|error| error.stable_reason() == "mcp_jsonrpc_error code=-32602")
                && protocol[2]["status"] == 200
        } else {
            matches!(&observed, Err(ironclaw_mcp::McpClientError::AuthRequired))
                && protocol[2]["status"] == 403
        };
    Ok(AuditExchange {
        accepted,
        output,
        protocol,
    })
}
