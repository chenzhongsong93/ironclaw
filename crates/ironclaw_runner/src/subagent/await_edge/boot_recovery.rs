//! Boot-driven roster walk + lazy per-scope admission backstop (§4.3, §5.3).
//! Split from `resolver.rs` (plan-review fix) — different trigger (process
//! boot / admission calls, not a lifecycle event) and unrelated primitives
//! (a bounded scheduler vs. a single store CAS call).
//!
//! Recovery uses one shared `Semaphore(4)` across boot, periodic, and lazy
//! first-touch work. The roster walk feeds at most four scope futures at once,
//! so process start does not create one waiting task per roster entry; lazy
//! calls retain the `in_progress` dedupe guard and reject admission immediately
//! while their scope is recovering.

use std::{
    collections::HashSet,
    future::Future,
    mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{StreamExt, stream};
use ironclaw_filesystem::RootFilesystem;
use ironclaw_loop_host::{
    AwaitEdgeWriter, AwaitedChildSetRecord, ResolveReport, ScopeRecoveryInProgress,
};
use ironclaw_threads::SessionThreadService;
use ironclaw_turns::{TurnScope, run_profile::AgentLoopHostError};
use tokio::{sync::Semaphore, task::JoinSet};

use super::{
    resolver::AwaitEdgeResolver,
    roster::{self, RosterKey},
    store::FilesystemAwaitEdgeStore,
};

/// Shared across boot and lazy recovery (round-4 fix: one limiter, not a
/// separate pool per origin).
pub const BOOT_RECOVERY_MAX_CONCURRENT_SCOPES: usize = 4;

/// Drives one scope's unclosed edges through the resolver's close machinery
/// (settle-if-still-open -> write -> resume -> release -> prune -> delete),
/// used by both the boot pass and a lazy first-touch recovery task.
async fn recover_scope<S, F>(
    resolver: &AwaitEdgeResolver<S, F>,
    store: &FilesystemAwaitEdgeStore<F>,
    scope: &TurnScope,
) -> ResolveReport
where
    S: SessionThreadService + ?Sized,
    F: RootFilesystem + ?Sized,
{
    let mut report = ResolveReport::default();
    let unclosed = match store.list_unclosed_for_scope(scope).await {
        Ok(edges) => edges,
        Err(error) => {
            tracing::debug!(error = %error, "await-edge scope recovery failed to list unclosed edges");
            report.record_failed();
            return report;
        }
    };
    for (parent_run_id, child_run_id, edge) in unclosed {
        match resolver
            .recover_edge(
                &edge.child_scope,
                parent_run_id,
                child_run_id,
                &edge,
                chrono::Utc::now(),
            )
            .await
        {
            Ok(outcome) => report.record(outcome),
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    %parent_run_id,
                    %child_run_id,
                    edge_state = ?edge.state,
                    "await-edge recovery failed; edge remains durable for a later pass"
                );
                report.record_failed();
            }
        }
    }
    if let Err(error) = store.prune_scope_roster(scope).await {
        tracing::debug!(error = %error, "await-edge recovery failed to prune scope roster marker");
        report.record_failed();
    }
    report
}

/// Boot-time roster walk (§4.3): enumerate every scope with unclosed edges
/// and drive each one's recovery, bounded by the caller-supplied `semaphore`
/// — the *same* `Arc<Semaphore>` a co-running [`ScopeRecoveryDriver`]'s lazy
/// backstop uses (via [`ScopeRecoveryDriver::semaphore`]), per the round-4
/// "one limiter, not a separate pool per origin" ruling. Callers must pass
/// `Arc::clone` of that shared semaphore, never a freshly constructed one.
pub async fn run_boot_recovery<S, F>(
    resolver: Arc<AwaitEdgeResolver<S, F>>,
    fs: Arc<ironclaw_filesystem::ScopedFilesystem<F>>,
    semaphore: Arc<Semaphore>,
) -> ResolveReport
where
    S: SessionThreadService + ?Sized + 'static,
    F: RootFilesystem + ?Sized + 'static,
{
    run_roster_recovery(resolver, fs, semaphore, None).await
}

#[derive(Default)]
struct ScopeRecoveryState {
    in_progress: HashSet<String>,
    booted: HashSet<String>,
}

