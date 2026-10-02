//! Boundary to the pinned RedactSecret core (ADR 0004, ADR 0006, core-completeness contract).
//!
//! This module owns the interpretation of core results and the worker execution of core
//! calls ([`pool`]). It uses only public core APIs. The pinned core's registry is
//! `!Send + !Sync`, so there is deliberately no shared `Arc<Registry>` and no global mutex:
//! an immutable, shareable [`InspectorSpec`] is built at startup and every owning thread
//! builds its own [`Inspector`] from it.
//!
//! Completeness (verified in `docs/probes/core-bridge-probe.md`): the core's whole-input
//! calls return `Ok` only when every detector inspected the whole input and every failure or
//! limit is a distinct `Err`; nothing is truncated or partial. [`CompleteInspection`] is
//! therefore minted only through [`RequestScope::finish`], which refuses once any leaf failed.
//!
//! Gateway code never reimplements detection (ADR 0001), never reads a client "already
//! scanned" claim, and never echoes request text or core messages in an error.

pub mod pool;

use std::fmt;

use redact_secret::{
    Action, DefaultPolicy, DetectorRegistry, Finding, FormatterFailure, PiiSelection,
    PlaceholderContext, PlaceholderFormatter, Profile, SecretScanError, SecretScanErrorCode,
    WholeInputLimits, scan_and_redact_with_limits,
};

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

/// Parse PII selectors using the core's own parser (the pinned core has no `pii:kr`).
///
/// # Errors
/// [`CoreBridgeError::UnsupportedProfile`] for any selector the pinned core rejects.
pub fn parse_pii(selectors: &[&str]) -> Result<PiiSelection, CoreBridgeError> {
    PiiSelection::parse(selectors).map_err(|e| map_core_error(&e))
}

/// Counts for one request inspection. Never content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InspectionSummary {
    /// Text leaves inspected (each a whole-input core call that returned `Ok`).
    pub leaves: usize,
    /// UTF-8 bytes of decoded text handed to the core.
    pub input_bytes: usize,
    /// Findings the core replaced with placeholders.
    pub redactions: usize,
    /// Findings the default policy left in the text (`Warn`/`Allow`). Reported because
    /// "no redactions" is not "no findings" (core-completeness contract).
    pub unredacted_findings: usize,
    /// Of those, findings whose action is `Warn` (not `Allow`).
    pub warned_findings: usize,
}

/// Proof that core inspected the entire input and produced valid output. Only this module
/// can mint it (through [`RequestScope::finish`]) and only `boundary::approve` consumes it.
/// Under the pinned core, `Ok` from a whole-input call is the completeness signal and every
/// `Err` is incomplete; the mapping is [`map_core_error`].
pub struct CompleteInspection {
    output: Vec<u8>,
    summary: InspectionSummary,
}

impl CompleteInspection {
    fn new(output: Vec<u8>, summary: InspectionSummary) -> Self {
        Self { output, summary }
    }

    #[cfg(test)]
    pub(crate) fn for_test(output: Vec<u8>) -> Self {
        Self::new(output, InspectionSummary::default())
    }

    pub(crate) fn into_output(self) -> Vec<u8> {
        self.output
    }

    /// Counts for this inspection. Contains no request content.
    #[must_use]
    pub const fn summary(&self) -> InspectionSummary {
        self.summary
    }
}

impl fmt::Debug for CompleteInspection {
    /// Never prints output content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompleteInspection")
            .field("output_len", &self.output.len())
            .field("summary", &self.summary)
            .finish()
    }
}

/// Safe core-bridge failure. Carries no request content, finding, or core message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreBridgeError {
    /// Profile or PII selector the pinned core does not know or does not support.
    UnsupportedProfile,
    /// Limits the pinned core rejects (for example zero). A configuration defect.
    InvalidConfiguration,
    /// Input bytes or finding count over a bound. The core never truncates.
    LimitExceeded,
    /// The policy decided `Block`: the core says the input must not proceed.
    Blocked,
    /// A `Warn` finding was left in the text and the content policy is `on_warn = reject`.
    Warned,
    /// Inspection queue full, or the pool is shutting down.
    Overload,
    /// Any other non-complete outcome: detector, policy or placeholder failure, discarded
    /// or cancelled job, worker panic. Fail closed.
    Incomplete,
}

