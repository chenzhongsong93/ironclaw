//! Read-only access to a canonical thread plan owned by a thread service.
//!
//! The plan payload remains opaque until its owning producer publishes a
//! versioned schema; readers must derive access from the authenticated resource
//! scope rather than a thread id supplied by the model.

use async_trait::async_trait;
use ironclaw_host_api::ResourceScope;
use serde_json::Value;

use crate::SessionThreadError;

#[async_trait]
pub trait ThreadPlanReader: Send + Sync {
    async fn read(&self, scope: &ResourceScope) -> Result<Option<Value>, SessionThreadError>;
}