#[derive(Clone)]
struct RecoveryTracking {
    state: Arc<Mutex<ScopeRecoveryState>>,
}

async fn run_roster_recovery<S, F>(
    resolver: Arc<AwaitEdgeResolver<S, F>>,
    fs: Arc<ironclaw_filesystem::ScopedFilesystem<F>>,
    semaphore: Arc<Semaphore>,
    tracking: Option<RecoveryTracking>,
) -> ResolveReport
where
    S: SessionThreadService + ?Sized + 'static,
    F: RootFilesystem + ?Sized + 'static,
{
    let keys = roster::walk_roster_shards(&fs).await;
    let scope_reports = stream::iter(keys.into_iter().map(|key| {
        let semaphore = Arc::clone(&semaphore);
        let resolver = Arc::clone(&resolver);
        let store = Arc::clone(resolver.store());
        let tracking = tracking.clone();
        async move {
            let scope_key = roster::encode_roster_filename(&key);
            let release_guard = tracking.as_ref().and_then(|tracking| {
                let mut state = ScopeRecoveryDriver::<S, F>::lock_state(&tracking.state);
                if state.in_progress.insert(scope_key.clone()) {
                    Some(InProgressReleaseGuard::new(
                        Arc::clone(&tracking.state),
                        scope_key.clone(),
                    ))
                } else {
                    None
                }
            });
            if tracking.is_some() && release_guard.is_none() {
                return ResolveReport::default();
            }
            let Ok(_permit) = semaphore.acquire_owned().await else {
                let mut report = ResolveReport::default();
                report.record_failed();
                return report;
            };
            let scope = roster_key_to_probe_scope(&key);
            let report = recover_scope(&resolver, &store, &scope).await;
            if report.failed == 0
                && let Some(tracking) = &tracking
            {
                ScopeRecoveryDriver::<S, F>::lock_state(&tracking.state)
                    .booted
                    .insert(scope_key);
            }
            report
        }
    }))
    .buffer_unordered(BOOT_RECOVERY_MAX_CONCURRENT_SCOPES)
    .collect::<Vec<_>>()
    .await;
    scope_reports
        .into_iter()
        .fold(ResolveReport::default(), merge_reports)
}

fn merge_reports(mut total: ResolveReport, next: ResolveReport) -> ResolveReport {
    total.resumed += next.resumed;
    total.drained += next.drained;
    total.abandoned += next.abandoned;
    total.already_closed += next.already_closed;
    total.failed += next.failed;
    total
}

/// A `TurnScope` carrying only the roster key's axes, for recovery-only use
/// (listing/closing edges never needs a real `ThreadId`). The literal
/// placeholder thread id is never persisted or resolved against — it exists
/// only because `TurnScope` requires the field.
///
/// Must preserve `key.user_id` as the scope's explicit owner (mirroring
/// `TurnScope::to_resource_scope`'s forward mapping in reverse): multi-user
/// edges live under the owner's mount, so a bare `TurnScope::new` here would
/// probe the system/`ActorFallback` mount and silently see zero unclosed
/// edges for every owner-scoped roster entry (external review finding on
/// this PR, #5720-class).
fn roster_key_to_probe_scope(key: &RosterKey) -> TurnScope {
    // `from_trusted` bypasses `validate_scope_id` — safe here because this
    // is a fixed literal, never caller-supplied, and never persisted or
    // resolved against a real thread (recovery only lists/closes edges by
    // scope axes). Avoids `.expect()` on a "known-valid" literal per repo
    // style (no unwrap/expect in production code).
    let owner = if key.user_id.as_str() == ironclaw_host_api::SYSTEM_RESERVED_ID {
        None
    } else {
        Some(key.user_id.clone())
    };
    TurnScope::new_with_owner(
        key.tenant_id.clone(),
        key.agent_id.clone(),
        key.project_id.clone(),
        ironclaw_host_api::ThreadId::from_trusted("await-edge-recovery-probe".to_string()),
        owner,
    )
}

