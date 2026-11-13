//! Closed, provider-neutral host failure codes. Never carries raw error text.
use crate::LoopDiagnosticRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ContextStateReadFailed,
    ContextRecordReadFailed,
    ContextRecordMissing,
    ContextMatchMismatch,
    SigningFailed,
    HttpSendFailed,
    HttpStatus(u16),
    ResponseTooLarge,
    ResponseInvalidJson,
    ResponseUnexpectedOutcome,
    ResponseInvalidRecord,
}

/// A closed host failure vocabulary, not authority or a public failure detail.
/// The private representation prevents unchecked stage/reason/status construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSafeDiagnostic {
    kind: Kind,
}

impl HostSafeDiagnostic {
    pub fn context_state_read_failed() -> Self {
        Self {
            kind: Kind::ContextStateReadFailed,
        }
    }
    pub fn context_record_read_failed() -> Self {
        Self {
            kind: Kind::ContextRecordReadFailed,
        }
    }
    pub fn context_record_missing() -> Self {
        Self {
            kind: Kind::ContextRecordMissing,
        }
    }
    pub fn context_match_mismatch() -> Self {
        Self {
            kind: Kind::ContextMatchMismatch,
        }
    }
    pub fn signing_failed() -> Self {
        Self {
            kind: Kind::SigningFailed,
        }
    }
    pub fn http_send_failed() -> Self {
        Self {
            kind: Kind::HttpSendFailed,
        }
    }
    pub fn http_status(status: u16) -> Option<Self> {
        if (100..=599).contains(&status) && !(200..=299).contains(&status) {
            Some(Self {
                kind: Kind::HttpStatus(status),
            })
        } else {
            None
        }
    }
    pub fn response_too_large() -> Self {
        Self {
            kind: Kind::ResponseTooLarge,
        }
    }
    pub fn response_invalid_json() -> Self {
        Self {
            kind: Kind::ResponseInvalidJson,
        }
    }
    pub fn response_unexpected_outcome() -> Self {
        Self {
            kind: Kind::ResponseUnexpectedOutcome,
        }
    }
    pub fn response_invalid_record() -> Self {
        Self {
            kind: Kind::ResponseInvalidRecord,
        }
    }

    /// Parse the complete canonical diagnostic reference. No trimming or aliases.
    pub fn parse(value: &str) -> Option<Self> {
        let code = value.strip_prefix("diag:host-safe.")?;
        let fixed = match code {
            "context-state.read-failed" => Some(Self::context_state_read_failed()),
            "context-record.read-failed" => Some(Self::context_record_read_failed()),
            "context-record.missing" => Some(Self::context_record_missing()),
            "context-match.mismatch" => Some(Self::context_match_mismatch()),
            "signing.failed" => Some(Self::signing_failed()),
            "http.send-failed" => Some(Self::http_send_failed()),
            "response.too-large" => Some(Self::response_too_large()),
            "response.invalid-json" => Some(Self::response_invalid_json()),
            "response.unexpected-outcome" => Some(Self::response_unexpected_outcome()),
            "response.invalid-record" => Some(Self::response_invalid_record()),
            _ => None,
        };
        if fixed.is_some() {
            return fixed;
        }
        let digits = code.strip_prefix("http.status-")?;
        if digits.len() != 3 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let status: u16 = digits.parse().ok()?;
        let diagnostic = Self::http_status(status)?;
        if diagnostic.detail_code() != format!("host-safe.{code}") {
            return None;
        }
        Some(diagnostic)
    }

    pub fn from_diagnostic_ref(value: &LoopDiagnosticRef) -> Option<Self> {
        Self::parse(value.as_str())
    }

