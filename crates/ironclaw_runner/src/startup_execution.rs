//! Internal assembly latch. It only defers startup execution; it grants no capability.
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
pub struct StartupExecutionLatch {
    released: CancellationToken,
    stopped: CancellationToken,
    original_deadline: Mutex<Option<tokio::time::Instant>>,
}
impl StartupExecutionLatch {
    pub fn open() -> Arc<Self> {
        let latch = Self::closed();
        latch.release();
        latch
    }
    pub fn closed() -> Arc<Self> {
        Arc::new(Self {
            released: CancellationToken::new(),
            stopped: CancellationToken::new(),
            original_deadline: Mutex::new(None),
        })
    }
    pub fn pending(&self) -> bool {
        !self.released.is_cancelled()
    }
    /// Internal composition-only release; never exported by the product Runtime facade.
    pub fn release(&self) {
        if !self.stopped.is_cancelled() {
            self.released.cancel();
        }
    }
    /// Installs the ORIGINAL host deadline before opening. A later call cannot extend it.
    pub fn release_before(&self, original_deadline: tokio::time::Instant) -> bool {
        let Ok(mut deadline) = self.original_deadline.lock() else {
            self.stop();
            return false;
        };
        *deadline = Some(deadline.map_or(original_deadline, |old| old.min(original_deadline)));
        if self.stopped.is_cancelled() || tokio::time::Instant::now() >= deadline.unwrap() {
            drop(deadline);
            self.stop();
            return false;
        }
        self.released.cancel();
        drop(deadline);
        self.allows_execution()
    }
    /// Open a resident worker after initial admission within the original startup budget.
    /// A latch already used for a timed worker retains its original deadline.
    pub fn release_service_before(&self, startup_end: tokio::time::Instant) -> bool {
        let Ok(deadline) = self.original_deadline.lock() else {
            self.stop();
            return false;
        };
        if deadline.is_some()
            || !self.pending()
            || self.stopped.is_cancelled()
            || tokio::time::Instant::now() >= startup_end
        {
            return false;
        }
        self.released.cancel();
        drop(deadline);
        self.allows_execution()
    }
    /// Dispatch-time clock recheck; poisoning/expired deadline only reduces authority.
    pub fn allows_execution(&self) -> bool {
        if self.pending() || self.stopped.is_cancelled() {
            return false;
        }
        let permitted =
            self.original_deadline.lock().ok().is_some_and(|deadline| {
                deadline.is_none_or(|end| tokio::time::Instant::now() < end)
            });
        if !permitted {
            self.stop();
        }
        permitted && !self.stopped.is_cancelled()
    }
    pub fn stop(&self) {
        self.stopped.cancel();
    }
    pub fn stopped(&self) -> bool {
        self.stopped.is_cancelled()
    }
    pub fn stop_token(&self) -> CancellationToken {
        self.stopped.clone()
    }
    pub async fn wait(&self, shutdown: &CancellationToken) -> bool {
        tokio::select! { biased;
            _=shutdown.cancelled()=>false,
            _=self.stopped.cancelled()=>false,
            _=self.released.cancelled()=>!shutdown.is_cancelled()&&self.allows_execution(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;

    #[test]
    fn resident_admission_is_bounded_at_start_and_interruptible_after_start() {
        let late = StartupExecutionLatch::closed();
        assert!(!late.release_service_before(Instant::now()));
        assert!(late.pending());
        let resident = StartupExecutionLatch::closed();
        assert!(resident.release_service_before(Instant::now() + Duration::from_secs(10)));
        assert!(resident.original_deadline.lock().unwrap().is_none());
        assert!(resident.allows_execution());
        resident.stop();
        assert!(!resident.allows_execution());
    }

    #[test]
    fn timed_admission_cannot_convert_to_a_resident_latch() {
        let latch = StartupExecutionLatch::closed();
        let end = Instant::now() + Duration::from_secs(10);
        assert!(latch.release_before(end));
        assert!(!latch.release_service_before(end + Duration::from_secs(10)));
        assert_eq!(*latch.original_deadline.lock().unwrap(), Some(end));
    }
}
