//! The bounded provider response and its body owner (#20; ADR 0003, ADR 0017).
//!
//! A buffered JSON response is complete and size-checked before anything is committed to
//! the caller, so every failure up to that point is a Gateway-generated error with a real
//! status code. The body is relayed unredacted: provider responses and error bodies can
//! contain sensitive data, never enter Gateway logs, telemetry, or errors, and have no
//! `Debug` rendering here beyond their length.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Bytes;
use http_body::{Body, Frame, SizeHint};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;

use crate::admission::UpstreamPermit;

/// A complete, bounded provider response ready to relay. Holds the [`UpstreamPermit`]: the
/// buffered body is Gateway memory attributable to one upstream slot, so the slot is
/// released only when the response body has been written to the caller or abandoned.
pub struct UpstreamResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
    permit: UpstreamPermit,
}

impl UpstreamResponse {
    pub(super) const fn new(
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
        permit: UpstreamPermit,
    ) -> Self {
        Self {
            status,
            headers,
            body,
            permit,
        }
    }

    /// The provider's status code, relayed unchanged (including `4xx` and `5xx`).
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The allowlisted response headers ([`super::headers::relay_response_headers`]).
    #[must_use]
    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[must_use]
    pub const fn body_len(&self) -> usize {
        self.body.len()
    }

    /// Split into status, allowlisted headers, and a body that keeps the upstream permit
    /// until it is fully written or dropped.
    #[must_use]
    pub fn into_parts(self) -> (StatusCode, HeaderMap, HeldBody) {
        let body = HeldBody {
            data: Bytes::from(self.body),
            _permit: self.permit,
        };
        (self.status, self.headers, body)
    }
}

impl fmt::Debug for UpstreamResponse {
    /// Status and length only: provider bodies and headers are never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status.as_u16())
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// Largest frame the buffered body hands to the HTTP server at once. The server requests
/// the next frame only when its own write buffer has room, and drops the body as soon as
/// it has taken the last frame, so the permit is held until all but the last few frames
/// (a bounded window inside the server) have been accepted by the socket (#59, ADR 0026).
/// A single frame would let the server take the whole buffer in one call, drop the body
/// (and so return the permit) at once, and keep up to the response bound queued per
/// connection behind a slow reader with no permit accounting it.
pub(super) const FRAME_BYTES: usize = 16 * 1024;

/// A response body of bounded frames that owns the upstream permit. Dropping it (the
/// caller disconnected, the write finished, or the server shut down) releases the permit
/// and the buffer together.
pub struct HeldBody {
    data: Bytes,
    _permit: UpstreamPermit,
}

impl fmt::Debug for HeldBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldBody")
            .field("remaining", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl Body for HeldBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // `HeldBody` is `Unpin` (no self-references), so a plain mutable borrow is safe.
        let this = Pin::into_inner(self);
        if this.data.is_empty() {
            return Poll::Ready(None);
        }
        let take = this.data.len().min(FRAME_BYTES);
        Poll::Ready(Some(Ok(Frame::data(this.data.split_to(take)))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(u64::try_from(self.data.len()).unwrap_or(u64::MAX))
    }
}
