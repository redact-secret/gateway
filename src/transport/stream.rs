//! The SSE relay body (#21; ADR 0018).
//!
//! A provider answer with `Content-Type: text/event-stream` is relayed to the caller as it
//! arrives. Rules:
//!
//! * **Bytes, not events.** The body forwards each provider body chunk unchanged. It never
//!   parses, splits, merges, reorders, redacts, or fabricates events or UTF-8 sequences, so
//!   a TCP or HTTP chunk that ends inside an event, inside a multibyte character, or after
//!   several events is relayed exactly as received. Nothing in this module reads event
//!   text; the only things it looks at are chunk lengths and timing.
//! * **No tasks, no queues.** [`StreamBody`] is polled by the HTTP server that owns the
//!   response; it polls the provider body inline. The server asks for the next chunk only
//!   when it can accept one, so a slow consumer backpressures the provider through TCP and
//!   the relay holds at most one provider chunk. Nothing is spawned.
//! * **Bounded by permits.** The body owns the [`UpstreamPermit`] and the
//!   [`StreamPermit`] from the response headers until the stream ends, fails, or the body
//!   is dropped, and drops the provider connection first, so capacity returns only after
//!   the exchange is closed. A provider chunk larger than the configured buffer bound
//!   terminates the stream.
//! * **Deadlines.** An idle deadline (counted only while waiting on the provider), a total
//!   lifetime deadline, and a shutdown signal. The write side is bounded separately by the
//!   write-stall deadline of the connection ([`crate::write_stall`]).
//! * **Termination contract.** Once the headers are committed no status can change. Every
//!   failure is reported to the HTTP server as a body error, which closes the connection
//!   without the normal end of the message: the caller sees a truncated stream. A normal
//!   end (and so a terminating chunk) is produced only when the provider itself ended the
//!   response cleanly. No completion event (`data: [DONE]` or otherwise) is ever added.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Bytes;
use http_body::{Body, Frame};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use tokio::sync::watch;
use tokio::time::{Instant, Sleep, sleep_until};

use crate::admission::{RequestLimits, StreamPermit, UpstreamPermit};
use crate::telemetry::{Metrics, SafeCode, Stage, StreamEnd};

/// Why a stream was cut after its headers were committed. Fixed, content-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamError {
    /// No provider chunk arrived within the idle deadline.
    IdleTimeout,
    /// The stream outlived its total lifetime deadline.
    LifetimeExceeded,
    /// The provider connection failed or sent a malformed or truncated response.
    Upstream,
    /// One provider chunk exceeded the relay buffer bound.
    BufferExceeded,
    /// The Gateway is shutting down and its drain deadline passed.
    Shutdown,
}