impl CoreBridgeError {
    /// Provisional mapping; exact code spellings and statuses are fixed in #4/#18.
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::UnsupportedProfile
            | Self::InvalidConfiguration
            | Self::Blocked
            | Self::Warned => SafeCode::UnsupportedInput,
            Self::LimitExceeded => SafeCode::LimitExceeded,
            Self::Overload => SafeCode::Overload,
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

/// Map a core error to a gateway failure. Only named limit and configuration codes get a
/// specific class; everything else, including any code a future core adds, is `Incomplete`,
/// so no unknown outcome can read as success.
#[must_use]
pub fn map_core_error(error: &SecretScanError) -> CoreBridgeError {
    match error.code() {
        SecretScanErrorCode::InputLimitExceeded
        | SecretScanErrorCode::FindingLimitExceeded
        | SecretScanErrorCode::BufferLimitExceeded
        | SecretScanErrorCode::TokenLimitExceeded
        | SecretScanErrorCode::MultilineLimitExceeded => CoreBridgeError::LimitExceeded,
        SecretScanErrorCode::InvalidLimits | SecretScanErrorCode::InvalidOptions => {
            CoreBridgeError::InvalidConfiguration
        }
        SecretScanErrorCode::PiiSelectorInvalid
        | SecretScanErrorCode::PiiSelectorUnsupported
        | SecretScanErrorCode::PiiSelectorUnavailable
        | SecretScanErrorCode::PiiActivationConflict => CoreBridgeError::UnsupportedProfile,
        _ => CoreBridgeError::Incomplete,
    }
}

/// Immutable `Send + Sync` description of what an inspector runs: profile, PII selection,
/// and request-wide limits. It never holds the registry (`!Send`).
#[derive(Clone, Debug)]
pub struct InspectorSpec {
    profile: Profile,
    pii: PiiSelection,
    limits: WholeInputLimits,
    reject_warnings: bool,
}

impl InspectorSpec {
    /// No defaults: every value comes from validated configuration (#4) and measurements.
    ///
    /// # Errors
    /// [`CoreBridgeError::UnsupportedProfile`] for an unknown profile or PII selector (the
    /// pinned core has no `pii:kr`); [`CoreBridgeError::InvalidConfiguration`] for zero limits.
    pub fn new(
        profile_name: &str,
        pii_selectors: &[&str],
        max_input_bytes: usize,
        max_findings: usize,
    ) -> Result<Self, CoreBridgeError> {
        Self::for_profile(
            parse_profile(profile_name)?,
            pii_selectors,
            max_input_bytes,
            max_findings,
        )
    }

    /// As [`new`](Self::new) for an already parsed profile.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn for_profile(
        profile: Profile,
        pii_selectors: &[&str],
        max_input_bytes: usize,
        max_findings: usize,
    ) -> Result<Self, CoreBridgeError> {
        let pii = PiiSelection::parse(pii_selectors).map_err(|e| map_core_error(&e))?;
        let limits =
            WholeInputLimits::new(max_input_bytes, max_findings).map_err(|e| map_core_error(&e))?;
        Ok(Self {
            profile,
            pii,
            limits,
            reject_warnings: false,
        })
    }

    /// Treat any `Warn` finding as a hard failure ([`CoreBridgeError::Warned`]). Off by
    /// default here; the content policy turns it on unless the operator chose `forward`.
    #[must_use]
    pub const fn with_warning_rejection(mut self, reject: bool) -> Self {
        self.reject_warnings = reject;
        self
    }

    /// The request-wide bounds this spec enforces.
    #[must_use]
    pub const fn limits(&self) -> WholeInputLimits {
        self.limits
    }
}

/// Request-wide placeholder numbering: the core restarts at 1 on every call, so the gateway
/// offsets by the replacements already made in this request.
struct RequestFormatter {
    offset: usize,
}

impl PlaceholderFormatter for RequestFormatter {
    fn format(
        &self,
        _finding: &Finding,
        context: &PlaceholderContext,
    ) -> Result<String, FormatterFailure> {
        Ok(format!(
            "<SECRET_{}>",
            self.offset.saturating_add(context.placeholder_index())
        ))
    }
}

