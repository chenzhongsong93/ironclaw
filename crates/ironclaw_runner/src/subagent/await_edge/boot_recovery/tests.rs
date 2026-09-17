use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ironclaw_filesystem::{
    CasExpectation, DirEntry, Entry, FileStat, FilesystemError, FilesystemOperation,
    InMemoryBackend, RecordVersion, ScopedFilesystem, VersionedEntry,
};
use ironclaw_host_api::{
    MountAlias, MountGrant, MountPermissions, MountView, TenantId, UserId, VirtualPath,
};
use ironclaw_threads::{InMemorySessionThreadService, ThreadScope};
use ironclaw_turns::TurnSpawnTreeStateStore;
use tokio::sync::Notify;

use super::*;

struct NoopResultWriter;

#[async_trait::async_trait]
impl ironclaw_loop_host::LoopCapabilityResultWriter for NoopResultWriter {
    async fn write_capability_result(
        &self,
        _write: ironclaw_loop_host::CapabilityResultWrite<'_>,
    ) -> Result<ironclaw_loop_host::CapabilityWriteResult, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            ironclaw_turns::run_profile::AgentLoopHostErrorKind::Unavailable,
            "not exercised by shared-semaphore tests",
        ))
    }
}

// External review finding on this PR (#5720-class): a roster key
// carrying a real, non-system owner must probe a scope with that same
// explicit owner, not silently fall back to `ActorFallback` (which
// resolves to the system mount, not the owner's) — otherwise boot
// recovery would list zero unclosed edges for every multi-user scope.
// Mutation: revert `roster_key_to_probe_scope` to `TurnScope::new(...)`
// -> RED (`explicit_owner_user_id()` becomes `None` for a real owner).
#[test]
fn roster_key_to_probe_scope_preserves_explicit_owner() {
    let key = RosterKey {
        tenant_id: TenantId::new("probe-tenant").unwrap(),
        user_id: UserId::new("probe-owner").unwrap(),
        agent_id: None,
        project_id: None,
    };
    let scope = roster_key_to_probe_scope(&key);
    assert_eq!(
        scope.explicit_owner_user_id(),
        Some(&UserId::new("probe-owner").unwrap()),
        "probe scope must carry the roster key's owner explicitly"
    );
    assert_eq!(
        scope.to_resource_scope().user_id,
        UserId::new("probe-owner").unwrap(),
        "reverse mapping must round-trip through to_resource_scope's forward mapping"
    );
}

// The system-sentinel roster key (agent-scoped / ownerless edges) must
// probe with `ActorFallback`, not an explicit "owner" of the system
// sentinel string — mirrors `to_resource_scope`'s forward direction where
// an absent explicit owner is *encoded* as the sentinel.
#[test]
fn roster_key_to_probe_scope_maps_system_sentinel_to_actor_fallback() {
    let key = RosterKey {
        tenant_id: TenantId::new("probe-tenant").unwrap(),
        user_id: UserId::from_trusted(ironclaw_host_api::SYSTEM_RESERVED_ID.to_string()),
        agent_id: None,
        project_id: None,
    };
    let scope = roster_key_to_probe_scope(&key);
    assert_eq!(scope.explicit_owner_user_id(), None);
}

struct FailingListBackend {
    inner: InMemoryBackend,
    fail_lists: AtomicBool,
}

#[async_trait::async_trait]
impl RootFilesystem for FailingListBackend {
    async fn list_dir(&self, path: &VirtualPath) -> Result<Vec<DirEntry>, FilesystemError> {
        if self.fail_lists.load(Ordering::SeqCst) {
            return Err(FilesystemError::Backend {
                path: path.clone(),
                operation: FilesystemOperation::ListDir,
                reason: "injected await-edge recovery list failure".to_string(),
            });
        }
        self.inner.list_dir(path).await
    }

    async fn stat(&self, path: &VirtualPath) -> Result<FileStat, FilesystemError> {
        self.inner.stat(path).await
    }

    async fn put(
        &self,
        path: &VirtualPath,
        entry: Entry,
        cas: CasExpectation,
    ) -> Result<RecordVersion, FilesystemError> {
        self.inner.put(path, entry, cas).await
    }

    async fn get(&self, path: &VirtualPath) -> Result<Option<VersionedEntry>, FilesystemError> {
        self.inner.get(path).await
    }

    async fn delete(&self, path: &VirtualPath) -> Result<(), FilesystemError> {
        self.inner.delete(path).await
    }

    async fn delete_if_version(
        &self,
        path: &VirtualPath,
        expected_version: RecordVersion,
    ) -> Result<(), FilesystemError> {
        self.inner.delete_if_version(path, expected_version).await
    }
}