impl StreamError {
    /// The safe code this ending is recorded under. It is never sent to the caller (the
    /// headers are committed): the caller sees a truncated stream.
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::IdleTimeout | Self::LifetimeExceeded => SafeCode::UpstreamTimeout,
            Self::Upstream => SafeCode::UpstreamInvalidResponse,
            Self::BufferExceeded => SafeCode::UpstreamResponseTooLarge,
            Self::Shutdown => SafeCode::NotReady,
        }
    }

    const fn end(self) -> StreamEnd {
        match self {
            Self::IdleTimeout => StreamEnd::IdleTimeout,
            Self::LifetimeExceeded => StreamEnd::LifetimeExceeded,
            Self::Upstream => StreamEnd::UpstreamError,
            Self::BufferExceeded => StreamEnd::BufferExceeded,
            Self::Shutdown => StreamEnd::Shutdown,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for StreamError {}

/// The deadlines and bound a stream runs under, copied from the validated limits.
#[derive(Clone, Copy, Debug)]
pub(super) struct StreamLimits {
    idle: Duration,
    lifetime: Duration,
    buffer: usize,
}

impl StreamLimits {
    pub(super) fn from_limits(limits: &RequestLimits) -> Self {
        Self {
            idle: limits.stream_idle(),
            lifetime: limits.stream_lifetime(),
            buffer: usize::try_from(limits.stream_buffer_bytes).unwrap_or(usize::MAX),
        }
    }
}

/// A provider SSE response whose headers were received and vetted. The body has not been
/// read. Holds both permits; they belong to the body once it is split off.
pub struct StreamResponse {
    status: StatusCode,
    headers: HeaderMap,
    inner: Pin<Box<reqwest::Body>>,
    upstream: UpstreamPermit,
    stream: StreamPermit,
    limits: StreamLimits,
    started: Instant,
    metrics: Option<Arc<Metrics>>,
}

impl StreamResponse {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        status: StatusCode,
        headers: HeaderMap,
        inner: reqwest::Body,
        upstream: UpstreamPermit,
        stream: StreamPermit,
        limits: StreamLimits,
        started: Instant,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            status,
            headers,
            inner: Box::pin(inner),
            upstream,
            stream,
            limits,
            started,
            metrics,
        }
    }

    /// The provider's status code (always a success status for a relayed stream).
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The allowlisted response headers ([`super::headers::relay_response_headers`]).
    #[must_use]
    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Split into status, allowlisted headers, and the relay body. The body ends the
    /// stream with an error when `cancel` flips to `true` (shutdown after the drain
    /// deadline). Dropping the body closes the provider connection and returns both
    /// permits.
    #[must_use]
    pub fn into_parts(self, cancel: watch::Receiver<bool>) -> (StatusCode, HeaderMap, StreamBody) {
        let now = Instant::now();
        let lifetime_deadline = self
            .started
            .checked_add(self.limits.lifetime)
            .unwrap_or(now);
        let idle_deadline = now.checked_add(self.limits.idle).unwrap_or(now);
        if let Some(m) = &self.metrics {
            m.note_stream_started();
        }
        let body = StreamBody {
            inner: Some(self.inner),
            cancel: Box::pin(cancelled(cancel)),
            idle: Box::pin(sleep_until(idle_deadline)),
            lifetime: Box::pin(sleep_until(lifetime_deadline)),
            permits: Some((self.stream, self.upstream)),
            limits: self.limits,
            metrics: self.metrics,
            started: self.started,
            waiting: false,
            wait_started: now,
            handed_at: None,
            in_flight: 0,
            first_byte: false,
            upstream_wait: Duration::ZERO,
            downstream_wait: Duration::ZERO,
            finished: false,
        };
        (self.status, self.headers, body)
    }
}

impl fmt::Debug for StreamResponse {
    /// Status only: provider bodies and headers are never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamResponse")
            .field("status", &self.status.as_u16())
            .finish_non_exhaustive()
    }
}

/// Resolves when shutdown cancellation is signalled; never if the sender is gone.
async fn cancelled(mut rx: watch::Receiver<bool>) {
    if rx.wait_for(|c| *c).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The relayed response body. See the module documentation for the contract. Field order
/// is drop order: the provider connection closes before the permits return.
pub struct StreamBody {
    inner: Option<Pin<Box<reqwest::Body>>>,
    cancel: Pin<Box<dyn Future<Output = ()> + Send>>,
    idle: Pin<Box<Sleep>>,
    lifetime: Pin<Box<Sleep>>,
    permits: Option<(StreamPermit, UpstreamPermit)>,
    limits: StreamLimits,
    metrics: Option<Arc<Metrics>>,
    started: Instant,
    /// Whether the relay is currently waiting on the provider (idle clock running).
    waiting: bool,
    wait_started: Instant,
    /// When the last chunk was handed to the server, until it asks for the next one.
    handed_at: Option<Instant>,
    /// Bytes of the chunk handed to the server and not yet superseded (buffer gauge).
    in_flight: usize,
    first_byte: bool,
    upstream_wait: Duration,
    downstream_wait: Duration,
    finished: bool,
}

impl fmt::Debug for StreamBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamBody")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

/// What one poll of the provider body produced, computed without borrowing the whole body.
enum Step {
    Chunk(Bytes),
    Skip,
    End,
    Failed,
    Pending,
}

impl StreamBody {
    fn release_in_flight(&mut self) {
        if self.in_flight > 0 {
            if let Some(m) = &self.metrics {
                m.stream_buffer_sub(self.in_flight);
            }
            self.in_flight = 0;
        }
    }

    /// End the stream exactly once: close the provider connection, then return the
    /// permits, then record bounded timings and counters.
    fn finish(&mut self, end: StreamEnd) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.inner = None;
        // A chunk handed to the server that it never came back from (a stalled or gone
        // consumer) is consumer wait too.
        if let Some(handed) = self.handed_at.take() {
            self.downstream_wait = self.downstream_wait.saturating_add(handed.elapsed());
        }
        self.release_in_flight();
        self.permits = None;
        if let Some(m) = &self.metrics {
            m.note_stream_end(end);
            m.record(Stage::StreamTotal, self.started.elapsed());
            m.record(Stage::StreamUpstreamWait, self.upstream_wait);
            m.record(Stage::StreamDownstreamWait, self.downstream_wait);
        }
    }

    fn fail(&mut self, error: StreamError) -> Poll<Option<Result<Frame<Bytes>, StreamError>>> {
        self.finish(error.end());
        Poll::Ready(Some(Err(error)))
    }
}

