//! Safe, bounded observability vocabulary (see `docs/contracts/errors-and-telemetry.md`).
//!
//! Types here carry fixed code points only. They never own bodies, URLs, credentials,
//! findings, or offending text. Exact code spellings and HTTP status mappings are fixed
//! in #4/#18.

/// Gateway-owned safe error categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SafeCode {
    MalformedInput,
    UnsupportedInput,
    LimitExceeded,
    IncompleteInspection,
    Overload,
    TransportFailure,
    /// Static configuration failed validation (startup only; never a request outcome).
    InvalidConfig,
    /// Readiness is false: validated plan or required initialization is missing.
    NotReady,
    /// The request passed admission and validation, but forwarding does not exist yet
    /// (until #19/#20). Never forwarded; replaced by the real outcome when wired.
    NotImplemented,
}

impl SafeCode {
    /// Stable, fixed string for the category. Never includes request content.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MalformedInput => "malformed_input",
            Self::UnsupportedInput => "unsupported_input",
            Self::LimitExceeded => "limit_exceeded",
            Self::IncompleteInspection => "incomplete_inspection",
            Self::Overload => "overload",
            Self::TransportFailure => "transport_failure",
            Self::InvalidConfig => "invalid_config",
            Self::NotReady => "not_ready",
            Self::NotImplemented => "not_implemented",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SafeCode;

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(SafeCode::MalformedInput.as_str(), "malformed_input");
        assert_eq!(SafeCode::Overload.as_str(), "overload");
    }
}