/// Mutable state of one request: placeholder numbering, cumulative limits, and whether any
/// leaf failed. Owned, not `Clone`, never shared between requests. Any failure poisons the
/// scope so [`finish`](Self::finish) can never mint a proof afterwards.
#[derive(Debug)]
pub struct RequestScope {
    max_input_bytes: usize,
    max_findings: usize,
    summary: InspectionSummary,
    findings: usize,
    poisoned: bool,
}

impl RequestScope {
    /// A fresh scope bounded by the spec's limits, which apply to the whole request.
    #[must_use]
    pub fn new(spec: &InspectorSpec) -> Self {
        Self {
            max_input_bytes: spec.limits.max_input_bytes(),
            max_findings: spec.limits.max_findings(),
            summary: InspectionSummary::default(),
            findings: 0,
            poisoned: false,
        }
    }

    /// Counts so far.
    #[must_use]
    pub const fn summary(&self) -> InspectionSummary {
        self.summary
    }

    /// Whether any leaf in this scope has failed.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Mint the completeness proof for `output`, the body the caller rebuilt from the
    /// inspected leaves. The bridge cannot verify that `output` was derived only from texts
    /// inspected in this scope; that is the protocol/boundary contract (ADR 0007).
    ///
    /// # Errors
    /// [`CoreBridgeError::Incomplete`] if any leaf in this scope failed.
    pub fn finish(self, output: Vec<u8>) -> Result<CompleteInspection, CoreBridgeError> {
        if self.poisoned {
            return Err(CoreBridgeError::Incomplete);
        }
        Ok(CompleteInspection::new(output, self.summary))
    }
}

/// Redacted text of one inspected leaf. Debug never prints the text.
pub struct InspectedText {
    text: String,
}

impl InspectedText {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.text
    }
}

impl fmt::Debug for InspectedText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectedText")
            .field("len", &self.text.len())
            .finish()
    }
}

/// Owner of one core `DetectorRegistry`: `!Send + !Sync` because the registry is. Build one
/// per owning thread from a shared [`InspectorSpec`]. Uses only public core APIs and the
/// core's `DefaultPolicy`.
#[derive(Debug)]
pub struct Inspector {
    registry: DetectorRegistry,
    limits: WholeInputLimits,
    reject_warnings: bool,
}

impl Inspector {
    /// # Errors
    /// [`CoreBridgeError::Incomplete`] if the core cannot build the registry.
    pub fn new(spec: &InspectorSpec) -> Result<Self, CoreBridgeError> {
        let built = match spec.profile {
            Profile::Common => DetectorRegistry::with_common_built_in_and_pii(&spec.pii),
            Profile::Full => DetectorRegistry::with_built_in_and_pii(&spec.pii),
        };
        let registry = built.map_err(|_| CoreBridgeError::Incomplete)?;
        Ok(Self {
            registry,
            limits: spec.limits,
            reject_warnings: spec.reject_warnings,
        })
    }

    /// Core activation identity (profile, selectors, vocabulary). Safe to report.
    #[must_use]
    pub fn activation_identity(&self) -> &str {
        self.registry.activation_identity()
    }

    /// Inspect one decoded text leaf. Callers traverse in the documented deterministic order
    /// and pass decoded text, never raw JSON.
    ///
    /// `Ok` means the core inspected the whole leaf with every registered detector and
    /// returned redacted text. Any `Err` poisons `scope`.
    ///
    /// # Errors
    /// [`CoreBridgeError::LimitExceeded`] over the request-wide byte or finding bound;
    /// [`CoreBridgeError::Blocked`] if the policy blocked a finding;
    /// [`CoreBridgeError::Incomplete`] for every other core failure.
    pub fn inspect_text(
        &self,
        scope: &mut RequestScope,
        text: &str,
    ) -> Result<InspectedText, CoreBridgeError> {
        let outcome = self.inspect_inner(scope, text);
        if outcome.is_err() {
            scope.poisoned = true;
        }
        outcome
    }

