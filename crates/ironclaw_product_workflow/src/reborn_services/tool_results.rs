use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RebornToolResultRequest {
    pub thread_id: String,
    pub run_id: String,
    pub result_ref: String,
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornToolResultResponse {
    pub result_ref: String,
    pub run_id: String,
    pub invocation_id: String,
    /// Complete redacted output, retaining valid JSON and nested JSON text.
    pub content: String,
}

pub(super) const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
