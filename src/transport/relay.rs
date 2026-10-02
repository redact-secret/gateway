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
            data: Some(Bytes::from(self.body)),
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

/// A single-frame response body that owns the upstream permit. Dropping it (the caller
/// disconnected, the write finished, or the server shut down) releases the permit and the
/// buffer together.
pub struct HeldBody {
    data: Option<Bytes>,
    _permit: UpstreamPermit,
}

impl fmt::Debug for HeldBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldBody")
            .field("remaining", &self.data.as_ref().map_or(0, Bytes::len))
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
        Poll::Ready(
            this.data
                .take()
                .filter(|d| !d.is_empty())
                .map(|d| Ok(Frame::data(d))),
        )
    }

    fn is_end_stream(&self) -> bool {
        self.data.as_ref().is_none_or(Bytes::is_empty)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(
            self.data
                .as_ref()
                .map_or(0, |d| u64::try_from(d.len()).unwrap_or(u64::MAX)),
        )
    }
}
