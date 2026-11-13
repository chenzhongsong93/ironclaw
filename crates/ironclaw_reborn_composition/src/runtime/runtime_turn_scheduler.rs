use ironclaw_runner::turn_scheduler::{SchedulerTurnRunWakeNotifier, TurnRunSchedulerHandle};
use ironclaw_turns::{TurnRunWake, TurnRunWakeNotifier};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

/// Owns the three scheduler lifecycle primitives and centralises the liveness
/// recipe. Every `RebornRuntime` method that previously duplicated the
/// "stopped-first, then contention=alive" check now delegates to
/// `is_stopped()`.
pub(super) struct RuntimeTurnScheduler {
    handle: Arc<Mutex<Option<TurnRunSchedulerHandle>>>,
    pub(super) stopped: Arc<AtomicBool>,
    notifier: Arc<SchedulerTurnRunWakeNotifier>,
}

impl RuntimeTurnScheduler {
    pub(super) fn new(
        handle: TurnRunSchedulerHandle,
        notifier: Arc<SchedulerTurnRunWakeNotifier>,
    ) -> Self {
        Self {
            handle: Arc::new(Mutex::new(Some(handle))),
            stopped: Arc::new(AtomicBool::new(false)),
            notifier,
        }
    }

    /// Canonical liveness check.
    ///
    /// Returns `true` when the scheduler has definitively stopped:
    /// - The `stopped` atomic flag is set (graceful shutdown or test-helper), OR
    /// - We can acquire the lock without contention AND the handle is gone/stopped.
    ///
    /// **Contention = alive**: a `try_lock` failure means another task momentarily
    /// holds the mutex (e.g. shutdown racing with submit) — that is NOT a stopped
    /// state. Only the atomic flag is authoritative for "truly stopped".
    pub(super) fn is_stopped(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        if let Ok(guard) = self.handle.try_lock()
            && guard.as_ref().is_none_or(|h| h.is_stopped())
        {
            return true;
        }
        false
    }

    /// Resident proof inspection is stricter than submit liveness: contention is unknown.
    /// Does not change is_stopped()'s established contention=alive application semantics.
    pub(super) fn resident_state(&self)->super::RebornResidentWorkerState {
        use super::RebornResidentWorkerState as S;
        if self.stopped.load(Ordering::Acquire){return S::Cancelled;}
        let Ok(guard)=self.handle.try_lock() else{return S::Unknown;};
        match guard.as_ref() {
            None=>S::Missing,
            Some(h) if h.cancellation_requested()=>S::Cancelled,
            Some(h) if h.is_stopped()=>S::Finished,
            Some(_)=>S::Running,
        }
    }

    pub(super) fn resident_observer(&self)->Arc<dyn Fn()->super::RebornResidentWorkerState+Send+Sync> {
        let stopped=self.stopped.clone();let handle=self.handle.clone();Arc::new(move|| {
            use super::RebornResidentWorkerState as S;
            if stopped.load(Ordering::Acquire){return S::Cancelled;}
            let Ok(guard)=handle.try_lock() else{return S::Unknown;};
            match guard.as_ref(){None=>S::Missing,Some(h) if h.cancellation_requested()=>S::Cancelled,Some(h) if h.is_stopped()=>S::Finished,Some(_)=>S::Running}
        })
    }

    /// Fire-and-forget wake nudge to the scheduler.
    pub(super) fn notify(&self, wake: TurnRunWake) {
        if let Err(error) = self.notifier.notify_queued_run(wake) {
            tracing::debug!(?error, "best-effort scheduler wake nudge failed");
        }
    }

    /// Graceful shutdown. Sets `stopped` BEFORE draining the handle so any
    /// concurrent `submit_user_turn` / wait loop observes stopped immediately
    /// (the shutdown-window fix).
    pub(super) async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(scheduler) = self.handle.lock().await.take() {
            scheduler.shutdown().await;
        }
    }

    /// Test-only: stop the scheduler for manual turn-state manipulation without
    /// consuming `self` (so `send_user_message` can still be called after).
    #[cfg(test)]
    pub(super) async fn stop_for_test(&self) {
        if let Some(scheduler) = self.handle.lock().await.take() {
            tokio::time::timeout(std::time::Duration::from_secs(2), scheduler.shutdown())
                .await
                .expect("turn-runner scheduler should stop before manual turn-state test");
        }
        self.stopped.store(true, Ordering::Release);
    }

    /// Expose the handle mutex for tests that need to simulate contention.
    #[cfg(test)]
    pub(super) fn handle_mutex(&self) -> &Mutex<Option<TurnRunSchedulerHandle>> {
        &self.handle
    }

    /// Read the raw stopped atomic flag (for tests verifying invariants).
    #[cfg(test)]
    pub(super) fn atomic_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}
