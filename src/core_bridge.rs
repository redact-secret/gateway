//! Boundary to the pinned RedactSecret core (ADR 0004, ADR 0006, core-completeness contract).
//!
//! Scaffold only. This module owns the interpretation of core results and, from #5, the
//! worker execution of core calls. It does not construct a `DetectorRegistry`: the pinned
//! core's registry is `!Send + !Sync`, so there is deliberately no shared `Arc<Registry>`
//! and no global mutex. #5 decides the per-owner construction strategy from measurements.
//!
//! Gateway code never reimplements detection (ADR 0001).

use std::fmt;

use redact_secret::Profile;

use crate::telemetry::SafeCode;

/// Exact core version this gateway build is pinned to. A test compares it to `Cargo.lock`.
pub const PINNED_CORE_VERSION: &str = "0.1.0-beta.12";

/// Resolve a configured core profile name using the core's own parser.
///
/// # Errors
/// [`CoreBridgeError::UnsupportedProfile`] for any name the pinned core does not know.
pub fn parse_profile(name: &str) -> Result<Profile, CoreBridgeError> {
    Profile::from_name(name).ok_or(CoreBridgeError::UnsupportedProfile)
}

/// Proof that core inspected the entire input and produced valid output. Only this module
/// can mint it, and only `boundary::approve` consumes it. Under the pinned core, `Ok` from
/// a whole-input call is the completeness signal and every `Err` is incomplete (#5
/// verifies and implements the mapping).
pub struct CompleteInspection {
    output: Vec<u8>,
}

impl CompleteInspection {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "minted by the #5 core probe and #19 inspection")
    )]
    fn new(output: Vec<u8>) -> Self {
        Self { output }
    }

    #[cfg(test)]
    pub(crate) fn for_test(output: Vec<u8>) -> Self {
        Self::new(output)
    }

    pub(crate) fn into_output(self) -> Vec<u8> {
        self.output
    }
}

impl fmt::Debug for CompleteInspection {
    /// Never prints output content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompleteInspection")
            .field("output_len", &self.output.len())
            .finish()
    }
}

/// Safe core-bridge failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreBridgeError {
    UnsupportedProfile,
    /// Any non-complete core outcome. Fail closed.
    Incomplete,
}

impl CoreBridgeError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::UnsupportedProfile => SafeCode::UnsupportedInput,
            Self::Incomplete => SafeCode::IncompleteInspection,
        }
    }
}

impl fmt::Display for CoreBridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for CoreBridgeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_use_core_parser() {
        assert!(parse_profile("full").is_ok());
        assert!(parse_profile("common").is_ok());
        assert_eq!(
            parse_profile("nope").unwrap_err(),
            CoreBridgeError::UnsupportedProfile
        );
    }
}