    /// Detect-only check for a validated structural identifier that must never be rewritten
    /// (for example `model`): any finding of any action is [`CoreBridgeError::Blocked`].
    /// Counts nothing against the request scope's numbering.
    ///
    /// # Errors
    /// [`CoreBridgeError::Blocked`] when the core finds anything;
    /// otherwise as [`inspect_text`](Self::inspect_text).
    pub fn reject_if_findings(&self, text: &str) -> Result<(), CoreBridgeError> {
        let formatter = RequestFormatter { offset: 0 };
        let result = scan_and_redact_with_limits(
            text,
            &self.registry,
            &DefaultPolicy,
            &formatter,
            &self.limits,
        )
        .map_err(|e| map_core_error(&e))?;
        if result.findings().is_empty() {
            Ok(())
        } else {
            Err(CoreBridgeError::Blocked)
        }
    }

    fn inspect_inner(
        &self,
        scope: &mut RequestScope,
        text: &str,
    ) -> Result<InspectedText, CoreBridgeError> {
        if scope.poisoned {
            return Err(CoreBridgeError::Incomplete);
        }
        let total = scope
            .summary
            .input_bytes
            .checked_add(text.len())
            .ok_or(CoreBridgeError::LimitExceeded)?;
        if total > scope.max_input_bytes {
            return Err(CoreBridgeError::LimitExceeded);
        }
        let formatter = RequestFormatter {
            offset: scope.summary.redactions,
        };
        let result = scan_and_redact_with_limits(
            text,
            &self.registry,
            &DefaultPolicy,
            &formatter,
            &self.limits,
        )
        .map_err(|e| map_core_error(&e))?;
        let findings = result.findings();
        if findings.iter().any(|f| f.action() == Action::Block) {
            return Err(CoreBridgeError::Blocked);
        }
        let warned = findings
            .iter()
            .filter(|f| f.action() == Action::Warn)
            .count();
        if warned > 0 && self.reject_warnings {
            return Err(CoreBridgeError::Warned);
        }
        let total_findings = scope
            .findings
            .checked_add(findings.len())
            .ok_or(CoreBridgeError::LimitExceeded)?;
        if total_findings > scope.max_findings {
            return Err(CoreBridgeError::LimitExceeded);
        }
        let redactions = findings
            .iter()
            .filter(|f| f.action().replaces_text())
            .count();
        let unredacted = findings.len().saturating_sub(redactions);
        scope.findings = total_findings;
        scope.summary.leaves = scope.summary.leaves.saturating_add(1);
        scope.summary.input_bytes = total;
        scope.summary.redactions = scope.summary.redactions.saturating_add(redactions);
        scope.summary.unredacted_findings =
            scope.summary.unredacted_findings.saturating_add(unredacted);
        scope.summary.warned_findings = scope.summary.warned_findings.saturating_add(warned);
        let (text, _findings) = result.into_parts();
        Ok(InspectedText { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_token() -> String {
        format!("ghp_SYNTHETICREVOKED{:020}", 1)
    }

    #[test]
    fn warn_rejection_is_opt_in_on_the_spec_and_counted_otherwise() {
        let text = "password=hunter2xyz";
        let lenient = InspectorSpec::new("full", &[], 4096, 100).expect("spec");
        let mut scope = RequestScope::new(&lenient);
        let out = Inspector::new(&lenient)
            .expect("inspector")
            .inspect_text(&mut scope, text)
            .expect("complete");
        assert_eq!(out.as_str(), text);
        assert_eq!(scope.summary().warned_findings, 1);

        let strict = lenient.with_warning_rejection(true);
        let mut scope = RequestScope::new(&strict);
        let err = Inspector::new(&strict)
            .expect("inspector")
            .inspect_text(&mut scope, text)
            .expect_err("warn rejects");
        assert_eq!(err, CoreBridgeError::Warned);
        assert!(scope.is_poisoned());
        assert_eq!(
            scope.finish(Vec::new()).expect_err("poisoned"),
            CoreBridgeError::Incomplete
        );
    }

    #[test]
    fn detect_only_check_rejects_any_finding_and_never_rewrites() {
        let spec = InspectorSpec::new("full", &[], 4096, 100).expect("spec");
        let inspector = Inspector::new(&spec).expect("inspector");
        assert_eq!(inspector.reject_if_findings("gpt-4o-mini"), Ok(()));
        assert_eq!(
            inspector.reject_if_findings(&synthetic_token()),
            Err(CoreBridgeError::Blocked)
        );
        assert_eq!(
            inspector.reject_if_findings("password=hunter2xyz"),
            Err(CoreBridgeError::Blocked)
        );
    }

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