/// Wraps an `InMemoryBackend`, counting every `list_dir` call and — on
/// the very first call only — holding it open behind a `Notify` gate
/// until the test explicitly releases it, so a second concurrent caller
/// can be raced against the first while it is provably still in flight.
/// Same delegating-decorator shape as
/// `ironclaw_authorization`'s `CountingFilesystem` test helper
/// (`capability_lease_contract.rs`), adapted to add the gate.
struct GatedCountingBackend {
    inner: InMemoryBackend,
    list_dir_calls: Arc<AtomicUsize>,
    gate_armed: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl GatedCountingBackend {
    fn new(inner: InMemoryBackend) -> Self {
        Self {
            inner,
            list_dir_calls: Arc::new(AtomicUsize::new(0)),
            gate_armed: AtomicBool::new(true),
            entered: Notify::new(),
            release: Notify::new(),
        }
    }

    fn list_dir_calls(&self) -> usize {
        self.list_dir_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl RootFilesystem for GatedCountingBackend {
    async fn list_dir(&self, path: &VirtualPath) -> Result<Vec<DirEntry>, FilesystemError> {
        self.list_dir_calls.fetch_add(1, Ordering::SeqCst);
        if self.gate_armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.list_dir(path).await
    }

    async fn stat(&self, path: &VirtualPath) -> Result<FileStat, FilesystemError> {
        self.inner.stat(path).await
    }

    async fn put(
        &self,
        path: &VirtualPath,
        entry: Entry,
        cas: CasExpectation,
    ) -> Result<RecordVersion, FilesystemError> {
        self.inner.put(path, entry, cas).await
    }

    async fn get(&self, path: &VirtualPath) -> Result<Option<VersionedEntry>, FilesystemError> {
        self.inner.get(path).await
    }

    async fn delete(&self, path: &VirtualPath) -> Result<(), FilesystemError> {
        self.inner.delete(path).await
    }

    async fn delete_if_version(
        &self,
        path: &VirtualPath,
        expected_version: RecordVersion,
    ) -> Result<(), FilesystemError> {
        self.inner.delete_if_version(path, expected_version).await
    }
}

#[tokio::test]
async fn failed_lazy_recovery_does_not_mark_scope_booted() {
    let backend = Arc::new(FailingListBackend {
        inner: InMemoryBackend::new(),
        fail_lists: AtomicBool::new(true),
    });
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/turns").unwrap(),
        VirtualPath::new("/turns").unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    let fs = Arc::new(ScopedFilesystem::with_fixed_view(
        Arc::clone(&backend),
        mounts,
    ));
    let store = Arc::new(FilesystemAwaitEdgeStore::new(fs));
    let goal_store: Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore> =
        Arc::new(crate::subagent::goal_store::InMemoryBoundedSubagentGoalStore::new());
    let turn_state_store: Arc<dyn TurnSpawnTreeStateStore> =
        Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let resolver = Arc::new(AwaitEdgeResolver::new_unbound(
        Arc::clone(&store),
        goal_store,
        turn_state_store,
        Arc::new(NoopResultWriter),
        Arc::new(InMemorySessionThreadService::default()),
    ));
    let driver = ScopeRecoveryDriver::new(resolver, store);
    let scope = TurnScope::new(
        TenantId::new("failed-lazy-recovery-tenant").unwrap(),
        None,
        None,
        ironclaw_host_api::ThreadId::from_trusted("failed-lazy-recovery-thread".to_string()),
    );
    let key =
        ScopeRecoveryDriver::<InMemorySessionThreadService, FailingListBackend>::scope_key(&scope);

    assert!(driver.check_scope_recovered(&scope).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while ScopeRecoveryDriver::<InMemorySessionThreadService, FailingListBackend>::lock_state(
            &driver.state,
        )
        .in_progress
        .contains(&key)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed recovery task should release its in-progress claim");
    assert!(
        !ScopeRecoveryDriver::<InMemorySessionThreadService, FailingListBackend>::lock_state(
            &driver.state,
        )
        .booted
        .contains(&key),
        "a failed lazy recovery must remain retryable, never be cached as booted"
    );

    backend.fail_lists.store(false, Ordering::SeqCst);
    assert!(
        driver.check_scope_recovered(&scope).await.is_ok(),
        "the next touch must perform a real retry and admit only after it succeeds"
    );
}

// External review finding on this PR: `check_scope_recovered`'s claim
// (the sync `in_progress` insert) must happen *before* the async
// `list_unclosed_for_scope` call, not after — otherwise two concurrent
// first-touches for the same never-seen scope would both pass the
// claim check while the first is still awaiting its list call, and both
// redundantly run it. Proven here by gating the backend's first
// `list_dir` call open and racing a second `check_scope_recovered` call
// against it while it is provably still in flight.
// Mutation: swap the two blocks in `check_scope_recovered` (list before
// claim) -> RED (`list_dir_calls()` observes 2, and/or B is wrongly
// admitted instead of rejected).
#[tokio::test]
async fn check_scope_recovered_claims_before_listing_so_two_concurrent_first_touches_list_exactly_once()
 {
    let backend = Arc::new(GatedCountingBackend::new(InMemoryBackend::new()));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/turns").unwrap(),
        VirtualPath::new("/turns").unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    let fs = Arc::new(ScopedFilesystem::with_fixed_view(
        Arc::clone(&backend),
        mounts,
    ));
    let store = Arc::new(FilesystemAwaitEdgeStore::new(Arc::clone(&fs)));
    let goal_store: Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore> =
        Arc::new(crate::subagent::goal_store::InMemoryBoundedSubagentGoalStore::new());
    let turn_state_store: Arc<dyn TurnSpawnTreeStateStore> =
        Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let result_writer: Arc<dyn ironclaw_loop_host::LoopCapabilityResultWriter> =
        Arc::new(NoopResultWriter);
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let resolver = Arc::new(AwaitEdgeResolver::new_unbound(
        Arc::clone(&store),
        goal_store,
        turn_state_store,
        result_writer,
        thread_service,
    ));
    let driver = Arc::new(ScopeRecoveryDriver::new(resolver, store));

    let scope = TurnScope::new(
        TenantId::new("concurrent-first-touch-tenant").unwrap(),
        None,
        None,
        ironclaw_host_api::ThreadId::from_trusted("concurrent-first-touch-thread".to_string()),
    );

    let driver_a = Arc::clone(&driver);
    let scope_a = scope.clone();
    let mut task_a = tokio::spawn(async move { driver_a.check_scope_recovered(&scope_a).await });

    // Wait until A's list call is actually in flight before racing B
    // against it -- not a fixed sleep.
    backend.entered.notified().await;

    let result_b = driver.check_scope_recovered(&scope).await;
    assert!(
        result_b.is_err(),
        "a concurrent first-touch for the same never-seen scope must be rejected \
             while A's claim is live, not independently re-list"
    );
    assert_eq!(
        backend.list_dir_calls(),
        1,
        "claim must be staked before the async list call so a second concurrent \
             caller never reaches it"
    );

    backend.release.notify_one();

    let result_a = tokio::time::timeout(Duration::from_secs(5), &mut task_a)
        .await
        .expect("task a should not hang")
        .expect("task a should not panic");
    assert!(
        result_a.is_ok(),
        "the sole caller that actually listed sees no unclosed edges on a \
             never-touched scope and must be admitted"
    );
    assert_eq!(backend.list_dir_calls(), 1);
}

fn boot_sem_scoped_fs() -> Arc<ScopedFilesystem<InMemoryBackend>> {
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/turns").unwrap(),
        VirtualPath::new("/turns").unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    Arc::new(ScopedFilesystem::with_fixed_view(
        Arc::new(InMemoryBackend::new()),
        mounts,
    ))
}

fn boot_sem_resolver(
    fs: Arc<ScopedFilesystem<InMemoryBackend>>,
) -> Arc<AwaitEdgeResolver<InMemorySessionThreadService, InMemoryBackend>> {
    let store = Arc::new(FilesystemAwaitEdgeStore::new(fs));
    let goal_store: Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore> =
        Arc::new(crate::subagent::goal_store::InMemoryBoundedSubagentGoalStore::new());
    let turn_state_store: Arc<dyn TurnSpawnTreeStateStore> =
        Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let result_writer: Arc<dyn ironclaw_loop_host::LoopCapabilityResultWriter> =
        Arc::new(NoopResultWriter);
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    Arc::new(AwaitEdgeResolver::new_unbound(
        store,
        goal_store,
        turn_state_store,
        result_writer,
        thread_service,
    ))
}

#[tokio::test]
async fn booted_scope_is_rejected_while_periodic_recovery_is_in_progress() {
    let fs = boot_sem_scoped_fs();
    let resolver = boot_sem_resolver(Arc::clone(&fs));
    let store = Arc::new(FilesystemAwaitEdgeStore::new(fs));
    let driver = ScopeRecoveryDriver::new(resolver, store);
    let scope = TurnScope::new(
        TenantId::new("booted-recovering-tenant").unwrap(),
        None,
        None,
        ironclaw_host_api::ThreadId::from_trusted("booted-recovering-thread".to_string()),
    );
    let key =
        ScopeRecoveryDriver::<InMemorySessionThreadService, InMemoryBackend>::scope_key(&scope);

    ScopeRecoveryDriver::<InMemorySessionThreadService, InMemoryBackend>::lock_state(&driver.state)
        .booted
        .insert(key.clone());
    ScopeRecoveryDriver::<InMemorySessionThreadService, InMemoryBackend>::lock_state(&driver.state)
        .in_progress
        .insert(key);

    assert!(
        driver.check_scope_recovered(&scope).await.is_err(),
        "an in-progress periodic recovery must reject admission even when its previous pass booted the scope"
    );
}

// Required test (§4.3 round-4 fix, boot_recovery.rs module header):
// `run_boot_recovery` and `ScopeRecoveryDriver`'s lazy backstop must
// contend for the *same* semaphore, not one each. Proven by having a
// lazy-shaped caller hold every permit on `driver.semaphore()`, then
// driving `run_boot_recovery` against `Arc::clone` of that exact
// semaphore and asserting it is blocked until the held permits are
// released — a boot pass with its own separate semaphore would complete
// immediately regardless of what the lazy origin was holding.
// Mutation: give `run_boot_recovery` its own freshly constructed
// `Semaphore::new(BOOT_RECOVERY_MAX_CONCURRENT_SCOPES)` internally
// instead of taking the caller's -> RED (boot no longer blocks).
#[tokio::test]
async fn boot_and_lazy_recovery_share_one_semaphore_not_separate_pools() {
    let fs = boot_sem_scoped_fs();
    let resolver = boot_sem_resolver(Arc::clone(&fs));
    let store = Arc::new(FilesystemAwaitEdgeStore::new(Arc::clone(&fs)));
    let driver = ScopeRecoveryDriver::new(Arc::clone(&resolver), store);

    // Seed exactly one roster entry so boot's walk has one scope to
    // attempt a permit acquisition for.
    let roster_key = RosterKey {
        tenant_id: TenantId::new("boot-sem-tenant").unwrap(),
        user_id: UserId::new("boot-sem-user").unwrap(),
        agent_id: None,
        project_id: None,
    };
    roster::touch_roster_marker(&fs, &roster_key).await.unwrap();

    // Simulate `BOOT_RECOVERY_MAX_CONCURRENT_SCOPES` lazy-origin
    // recovery tasks already holding every permit on the shared
    // limiter.
    let shared_semaphore = driver.semaphore();
    let held_permits = shared_semaphore
        .try_acquire_many(BOOT_RECOVERY_MAX_CONCURRENT_SCOPES as u32)
        .expect("semaphore should start with every permit free");

    let mut boot_handle = tokio::spawn(run_boot_recovery(
        Arc::clone(&resolver),
        Arc::clone(&fs),
        Arc::clone(&shared_semaphore),
    ));

    let raced = tokio::time::timeout(Duration::from_millis(150), &mut boot_handle).await;
    assert!(
        raced.is_err(),
        "boot recovery completed while the shared semaphore was fully held \
             by another origin — it must be blocked on the SAME limiter, proving \
             it is not acquiring against a separate pool"
    );

    drop(held_permits);

    tokio::time::timeout(Duration::from_secs(5), boot_handle)
        .await
        .expect("boot recovery should complete once the shared semaphore frees up")
        .expect("boot recovery task should not panic");
}

#[derive(Clone, Copy)]
enum OpenRecoveryChild {
    Terminal,
    MissingPastDeadline,
}

async fn recover_open_edge(
    child_case: OpenRecoveryChild,
) -> (ResolveReport, ironclaw_turns::TurnStatus, bool, bool) {
    use crate::subagent::goal_store::{InMemoryBoundedSubagentGoalStore, SubagentGoal};
    use ironclaw_turns::{
        DefaultTurnCoordinator, GateRef, SubmitChildRunRequest, SubmitTurnRequest, TurnActor,
        TurnCoordinator, TurnSpawnTreePort, runner::TurnRunTransitionPort,
    };

    struct RecoveryResultWriter;

    #[async_trait::async_trait]
    impl ironclaw_loop_host::LoopCapabilityResultWriter for RecoveryResultWriter {
        async fn write_capability_result(
            &self,
            _write: ironclaw_loop_host::CapabilityResultWrite<'_>,
        ) -> Result<ironclaw_loop_host::CapabilityWriteResult, AgentLoopHostError> {
            Err(AgentLoopHostError::new(
                ironclaw_turns::run_profile::AgentLoopHostErrorKind::Unavailable,
                "not exercised by open-edge recovery tests",
            ))
        }

        async fn update_capability_result(
            &self,
            _run_context: &ironclaw_turns::run_profile::LoopRunContext,
            _result_ref: &ironclaw_turns::LoopResultRef,
            output: serde_json::Value,
        ) -> Result<u64, AgentLoopHostError> {
            serde_json::to_vec(&output)
                .map(|bytes| bytes.len() as u64)
                .map_err(|error| {
                    AgentLoopHostError::new(
                        ironclaw_turns::run_profile::AgentLoopHostErrorKind::Unavailable,
                        error.to_string(),
                    )
                })
        }
    }

    let fs = boot_sem_scoped_fs();
    let store = Arc::new(FilesystemAwaitEdgeStore::new(Arc::clone(&fs)));
    let state_store = Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let coordinator = Arc::new(DefaultTurnCoordinator::new(Arc::clone(&state_store)));
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let goal_store = Arc::new(InMemoryBoundedSubagentGoalStore::new());

    let tenant_id = ironclaw_host_api::TenantId::new("recover-open-tenant").unwrap();
    let agent_id = ironclaw_host_api::AgentId::new("recover-open-agent").unwrap();
    let owner = UserId::new("recover-open-owner").unwrap();
    let actor = TurnActor::new(owner.clone());
    let parent_thread_id = ironclaw_host_api::ThreadId::new("recover-open-parent-thread").unwrap();
    let parent_scope = TurnScope::new_with_owner(
        tenant_id.clone(),
        Some(agent_id.clone()),
        None,
        parent_thread_id.clone(),
        Some(owner.clone()),
    );
    let ironclaw_turns::SubmitTurnResponse::Accepted {
        run_id: parent_run_id,
        ..
    } = coordinator
        .submit_turn(SubmitTurnRequest {
            requested_model: None,
            scope: parent_scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new(
                "msg:recover-open-parent",
            )
            .unwrap(),
            source_binding_ref: ironclaw_turns::SourceBindingRef::new("source:recover-open-parent")
                .unwrap(),
            reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                "reply:recover-open-parent",
            )
            .unwrap(),
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:recover-open-parent")
                .unwrap(),
            received_at: chrono::Utc::now(),
            requested_run_id: None,
            parent_run_id: None,
            subagent_depth: 0,
            spawn_tree_root_run_id: None,
            product_context: None,
            llm_subject: None,
        })
        .await
        .unwrap();
    let parent_runner_id = ironclaw_turns::TurnRunnerId::new();
    let parent_lease = ironclaw_turns::TurnLeaseToken::new();
    state_store
        .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
            runner_id: parent_runner_id,
            lease_token: parent_lease,
            scope_filter: None,
        })
        .await
        .unwrap()
        .expect("parent run claimable");
    let gate_ref = GateRef::new("gate:recover-open").unwrap();
    state_store
        .block_run(ironclaw_turns::runner::BlockRunRequest {
            run_id: parent_run_id,
            runner_id: parent_runner_id,
            lease_token: parent_lease,
            checkpoint_id: ironclaw_turns::TurnCheckpointId::new(),
            state_ref: ironclaw_turns::run_profile::LoopCheckpointStateRef::new(
                "checkpoint:recover-open",
            )
            .unwrap(),
            reason: ironclaw_turns::BlockedReason::AwaitDependentRun {
                gate_ref: gate_ref.clone(),
            },
        })
        .await
        .unwrap();

    let child_thread_id = ironclaw_host_api::ThreadId::new("recover-open-child-thread").unwrap();
    let child_scope = TurnScope::new_with_owner(
        tenant_id.clone(),
        Some(agent_id.clone()),
        None,
        child_thread_id.clone(),
        Some(owner.clone()),
    );
    let child_run_id = match child_case {
        OpenRecoveryChild::Terminal => {
            let ironclaw_turns::SubmitTurnResponse::Accepted { run_id, .. } = coordinator
                .submit_child_run(SubmitChildRunRequest {
                    parent_scope: parent_scope.clone(),
                    parent_run_id,
                    child_scope: child_scope.clone(),
                    actor: actor.clone(),
                    accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new(
                        "msg:recover-open-child",
                    )
                    .unwrap(),
                    source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                        "source:recover-open-child",
                    )
                    .unwrap(),
                    reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                        "reply:recover-open-child",
                    )
                    .unwrap(),
                    requested_run_profile: None,
                    idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:recover-open-child")
                        .unwrap(),
                    received_at: chrono::Utc::now(),
                    requested_run_id: None,
                    spawn_tree_descendant_cap: 16,
                })
                .await
                .unwrap();
            let child_runner_id = ironclaw_turns::TurnRunnerId::new();
            let child_lease = ironclaw_turns::TurnLeaseToken::new();
            state_store
                .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
                    runner_id: child_runner_id,
                    lease_token: child_lease,
                    scope_filter: None,
                })
                .await
                .unwrap()
                .expect("child run claimable");
            state_store
                .complete_run(ironclaw_turns::runner::CompleteRunRequest {
                    run_id,
                    runner_id: child_runner_id,
                    lease_token: child_lease,
                })
                .await
                .unwrap();
            run_id
        }
        OpenRecoveryChild::MissingPastDeadline => {
            let child_run_id = ironclaw_turns::TurnRunId::new();
            state_store
                .reserve_tree_descendants(&child_scope, parent_run_id, 1, 16)
                .await
                .unwrap();
            child_run_id
        }
    };

    let thread_scope = ThreadScope {
        tenant_id,
        agent_id,
        project_id: None,
        owner_user_id: Some(owner),
        mission_id: None,
    };
    for thread_id in [&parent_thread_id, &child_thread_id] {
        thread_service
            .ensure_thread(ironclaw_threads::EnsureThreadRequest {
                scope: thread_scope.clone(),
                thread_id: Some(thread_id.clone()),
                created_by_actor_id: "test".to_string(),
                title: None,
                metadata_json: None,
            })
            .await
            .unwrap();
    }
    let result_ref = ironclaw_turns::LoopResultRef::new("result:recover-open").unwrap();
    thread_service
        .append_tool_result_reference(ironclaw_threads::AppendToolResultReferenceRequest {
            scope: thread_scope,
            thread_id: parent_thread_id.clone(),
            turn_run_id: parent_run_id.to_string(),
            result_ref: result_ref.as_str().to_string(),
            safe_summary: ironclaw_threads::ToolResultSafeSummary::new("subagent spawned").unwrap(),
            provider_call: None,
            model_observation: None,
        })
        .await
        .unwrap();
    goal_store
        .put(
            &child_scope,
            child_run_id,
            SubagentGoal {
                task: "recover open edge".to_string(),
                handoff: None,
            },
        )
        .unwrap();

    let mut parent_run_context =
        ironclaw_agent_loop::test_support::test_run_context("recover-open-parent");
    parent_run_context.scope = parent_scope.clone();
    parent_run_context.thread_id = parent_thread_id;
    parent_run_context.run_id = parent_run_id;
    parent_run_context.actor = Some(actor);
    let created_at = chrono::Utc::now();
    store
        .open(
            &child_scope,
            parent_run_id,
            child_run_id,
            super::super::AwaitEdge {
                child_scope: child_scope.clone(),
                child_thread_id,
                parent_thread_id: parent_run_context.thread_id.clone(),
                parent_run_context,
                tree_root_run_id: parent_run_id,
                gate_ref,
                source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                    "subagent-source:recover-open",
                )
                .unwrap(),
                reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                    "subagent-reply:recover-open",
                )
                .unwrap(),
                subagent_kind: ironclaw_loop_host::SubagentKindId::new("general").unwrap(),
                spawn_capability_id: ironclaw_host_api::CapabilityId::new(
                    ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID,
                )
                .unwrap(),
                result_ref,
                mode: ironclaw_loop_host::SpawnSubagentMode::Blocking,
                state: super::super::AwaitEdgeState::Open,
                terminal_kind: None,
                terminal_byte_len: None,
                terminal_reason: None,
                reservation_release: super::super::ReservationReleaseState::Unclaimed,
                created_at,
                deadline: match child_case {
                    OpenRecoveryChild::Terminal => Some(
                        created_at
                            + chrono::Duration::seconds(
                                super::super::DEFAULT_AWAIT_EDGE_DEADLINE_SECONDS,
                            ),
                    ),
                    OpenRecoveryChild::MissingPastDeadline => {
                        Some(created_at - chrono::Duration::seconds(1))
                    }
                },
                settled_at: None,
            },
        )
        .await
        .unwrap();

    let goal_store_dyn: Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore> = goal_store.clone();
    let resolver = Arc::new(AwaitEdgeResolver::new_unbound(
        Arc::clone(&store),
        goal_store_dyn,
        state_store.clone(),
        Arc::new(RecoveryResultWriter),
        thread_service,
    ));
    let coordinator_dyn: Arc<dyn ironclaw_turns::TurnCoordinator> = coordinator.clone();
    resolver.bind_coordinator(coordinator_dyn).unwrap();

    let report = recover_scope(&resolver, &store, &child_scope).await;
    let parent_status = coordinator
        .get_run_state(ironclaw_turns::GetRunStateRequest {
            scope: parent_scope,
            run_id: parent_run_id,
        })
        .await
        .unwrap()
        .status;
    let edge_exists = store
        .peek(&child_scope, parent_run_id, child_run_id)
        .await
        .unwrap()
        .is_some();
    let goal_exists = goal_store.get(&child_scope, child_run_id).is_ok();
    (report, parent_status, edge_exists, goal_exists)
}

