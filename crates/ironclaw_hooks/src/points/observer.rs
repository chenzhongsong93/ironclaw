//! Context for observer hook points (`after_model`, `after_capability`,
//! `after_checkpoint`).

use ironclaw_host_api::{ExtensionId, TenantId};
use ironclaw_turns::CapabilityActivityId;

/// Read-only context handed to an observer hook. As with the other points,
/// `#[non_exhaustive]` so additional fields can land without breaking authors.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ObserverHookContext {
    pub tenant_id: TenantId,
    pub observed_kind: ObservedKind,
    /// Provider of the observed capability. Populated only at
    /// [`ObservedKind::AfterCapability`]; `None` at the other kinds which have
    /// no per-capability resolution. Used by the dispatcher to enforce
    /// [`crate::registry::HookBindingScope::OwnCapabilities`] for Installed
    /// observers (serrrfirat finding #3).
    pub provider: Option<ExtensionId>,
    /// Stable, provider-safe capability identifier. Present only for
    /// [`ObservedKind::AfterCapability`]. The observer never receives the
    /// capability's input or output.
    pub capability_name: Option<String>,
    /// Stable identity of the capability invocation being observed. This is an
    /// opaque correlation token, not a capability input or result payload.
    pub capability_activity_id: Option<CapabilityActivityId>,
    /// Bounded dispatch classification. Present only for
    /// [`ObservedKind::AfterCapability`]; raw output and error detail are never
    /// included in observer context.
    pub capability_outcome: Option<ObservedCapabilityOutcome>,
}

impl ObserverHookContext {
    /// Construct the bounded context for one completed capability dispatch.
    pub fn after_capability(
        tenant_id: TenantId,
        capability_name: String,
        capability_activity_id: CapabilityActivityId,
        provider: Option<ExtensionId>,
        capability_outcome: ObservedCapabilityOutcome,
    ) -> Self {
        Self {
            tenant_id,
            observed_kind: ObservedKind::AfterCapability,
            provider,
            capability_name: Some(capability_name),
            capability_activity_id: Some(capability_activity_id),
            capability_outcome: Some(capability_outcome),
        }
    }
}

/// Outcome classification exposed to `AfterCapability` observers. This is
/// deliberately coarser than [`ironclaw_host_api::Resolution`]: observers can
/// coordinate lifecycle state without seeing result payloads, gate data, denial
/// reasons, or host error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObservedCapabilityOutcome {
    /// Dispatch produced a parking resolution (`Blocked` or `Suspended`).
    Parked,
    /// Dispatch returned a non-parking resolution, including recoverable
    /// failures and policy denials.
    Returned,
    /// The capability port returned a host error instead of a resolution.
    Errored,
}

/// What kind of fact the observer is being notified about. The dispatcher
/// dispatches one hook list per kind, so a single hook implementation is
/// scoped to one observation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedKind {
    /// A model call returned. The observer sees only that an exchange
    /// happened, never the model's raw output.
    AfterModel,
    /// A capability invocation completed (successfully or otherwise).
    AfterCapability,
    /// A checkpoint was written.
    AfterCheckpoint,
}