/// Lazy per-scope admission backstop (§5.3): `AwaitEdgeWriter::check_scope_recovered`'s
/// real implementation. Wraps a `FilesystemAwaitEdgeStore` and implements
/// `AwaitEdgeWriter` by delegating writes to it while adding the admission
/// check on top.
pub struct ScopeRecoveryDriver<S: SessionThreadService + ?Sized, F: RootFilesystem + ?Sized> {
    resolver: Arc<AwaitEdgeResolver<S, F>>,
    store: Arc<FilesystemAwaitEdgeStore<F>>,
    semaphore: Arc<Semaphore>,
    // `in_progress` and `booted` share one lock so admission can decide the
    // recovery state atomically: an active periodic pass always wins over a
    // prior successful pass for the same scope.
    state: Arc<Mutex<ScopeRecoveryState>>,
    lazy_tasks: Mutex<JoinSet<()>>,
    shutting_down: Arc<AtomicBool>,
    shutdown_gate: Arc<Mutex<()>>,
}

impl<S, F> ScopeRecoveryDriver<S, F>
where
    S: SessionThreadService + ?Sized,
    F: RootFilesystem + ?Sized,
{
    pub fn new(
        resolver: Arc<AwaitEdgeResolver<S, F>>,
        store: Arc<FilesystemAwaitEdgeStore<F>>,
    ) -> Self {
        Self {
            resolver,
            store,
            semaphore: Arc::new(Semaphore::new(BOOT_RECOVERY_MAX_CONCURRENT_SCOPES)),
            state: Arc::new(Mutex::new(ScopeRecoveryState::default())),
            lazy_tasks: Mutex::new(JoinSet::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_gate: Arc::new(Mutex::new(())),
        }
    }

    fn scope_key(scope: &TurnScope) -> String {
        roster::encode_roster_filename(&RosterKey::from_resource_scope(&scope.to_resource_scope()))
    }

    fn lock_state(
        state: &Mutex<ScopeRecoveryState>,
    ) -> std::sync::MutexGuard<'_, ScopeRecoveryState> {
        state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// The shared limiter this driver's boot, periodic, and lazy recovery
    /// tasks acquire against (one limiter, not a separate pool per origin).
    pub fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.semaphore)
    }

    fn spawn_lazy_recovery(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        let _shutdown_gate = self
            .shutdown_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.shutting_down.load(Ordering::Acquire) {
            return false;
        }
        let mut tasks = self
            .lazy_tasks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
        true
    }

    async fn shutdown_lazy_recovery(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let mut tasks = {
            let _shutdown_gate = self
                .shutdown_gate
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut lazy_tasks = self
                .lazy_tasks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            mem::take(&mut *lazy_tasks)
        };
        tasks.shutdown().await;
    }
}

/// RAII release of one `in_progress` claim, so a panic anywhere in the
/// spawned lazy-recovery task (most notably inside `recover_scope`) still
/// unblocks the scope for a future admission attempt instead of wedging it
/// shut forever. Hand-rolled rather than pulling in `scopeguard` — the whole
/// type is this one field plus a three-line `Drop` impl.
struct InProgressReleaseGuard {
    state: Arc<Mutex<ScopeRecoveryState>>,
    key: String,
}

impl InProgressReleaseGuard {
    fn new(state: Arc<Mutex<ScopeRecoveryState>>, key: String) -> Self {
        Self { state, key }
    }
}

impl Drop for InProgressReleaseGuard {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.in_progress.remove(&self.key);
    }
}