#[tokio::test]
async fn recover_scope_settles_open_edge_from_terminal_child_record() {
    let (report, parent_status, edge_exists, goal_exists) =
        recover_open_edge(OpenRecoveryChild::Terminal).await;

    assert_eq!(report.resumed, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(parent_status, ironclaw_turns::TurnStatus::Queued);
    assert!(!edge_exists);
    assert!(!goal_exists);
}

#[tokio::test]
async fn recover_scope_times_out_open_edge_when_child_submit_never_committed() {
    let (report, parent_status, edge_exists, goal_exists) =
        recover_open_edge(OpenRecoveryChild::MissingPastDeadline).await;

    assert_eq!(report.resumed, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(parent_status, ironclaw_turns::TurnStatus::Queued);
    assert!(!edge_exists);
    assert!(!goal_exists);
}

// A crash-settled edge must re-drive write+resume, not just `close_edge`,
// or the parent stays blocked forever (external review, PR #5819).
// Mutation: revert the `Settled` branch to a bare `close_edge` call ->
// RED (parent never leaves `BlockedDependentRun`, `report.resumed == 0`).
#[tokio::test]
async fn recover_scope_redrives_write_and_resume_for_a_crash_settled_undrained_edge() {
    use ironclaw_turns::{
        DefaultTurnCoordinator, GateRef, SubmitChildRunRequest, SubmitTurnRequest, TurnActor,
        TurnCoordinator, TurnSpawnTreePort, runner::TurnRunTransitionPort,
    };

    let fs = boot_sem_scoped_fs();
    let store = Arc::new(FilesystemAwaitEdgeStore::new(Arc::clone(&fs)));
    let state_store = Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let coordinator = DefaultTurnCoordinator::new(Arc::clone(&state_store));
    let thread_service = Arc::new(InMemorySessionThreadService::default());

    let tenant_id = ironclaw_host_api::TenantId::new("recover-settled-tenant").unwrap();
    let agent_id = ironclaw_host_api::AgentId::new("recover-settled-agent").unwrap();
    let owner = UserId::new("recover-settled-owner").unwrap();
    let actor = TurnActor::new(owner.clone());
    let parent_thread_id =
        ironclaw_host_api::ThreadId::new("recover-settled-parent-thread").unwrap();
    let parent_scope = TurnScope::new_with_owner(
        tenant_id.clone(),
        Some(agent_id.clone()),
        None,
        parent_thread_id.clone(),
        Some(owner.clone()),
    );

    // 1. Submit + block the parent on a dependent-run gate.
    let ironclaw_turns::SubmitTurnResponse::Accepted {
        run_id: parent_run_id,
        ..
    } = coordinator
        .submit_turn(SubmitTurnRequest {
            requested_model: None,
            scope: parent_scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new(
                "msg:recover-settled-parent",
            )
            .unwrap(),
            source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                "source:recover-settled-parent",
            )
            .unwrap(),
            reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                "reply:recover-settled-parent",
            )
            .unwrap(),
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:recover-settled-parent")
                .unwrap(),
            received_at: chrono::Utc::now(),
            requested_run_id: None,
            parent_run_id: None,
            subagent_depth: 0,
            spawn_tree_root_run_id: None,
            product_context: None,
            llm_subject: None,
        })
        .await
        .unwrap();
    let runner_id = ironclaw_turns::TurnRunnerId::new();
    let lease_token = ironclaw_turns::TurnLeaseToken::new();
    state_store
        .claim_next_run(ironclaw_turns::runner::ClaimRunRequest {
            runner_id,
            lease_token,
            scope_filter: None,
        })
        .await
        .unwrap()
        .expect("parent run claimable");
    let gate_ref = GateRef::new("gate:recover-settled-test").unwrap();
    state_store
        .block_run(ironclaw_turns::runner::BlockRunRequest {
            run_id: parent_run_id,
            runner_id,
            lease_token,
            checkpoint_id: ironclaw_turns::TurnCheckpointId::new(),
            state_ref: ironclaw_turns::run_profile::LoopCheckpointStateRef::new(
                "checkpoint:recover-settled-test",
            )
            .unwrap(),
            reason: ironclaw_turns::BlockedReason::AwaitDependentRun {
                gate_ref: gate_ref.clone(),
            },
        })
        .await
        .unwrap();

    // 2. Submit the child as a real lineage child of the parent (its own
    // run status never needs to advance -- the edge below carries the
    // already-`Settled` terminal state directly, simulating "crashed
    // after settle, before drain").
    let child_thread_id = ironclaw_host_api::ThreadId::new("recover-settled-child-thread").unwrap();
    let child_scope = TurnScope::new_with_owner(
        tenant_id.clone(),
        Some(agent_id.clone()),
        None,
        child_thread_id.clone(),
        Some(owner.clone()),
    );
    let ironclaw_turns::SubmitTurnResponse::Accepted {
        run_id: child_run_id,
        ..
    } = coordinator
        .submit_child_run(SubmitChildRunRequest {
            parent_scope: parent_scope.clone(),
            parent_run_id,
            child_scope: child_scope.clone(),
            actor: actor.clone(),
            accepted_message_ref: ironclaw_turns::AcceptedMessageRef::new(
                "msg:recover-settled-child",
            )
            .unwrap(),
            source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                "source:recover-settled-child",
            )
            .unwrap(),
            reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
                "reply:recover-settled-child",
            )
            .unwrap(),
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:recover-settled-child")
                .unwrap(),
            received_at: chrono::Utc::now(),
            requested_run_id: None,
            spawn_tree_descendant_cap: 16,
        })
        .await
        .unwrap();

    // 3. Seed both threads and the parent's spawn-time tool-result
    // placeholder.
    thread_service
        .ensure_thread(ironclaw_threads::EnsureThreadRequest {
            scope: ThreadScope {
                tenant_id: tenant_id.clone(),
                agent_id: agent_id.clone(),
                project_id: None,
                owner_user_id: Some(owner.clone()),
                mission_id: None,
            },
            thread_id: Some(child_thread_id.clone()),
            created_by_actor_id: "test".to_string(),
            title: Some("Subagent".to_string()),
            metadata_json: None,
        })
        .await
        .unwrap();
    thread_service
        .ensure_thread(ironclaw_threads::EnsureThreadRequest {
            scope: ThreadScope {
                tenant_id: tenant_id.clone(),
                agent_id: agent_id.clone(),
                project_id: None,
                owner_user_id: Some(owner.clone()),
                mission_id: None,
            },
            thread_id: Some(parent_thread_id.clone()),
            created_by_actor_id: "test".to_string(),
            title: None,
            metadata_json: None,
        })
        .await
        .unwrap();
    let result_ref = ironclaw_turns::LoopResultRef::new("result:subagent.recover-settled").unwrap();
    thread_service
        .append_tool_result_reference(ironclaw_threads::AppendToolResultReferenceRequest {
            scope: ThreadScope {
                tenant_id: tenant_id.clone(),
                agent_id: agent_id.clone(),
                project_id: None,
                owner_user_id: Some(owner.clone()),
                mission_id: None,
            },
            thread_id: parent_thread_id.clone(),
            turn_run_id: parent_run_id.to_string(),
            result_ref: result_ref.as_str().to_string(),
            safe_summary: ironclaw_threads::ToolResultSafeSummary::new("subagent spawned").unwrap(),
            provider_call: None,
            model_observation: None,
        })
        .await
        .unwrap();

    // 4. Open the edge already in `Settled` state -- simulating a crash
    // that landed after the settle CAS write but before drain ran.
    let mut parent_run_context =
        ironclaw_agent_loop::test_support::test_run_context("recover-settled-parent-ctx");
    parent_run_context.scope = parent_scope.clone();
    parent_run_context.thread_id = parent_thread_id.clone();
    parent_run_context.run_id = parent_run_id;
    parent_run_context.actor = Some(actor.clone());
    let edge = super::super::AwaitEdge {
        child_scope: child_scope.clone(),
        child_thread_id: child_thread_id.clone(),
        parent_thread_id: parent_thread_id.clone(),
        parent_run_context,
        tree_root_run_id: parent_run_id,
        gate_ref: gate_ref.clone(),
        source_binding_ref: ironclaw_turns::SourceBindingRef::new(
            "subagent-source:recover-settled",
        )
        .unwrap(),
        reply_target_binding_ref: ironclaw_turns::ReplyTargetBindingRef::new(
            "subagent-reply:recover-settled",
        )
        .unwrap(),
        subagent_kind: ironclaw_loop_host::SubagentKindId::new("general").unwrap(),
        spawn_capability_id: ironclaw_host_api::CapabilityId::new(
            ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID,
        )
        .unwrap(),
        result_ref,
        mode: ironclaw_loop_host::SpawnSubagentMode::Blocking,
        state: super::super::AwaitEdgeState::Settled,
        terminal_kind: Some(super::super::EdgeTerminalKind::Completed),
        terminal_byte_len: None,
        terminal_reason: None,
        reservation_release: super::super::ReservationReleaseState::Unclaimed,
        created_at: chrono::Utc::now(),
        deadline: Some(
            chrono::Utc::now()
                + chrono::Duration::seconds(super::super::DEFAULT_AWAIT_EDGE_DEADLINE_SECONDS),
        ),
        settled_at: Some(chrono::Utc::now()),
    };
    store
        .open(&child_scope, parent_run_id, child_run_id, edge)
        .await
        .unwrap();

    // 5. Build the resolver, bind the real coordinator, and re-drive
    // recovery over this exact scope.
    let goal_store: Arc<dyn ironclaw_loop_host::SubagentSpawnGoalStore> =
        Arc::new(crate::subagent::goal_store::InMemoryBoundedSubagentGoalStore::new());
    let turn_state_store: Arc<dyn TurnSpawnTreeStateStore> = state_store.clone();
    let resolver = Arc::new(AwaitEdgeResolver::new_unbound(
        Arc::clone(&store),
        goal_store,
        turn_state_store,
        Arc::new(NoopResultWriter),
        Arc::clone(&thread_service),
    ));
    let coordinator_dyn: Arc<dyn ironclaw_turns::TurnCoordinator> = Arc::new(coordinator);
    resolver
        .bind_coordinator(Arc::clone(&coordinator_dyn))
        .unwrap();

    let report = recover_scope(&resolver, &store, &child_scope).await;
    assert_eq!(
        report.resumed, 1,
        "recovery must actually drive the write+resume path, not just close the edge"
    );
    assert_eq!(report.failed, 0);

    // 6. The parent actually left `BlockedDependentRun` -- not stuck.
    let parent_state = coordinator_dyn
        .get_run_state(ironclaw_turns::GetRunStateRequest {
            scope: parent_scope,
            run_id: parent_run_id,
        })
        .await
        .unwrap();
    assert_ne!(
        parent_state.status,
        ironclaw_turns::TurnStatus::BlockedDependentRun,
        "the parent must actually resume, not stay stuck on its dependent-run gate"
    );

    // 7. The edge is actually gone -- the close half of the sequence
    // still ran too.
    assert!(
        store
            .list_unclosed_for_scope(&child_scope)
            .await
            .unwrap()
            .is_empty()
    );
}