impl Body for StreamBody {
    type Data = Bytes;
    type Error = StreamError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Every field is `Unpin` (boxed or plain), so a plain mutable borrow is safe.
        let this = Pin::into_inner(self);
        if this.finished {
            return Poll::Ready(None);
        }
        let now = Instant::now();
        // The server is asking for more: it has taken the previous chunk.
        if let Some(handed) = this.handed_at.take() {
            this.downstream_wait = this
                .downstream_wait
                .saturating_add(now.saturating_duration_since(handed));
        }
        this.release_in_flight();

        // Both are polled on every call so their wakers are registered.
        if this.cancel.as_mut().poll(cx).is_ready() {
            return this.fail(StreamError::Shutdown);
        }
        if this.lifetime.as_mut().poll(cx).is_ready() {
            return this.fail(StreamError::LifetimeExceeded);
        }
        if !this.waiting {
            this.waiting = true;
            this.wait_started = now;
            // The idle clock runs only while the relay waits on the provider.
            let deadline = now.checked_add(this.limits.idle).unwrap_or(now);
            this.idle.as_mut().reset(deadline);
        }
        loop {
            let step = match this.inner.as_mut() {
                None => return Poll::Ready(None),
                Some(inner) => match inner.as_mut().poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                        Ok(data) if data.is_empty() => Step::Skip,
                        Ok(data) => Step::Chunk(data),
                        // Trailers are not part of an event stream and never relayed.
                        Err(_) => Step::Skip,
                    },
                    Poll::Ready(Some(Err(_))) => Step::Failed,
                    Poll::Ready(None) => Step::End,
                    Poll::Pending => Step::Pending,
                },
            };
            match step {
                Step::Skip => {}
                Step::Chunk(data) => {
                    let received = Instant::now();
                    this.waiting = false;
                    this.upstream_wait = this
                        .upstream_wait
                        .saturating_add(received.saturating_duration_since(this.wait_started));
                    if !this.first_byte {
                        this.first_byte = true;
                        if let Some(m) = &this.metrics {
                            m.record(Stage::StreamFirstByte, this.started.elapsed());
                        }
                    }
                    if data.len() > this.limits.buffer {
                        return this.fail(StreamError::BufferExceeded);
                    }
                    this.in_flight = data.len();
                    this.handed_at = Some(received);
                    if let Some(m) = &this.metrics {
                        m.stream_buffer_add(data.len());
                        m.add_stream_bytes(data.len());
                    }
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Step::End => {
                    this.waiting = false;
                    this.upstream_wait = this.upstream_wait.saturating_add(
                        Instant::now().saturating_duration_since(this.wait_started),
                    );
                    this.finish(StreamEnd::Completed);
                    return Poll::Ready(None);
                }
                Step::Failed => {
                    this.waiting = false;
                    return this.fail(StreamError::Upstream);
                }
                Step::Pending => {
                    if this.idle.as_mut().poll(cx).is_ready() {
                        this.waiting = false;
                        return this.fail(StreamError::IdleTimeout);
                    }
                    return Poll::Pending;
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }
}

impl Drop for StreamBody {
    /// A body dropped before its stream ended (caller disconnect, write stall, server
    /// teardown) closes the provider connection and returns both permits.
    fn drop(&mut self) {
        self.finish(StreamEnd::Abandoned);
    }
}