#[async_trait::async_trait]
impl<S, F> AwaitEdgeWriter for ScopeRecoveryDriver<S, F>
where
    S: SessionThreadService + ?Sized + 'static,
    F: RootFilesystem + ?Sized + 'static,
{
    async fn recover_all_await_edges(&self) -> ResolveReport {
        run_roster_recovery(
            Arc::clone(&self.resolver),
            self.store.filesystem(),
            Arc::clone(&self.semaphore),
            Some(RecoveryTracking {
                state: Arc::clone(&self.state),
            }),
        )
        .await
    }

    async fn shutdown_await_edge_recovery(&self) {
        self.shutdown_lazy_recovery().await;
    }

    async fn check_scope_recovered(
        &self,
        scope: &TurnScope,
    ) -> Result<(), ScopeRecoveryInProgress> {
        let key = Self::scope_key(scope);
        let already_claimed = {
            let mut state = Self::lock_state(&self.state);
            if state.in_progress.contains(&key) {
                true
            } else if state.booted.contains(&key) {
                return Ok(());
            } else {
                state.in_progress.insert(key.clone());
                false
            }
        };
        if already_claimed {
            return Err(ScopeRecoveryInProgress {
                retry_after_hint: Duration::from_millis(200),
            });
        }
        // This call now uniquely owns the `in_progress` claim for `key` —
        // check whether there is actually anything to recover before ever
        // rejecting admission. A scope with no unclosed edges (the
        // overwhelmingly common case — a brand new scope's very first
        // spawn) has nothing a background recovery task would do; gating it
        // behind `ScopeRecoveryInProgress` regardless would reject every
        // first-ever spawn for every scope, which is not what §5.3 intends
        // (recovery exists for scopes that *might* have unclosed edges from
        // a prior crash, not as a tax on first contact).
        let has_unclosed_edges = match self.store.list_unclosed_for_scope(scope).await {
            Ok(edges) => !edges.is_empty(),
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    "await-edge scope-recovery check failed to list unclosed edges; \
                     treating as needing recovery rather than silently admitting"
                );
                true
            }
        };
        if !has_unclosed_edges {
            let mut state = Self::lock_state(&self.state);
            state.in_progress.remove(&key);
            state.booted.insert(key);
            return Ok(());
        }
        let resolver = Arc::clone(&self.resolver);
        let store = Arc::clone(&self.store);
        let semaphore = Arc::clone(&self.semaphore);
        let state = Arc::clone(&self.state);
        let shutting_down = Arc::clone(&self.shutting_down);
        let scope = scope.clone();
        let task_key = key.clone();
        if self.spawn_lazy_recovery(async move {
            // Panic-safety (external review finding on this PR): a panic
            // inside `recover_scope` must still release the `in_progress`
            // claim, or the scope is wedged shut (never admitted again)
            // until process restart. The guard is constructed before the
            // permit/recovery work so it covers the whole task, and only
            // releases `in_progress` — `booted` is intentionally left alone
            // here, since wedging admission *open* on panic (retry from
            // scratch next touch) is safer than wedging it permanently shut.
            let _release_guard = InProgressReleaseGuard::new(Arc::clone(&state), task_key.clone());
            let Ok(_permit) = semaphore.acquire().await else {
                return;
            };
            let report = recover_scope(&resolver, &store, &scope).await;
            if report.failed == 0 && !shutting_down.load(Ordering::Acquire) {
                ScopeRecoveryDriver::<S, F>::lock_state(&state)
                    .booted
                    .insert(task_key);
            } else if report.failed > 0 {
                tracing::debug!(
                    failures = report.failed,
                    "await-edge lazy recovery remains unbooted after failures"
                );
            }
        }) {
            return Err(ScopeRecoveryInProgress {
                retry_after_hint: Duration::from_millis(200),
            });
        }

        Err(ScopeRecoveryInProgress {
            retry_after_hint: Duration::from_millis(200),
        })
    }

    async fn record_awaited_child(
        &self,
        record: AwaitedChildSetRecord,
    ) -> Result<(), AgentLoopHostError> {
        self.store.record_awaited_child(record).await
    }

    async fn record_child_submitted(
        &self,
        child_scope: &TurnScope,
        parent_run_id: ironclaw_turns::TurnRunId,
        child_run_id: ironclaw_turns::TurnRunId,
        subagent_kind: &ironclaw_loop_host::SubagentKindId,
        submitted_at: ironclaw_turns::TurnTimestamp,
    ) {
        self.store
            .record_child_submitted(
                child_scope,
                parent_run_id,
                child_run_id,
                subagent_kind,
                submitted_at,
            )
            .await;
    }

    async fn abandon_awaited_child(
        &self,
        child_scope: &TurnScope,
        parent_run_id: ironclaw_turns::TurnRunId,
        child_run_id: ironclaw_turns::TurnRunId,
    ) -> Result<(), AgentLoopHostError> {
        self.store
            .abandon_awaited_child(child_scope, parent_run_id, child_run_id)
            .await
    }
}

#[cfg(test)]
#[path = "boot_recovery/tests.rs"]
mod tests;
