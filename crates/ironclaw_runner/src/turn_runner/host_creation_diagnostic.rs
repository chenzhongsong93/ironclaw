//! Closed host-construction telemetry. Private reasons never enter formatting.
use ironclaw_turns::run_profile::AgentLoopHostErrorKind;

use super::HostFactoryError;

/// The original operation that failed, held only on this build's stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCreationStage {
    DriverRequirements,
    CapabilityFactory,
    CapabilityPort,
    ContextScope,
    SurfaceResolve,
    ModelRoute,
    HookDispatcher,
    EventSubscription,
    VisibleCapabilities,
    SubagentPrompt,
    Cancellation,
}

impl HostCreationStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DriverRequirements => "driver-requirements",
            Self::CapabilityFactory => "capability-factory",
            Self::CapabilityPort => "capability-port",
            Self::ContextScope => "context-scope",
            Self::SurfaceResolve => "surface-resolve",
            Self::ModelRoute => "model-route",
            Self::HookDispatcher => "hook-dispatcher",
            Self::EventSubscription => "event-subscription",
            Self::VisibleCapabilities => "visible-capabilities",
            Self::SubagentPrompt => "subagent-prompt",
            Self::Cancellation => "cancellation",
        }
    }
}

/// Both coordinates come from enums, never from a backend reason string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCreationDiagnostic {
    stage: HostCreationStage,
    kind: AgentLoopHostErrorKind,
}

impl HostCreationDiagnostic {
    pub fn new(stage: HostCreationStage, kind: AgentLoopHostErrorKind) -> Self {
        Self { stage, kind }
    }

    pub fn detail_code(self) -> String {
        format!(
            "host-safe.host-create.{}.{}",
            self.stage.as_str(),
            self.kind.as_str().replace('_', "-")
        )
    }
}

/// Keeps the legacy reason in RAM without exposing it through Debug/Display.
#[derive(Clone, PartialEq, Eq)]
pub struct HostCreationFailure {
    legacy_error: HostFactoryError,
    diagnostic: Option<HostCreationDiagnostic>,
}

impl HostCreationFailure {
    pub fn without_diagnostic(error: HostFactoryError) -> Self {
        Self {
            legacy_error: error,
            diagnostic: None,
        }
    }

    pub fn with_diagnostic(error: HostFactoryError, diagnostic: HostCreationDiagnostic) -> Self {
        Self {
            legacy_error: error,
            diagnostic: Some(diagnostic),
        }
    }

    pub fn diagnostic(&self) -> Option<HostCreationDiagnostic> {
        self.diagnostic
    }

    pub fn into_legacy_error(self) -> HostFactoryError {
        self.legacy_error
    }
}

impl std::fmt::Debug for HostCreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCreationFailure")
            .field("diagnostic", &self.diagnostic)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for HostCreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.diagnostic {
            Some(diagnostic) => f.write_str(&diagnostic.detail_code()),
            None => f.write_str("host factory error"),
        }
    }
}

impl std::error::Error for HostCreationFailure {}
