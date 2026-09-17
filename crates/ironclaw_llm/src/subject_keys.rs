//! Request-scoped LLM key resolution for server-to-server identity labels.
//!
//! The provider receives only `llm_subject` in request metadata. Secret
//! material stays behind this resolver boundary and is never copied into
//! metadata, traces, or persisted turn state (ISSUE-IRONCLAW-008).

use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use secrecy::SecretString;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubjectKeyPolicy {
    pub required: bool,
}

impl SubjectKeyPolicy {
    pub const OPTIONAL: Self = Self { required: false };
    pub const REQUIRED: Self = Self { required: true };
}

#[derive(Debug, Error)]
pub enum SubjectKeyResolveError {
    #[error("llm subject is required")]
    SubjectRequired,
    #[error("no LLM key is configured for subject")]
    UnknownSubject,
    #[error("LLM subject key store is unavailable")]
    StoreUnavailable,
}

#[async_trait]
pub trait SubjectKeyResolver: Send + Sync {
    async fn resolve_bearer_token(
        &self,
        subject: &str,
        provider_id: &str,
    ) -> Result<Option<SecretString>, SubjectKeyResolveError>;
}

#[derive(Clone)]
struct RegisteredResolver {
    resolver: Arc<dyn SubjectKeyResolver>,
    policy: SubjectKeyPolicy,
}

fn registry() -> &'static RwLock<Option<RegisteredResolver>> {
    static REGISTRY: OnceLock<RwLock<Option<RegisteredResolver>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(None))
}

/// Install the deployment-wide subject key resolver used by request-time
/// provider authentication. Re-registering atomically replaces the previous
/// resolver, which keeps runtime rebuilds and tests deterministic.
pub fn register_subject_key_resolver(
    resolver: Arc<dyn SubjectKeyResolver>,
    policy: SubjectKeyPolicy,
) {
    let mut guard = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(RegisteredResolver { resolver, policy });
}

pub(crate) fn registered_subject_key_resolver()
-> Option<(Arc<dyn SubjectKeyResolver>, SubjectKeyPolicy)> {
    let guard = registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .map(|entry| (Arc::clone(&entry.resolver), entry.policy))
}

#[cfg(test)]
pub(crate) fn clear_subject_key_resolver() {
    let mut guard = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}
