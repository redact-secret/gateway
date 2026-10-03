//! Safe, bounded observability vocabulary (see `docs/contracts/errors-and-telemetry.md`).
//!
//! Types here carry fixed code points only. They never own bodies, URLs, credentials,
//! findings, or offending text. Exact code spellings and HTTP status mappings are fixed
//! in #4/#18.
//!
//! [`Metrics`] (#20, ADR 0008) holds coarse stage timings and one attempt counter as plain
//! atomics, plus bounded SSE stream counters and a buffered-bytes gauge (#21). Its
//! vocabulary is closed: a [`Stage`] is a fixed enum, so no payload, route,
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
    /// The request cannot be served by this build or deployment: no upstream is configured
    /// for the route. Never forwarded.
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
    /// Stream: from initiating the send to the first provider body byte (#21).
    StreamFirstByte,
    /// Stream: from initiating the send to the end of the relay, however it ended (#21).
    StreamTotal,
    /// Stream: per stream, the sum of time spent waiting on the provider for the next
    /// chunk (#21). Total minus this and [`Stage::StreamDownstreamWait`] is relay overhead.
    StreamUpstreamWait,
    /// Stream: per stream, the sum of time between handing a chunk to the HTTP server and
    /// the server asking for the next one, that is, consumer and socket backpressure (#21).
    StreamDownstreamWait,
}

/// How an SSE stream ended (#21). Closed set; counters only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamEnd {
    /// The provider ended the stream cleanly and every byte was handed to the server.
    Completed,
    /// The provider connection failed or the response was malformed mid-stream.
    UpstreamError,
    /// No provider chunk within the idle deadline.
    IdleTimeout,
    /// The total stream lifetime deadline elapsed.
    LifetimeExceeded,
    /// A single provider chunk exceeded the relay buffer bound.
    BufferExceeded,
    /// The Gateway is shutting down and its drain deadline passed.
    Shutdown,
    /// The body was dropped before the stream ended: the caller disconnected, or the
    /// connection was closed by the write-stall deadline.
    Abandoned,
}

impl StreamEnd {
    const COUNT: usize = 7;

    const fn index(self) -> usize {
        match self {
            Self::Completed => 0,
            Self::UpstreamError => 1,
            Self::IdleTimeout => 2,
            Self::LifetimeExceeded => 3,
            Self::BufferExceeded => 4,
            Self::Shutdown => 5,
            Self::Abandoned => 6,
        }
    }
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
    stream_first_byte: StageCell,
    stream_total: StageCell,
    stream_upstream_wait: StageCell,
    stream_downstream_wait: StageCell,
    streams_started: AtomicU64,
    streams_ended: [AtomicU64; StreamEnd::COUNT],
    stream_bytes: AtomicU64,
    stream_buffered: AtomicU64,
    stream_buffered_peak: AtomicU64,
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
            Stage::StreamFirstByte => &self.stream_first_byte,
            Stage::StreamTotal => &self.stream_total,
            Stage::StreamUpstreamWait => &self.stream_upstream_wait,
            Stage::StreamDownstreamWait => &self.stream_downstream_wait,
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

    /// Count one stream whose response headers were committed to the caller (#21).
    pub fn note_stream_started(&self) {
        self.streams_started.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn streams_started(&self) -> u64 {
        self.streams_started.load(Ordering::Relaxed)
    }

    /// Count one stream end, by cause.
    pub fn note_stream_end(&self, end: StreamEnd) {
        if let Some(counter) = self.streams_ended.get(end.index()) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn streams_ended(&self, end: StreamEnd) -> u64 {
        self.streams_ended
            .get(end.index())
            .map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// Provider body bytes handed to the HTTP server across all streams. A count only.
    pub fn add_stream_bytes(&self, n: usize) {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        let _ = self
            .stream_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                Some(t.saturating_add(n))
            });
    }

    #[must_use]
    pub fn stream_bytes(&self) -> u64 {
        self.stream_bytes.load(Ordering::Relaxed)
    }

    /// Provider bytes now held by stream relays (gauge) and its peak. A relay adds on
    /// receiving a chunk and subtracts when the chunk has been handed on or discarded.
    pub fn stream_buffer_add(&self, n: usize) {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        let now = self
            .stream_buffered
            .fetch_add(n, Ordering::Relaxed)
            .saturating_add(n);
        self.stream_buffered_peak.fetch_max(now, Ordering::Relaxed);
    }

    pub fn stream_buffer_sub(&self, n: usize) {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        let _ = self
            .stream_buffered
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                Some(t.saturating_sub(n))
            });
    }

    #[must_use]
    pub fn stream_buffered(&self) -> u64 {
        self.stream_buffered.load(Ordering::Relaxed)
    }

    /// Highest total of provider bytes held by stream relays at one time.
    #[must_use]
    pub fn stream_buffered_peak(&self) -> u64 {
        self.stream_buffered_peak.load(Ordering::Relaxed)
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

#[cfg(test)]
mod stream_tests {
    use super::{Metrics, Stage, StreamEnd};
    use std::time::Duration;

    #[test]
    fn stream_counters_are_closed_bounded_and_label_free() {
        let m = Metrics::new();
        m.note_stream_started();
        m.note_stream_end(StreamEnd::Completed);
        m.note_stream_end(StreamEnd::IdleTimeout);
        m.note_stream_end(StreamEnd::IdleTimeout);
        assert_eq!(m.streams_started(), 1);
        assert_eq!(m.streams_ended(StreamEnd::Completed), 1);
        assert_eq!(m.streams_ended(StreamEnd::IdleTimeout), 2);
        assert_eq!(m.streams_ended(StreamEnd::Shutdown), 0);
        m.record(Stage::StreamTotal, Duration::from_micros(5));
        assert_eq!(m.stage(Stage::StreamTotal).count, 1);
        m.add_stream_bytes(10);
        assert_eq!(m.stream_bytes(), 10);
        m.stream_buffer_add(300);
        m.stream_buffer_add(200);
        m.stream_buffer_sub(300);
        assert_eq!(m.stream_buffered(), 200);
        assert_eq!(m.stream_buffered_peak(), 500);
        m.stream_buffer_sub(1_000);
        assert_eq!(m.stream_buffered(), 0, "saturates, never wraps");
        assert!(!format!("{m:?}").contains("data:"));
    }
}

#[cfg(test)]
#[path = "status_contract_tests.rs"]
mod status_contract_tests;
