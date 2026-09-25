//! Facade DTOs for the optional read-only thread-plan projection.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebornGetThreadPlanRequest {
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebornGetThreadPlanResponse {
    pub plan: Option<Value>,
}