// External review finding on this PR: a panic inside the lazy-recovery
// task (most likely `recover_scope`) must not leave the scope's
// `in_progress` claim set forever -- that would wedge the scope shut,
// rejecting every future admission attempt until process restart.
// Exercises `InProgressReleaseGuard` directly (not the full spawned-task
// path, which needs cooperative-scheduling yields to observe a
// backgrounded panic and would be flaky under `#[tokio::test]`'s
// current-thread runtime) -- this still pins the exact defect: the guard
// must release the claim across an unwind, not just on a normal return.
// Mutation: delete the `Drop` impl's body (or skip constructing the
// guard) -> RED (`in_progress` still contains the key after the panic).
#[test]
fn in_progress_release_guard_releases_the_claim_across_a_panic_unwind() {
    let state = Arc::new(Mutex::new(ScopeRecoveryState::default()));
    state
        .lock()
        .unwrap()
        .in_progress
        .insert("panic-guard-scope-key".to_string());

    let guard_state = Arc::clone(&state);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = InProgressReleaseGuard::new(
            Arc::clone(&guard_state),
            "panic-guard-scope-key".to_string(),
        );
        panic!("simulated recover_scope crash");
    }));
    assert!(unwound.is_err(), "the closure should have panicked");
    assert!(
        !state
            .lock()
            .unwrap()
            .in_progress
            .contains("panic-guard-scope-key"),
        "the in_progress claim must be released even when the recovery task \
             panics, or the scope is wedged shut until process restart"
    );
}
