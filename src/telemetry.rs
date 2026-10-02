//! Safe, bounded observability vocabulary (see `docs/contracts/errors-and-telemetry.md`).
//!
//! Types here carry fixed code points only. They never own bodies, URLs, credentials,
//! findings, or offending text. Exact code spellings and HTTP status mappings are fixed
//! in #4/#18.
//!
//! [`Metrics`] (#20, ADR 0008) holds coarse stage timings and one attempt counter as plain
//! atomics. Its vocabulary is closed: a [`Stage`] is a fixed enum, so no payload, route,
//! credential, or caller-chosen label can ever become a metric dimension.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
    /// The request cannot be served by this build or deployment: `stream: true` until SSE
    /// relay lands (#21), or no upstream is configured. Never forwarded.
    NotImplemented,
    /// The caller supplied no usable provider `Authorization` (#24). Distinct from every
    /// local-authentication outcome (Beta 1 #12): this credential is the provider's.
    MissingCredential,
    /// The provider did not answer within a connect, response-header, or total deadline (#20).
    UpstreamTimeout,
    /// The provider could not be reached: resolution, address policy, refused, or reset
    /// before the request was written (#20).
    UpstreamUnavailable,
    /// TLS to the provider failed: certificate, hostname, or handshake (#20).
    UpstreamTls,
    /// The provider answered with something that cannot be relayed: malformed or truncated
    /// HTTP, a disconnect after the request was sent, or an unsupported content coding (#20).
    UpstreamInvalidResponse,
    /// The provider response headers or body exceeded the configured bounds (#20).
    UpstreamResponseTooLarge,
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
            Self::MissingCredential => "missing_credential",
            Self::UpstreamTimeout => "upstream_timeout",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::UpstreamTls => "upstream_tls_failure",
            Self::UpstreamInvalidResponse => "upstream_invalid_response",
            Self::UpstreamResponseTooLarge => "upstream_response_too_large",
        }
    }
}

/// A Gateway-side stage whose duration is recorded. Closed set; no payload labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// Waiting for receipt and memory capacity before the body is read.
    AdmissionWait,
    /// One strict parse and endpoint validation.
    Parse,
    /// Core inspection and approval, including queueing for the worker.
    Inspection,
    /// Serializing the transformed request (part of inspection, measured on the worker).
    Serialization,
    /// From initiating the upstream send to the response headers.
    UpstreamFirstResponse,
    /// From initiating the upstream send to the last buffered body byte (or failure).
    UpstreamTotal,
}

/// Count, total, and maximum of one stage. Saturating; never resets.
#[derive(Debug, Default)]
struct StageCell {
    count: AtomicU64,
    total_micros: AtomicU64,
    max_micros: AtomicU64,
}

/// A point-in-time copy of one stage's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageSnapshot {
    pub count: u64,
    pub total_micros: u64,
    pub max_micros: u64,
}

/// Bounded safe counters shared by the request path. No export endpoint exists yet (#20
/// adds only the minimum ADR 0008 needs); tests and a later exporter read [`Self::stage`].
#[derive(Debug, Default)]
pub struct Metrics {
    admission_wait: StageCell,
    parse: StageCell,
    inspection: StageCell,
    serialization: StageCell,
    upstream_first_response: StageCell,
    upstream_total: StageCell,
    upstream_attempts: AtomicU64,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    const fn cell(&self, stage: Stage) -> &StageCell {
        match stage {
            Stage::AdmissionWait => &self.admission_wait,
            Stage::Parse => &self.parse,
            Stage::Inspection => &self.inspection,
            Stage::Serialization => &self.serialization,
            Stage::UpstreamFirstResponse => &self.upstream_first_response,
            Stage::UpstreamTotal => &self.upstream_total,
        }
    }

    /// Record one observation of `stage`.
    pub fn record(&self, stage: Stage, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let cell = self.cell(stage);
        cell.count.fetch_add(1, Ordering::Relaxed);
        let _ = cell
            .total_micros
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                Some(t.saturating_add(micros))
            });
        cell.max_micros.fetch_max(micros, Ordering::Relaxed);
    }

    #[must_use]
    pub fn stage(&self, stage: Stage) -> StageSnapshot {
        let cell = self.cell(stage);
        StageSnapshot {
            count: cell.count.load(Ordering::Relaxed),
            total_micros: cell.total_micros.load(Ordering::Relaxed),
            max_micros: cell.max_micros.load(Ordering::Relaxed),
        }
    }

    /// Count one upstream send attempt (initiated, whatever its outcome).
    pub fn note_upstream_attempt(&self) {
        self.upstream_attempts.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn upstream_attempts(&self) -> u64 {
        self.upstream_attempts.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::{Metrics, SafeCode, Stage, StageSnapshot};
    use std::time::Duration;

    #[test]
    fn metrics_accumulate_without_labels() {
        let m = Metrics::new();
        assert_eq!(m.stage(Stage::Parse), StageSnapshot::default());
        m.record(Stage::Parse, Duration::from_micros(10));
        m.record(Stage::Parse, Duration::from_micros(30));
        assert_eq!(
            m.stage(Stage::Parse),
            StageSnapshot {
                count: 2,
                total_micros: 40,
                max_micros: 30
            }
        );
        assert_eq!(m.stage(Stage::Inspection).count, 0);
        m.note_upstream_attempt();
        assert_eq!(m.upstream_attempts(), 1);
    }

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(SafeCode::MalformedInput.as_str(), "malformed_input");
        assert_eq!(SafeCode::Overload.as_str(), "overload");
        assert_eq!(SafeCode::UpstreamTls.as_str(), "upstream_tls_failure");
    }
}
