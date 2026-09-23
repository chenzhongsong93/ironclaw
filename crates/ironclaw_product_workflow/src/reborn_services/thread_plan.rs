//! Thread-owned task snapshots. The runtime store owns the canonical plan DTO.

use ironclaw_threads::plan::ThreadPlanUpdate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornGetThreadPlanRequest {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornGetThreadPlanResponse {
    /// `None` means the authorized thread has no persisted plan. Read failures
    /// are service errors and must never be represented by an empty plan.
    pub plan: Option<ThreadPlanUpdate>,
}