    pub fn detail_code(self) -> String {
        let suffix = match self.kind {
            Kind::ContextStateReadFailed => "context-state.read-failed",
            Kind::ContextRecordReadFailed => "context-record.read-failed",
            Kind::ContextRecordMissing => "context-record.missing",
            Kind::ContextMatchMismatch => "context-match.mismatch",
            Kind::SigningFailed => "signing.failed",
            Kind::HttpSendFailed => "http.send-failed",
            Kind::HttpStatus(status) => return format!("host-safe.http.status-{status}"),
            Kind::ResponseTooLarge => "response.too-large",
            Kind::ResponseInvalidJson => "response.invalid-json",
            Kind::ResponseUnexpectedOutcome => "response.unexpected-outcome",
            Kind::ResponseInvalidRecord => "response.invalid-record",
        };
        format!("host-safe.{suffix}")
    }

    pub fn diagnostic_ref(self) -> LoopDiagnosticRef {
        // All suffixes are closed ASCII constants. HttpStatus can only enter through
        // the bounded non-2xx constructor. Exhaustive round-trip tests cover this
        // internal invariant; untrusted input cannot construct the private Kind.
        LoopDiagnosticRef::new(format!("diag:{}", self.detail_code()))
            .expect("closed host-safe diagnostic satisfies diagnostic reference grammar")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_typed_codes_round_trip_and_never_copy_private_text() {
        let fixed = [
            HostSafeDiagnostic::context_state_read_failed(),
            HostSafeDiagnostic::context_record_read_failed(),
            HostSafeDiagnostic::context_record_missing(),
            HostSafeDiagnostic::context_match_mismatch(),
            HostSafeDiagnostic::signing_failed(),
            HostSafeDiagnostic::http_send_failed(),
            HostSafeDiagnostic::response_too_large(),
            HostSafeDiagnostic::response_invalid_json(),
            HostSafeDiagnostic::response_unexpected_outcome(),
            HostSafeDiagnostic::response_invalid_record(),
        ];
        for diagnostic in fixed {
            let reference = diagnostic.diagnostic_ref();
            assert_eq!(
                HostSafeDiagnostic::from_diagnostic_ref(&reference),
                Some(diagnostic)
            );
            assert_eq!(
                reference.as_str(),
                format!("diag:{}", diagnostic.detail_code())
            );
            assert!(!diagnostic.detail_code().contains("SECRETsentinel"));
        }
        for status in 0..=u16::MAX {
            let allowed = (100..=599).contains(&status) && !(200..=299).contains(&status);
            assert_eq!(HostSafeDiagnostic::http_status(status).is_some(), allowed);
            if let Some(diagnostic) = HostSafeDiagnostic::http_status(status) {
                assert_eq!(
                    HostSafeDiagnostic::from_diagnostic_ref(&diagnostic.diagnostic_ref()),
                    Some(diagnostic)
                );
            }
        }
    }

    #[test]
    fn unknown_extra_secret_and_noncanonical_status_are_rejected() {
        for value in [
            "diag:host-safe.unknown.missing",
            "diag:host-safe.context-state.missing",
            "diag:host-safe.context-record.mismatch",
            "diag:host-safe.context-match.read-failed",
            "diag:host-safe.signing.read-failed",
            "diag:host-safe.response.missing",
            "diag:host-safe.http.status-99",
            "diag:host-safe.http.status-600",
            "diag:host-safe.http.status-200",
            "diag:host-safe.http.status-204",
            "diag:host-safe.http.status-299",
            "diag:host-safe.http.status-0403",
            "diag:host-safe.http.status-+403",
            "diag:host-safe.http.status-403.0",
            "diag:host-safe.http.status-403.extra",
            "diag:host-safe.http.status-403-SECRETsentinel",
            "diag:host-safe.context-match.mismatch.SECRETsentinel",
            "diag:host-safe.Context-match.mismatch",
            "diag:host-safe.context-match.MISMATCH",
            "diag:host-safe.context-record.missing-extra",
            " diag:host-safe.http.status-403",
            "diag:host-safe.http.status-403 ",
            "host-safe.http.status-403",
            "diag:SECRETsentinel",
        ] {
            assert_eq!(HostSafeDiagnostic::parse(value), None, "{value}");
        }
    }
}
