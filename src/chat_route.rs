//! HTTP admission for `POST /v1/chat/completions` (#18; ADR 0002, 0003, 0007).
//!
//! Order of work for one request, each step failing closed with a fixed safe code:
//!
//! 1. method, target, and header admission, before anything is reserved or read;
//! 2. receipt permit and conservative memory reservation via
//!    [`Admission::begin_body_receipt`], bounded wait, **before** the first body byte is
//!    collected (a declared `Content-Length` is only an upper bound that collection then
//!    enforces, never the allocation bound);
//! 3. bounded body collection under a body deadline;
//! 4. [`validate_with`]: one strict parse and the endpoint matrix, yielding a
//!    [`ValidatedRequest`] that keeps the reservation alive.
//!
//! After step 4 the request still ends in a local `not_implemented` rejection: the
//! forwarding path (#19 inspection, #20 transport) does not exist, so no byte of any
//! request is ever sent upstream. This module holds no HTTP client and never imports the
//! transport module.
//!
//! Every rejection closes the connection (`Connection: close`): a body that was not read
//! must not be parsed as the next request. Responses are fixed strings and contain no
//! request content. Status mappings and SDK retry implications are documented in
//! `docs/contracts/errors-and-telemetry.md`.

use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::admission::{Admission, AdmissionError, RequestLimits};
use crate::config::RouteId;
use crate::protocol::chat::ChatRequest;
use crate::protocol::{self, Protocol, ProtocolError, ValidatedRequest};
use crate::telemetry::SafeCode;

/// The one exact route served by this module.
pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// A fixed local rejection. Carries no request content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reject {
    /// Method other than `POST` on the exact route.
    Method,
    /// Query string, absolute-form target, or upgrade request.
    Target,
    /// `Content-Type` is not exactly one `application/json` (optional `charset=utf-8`).
    ContentType,
    /// `Content-Encoding` present, or a transfer coding other than `chunked`.
    Encoding,
    /// Malformed or conflicting framing headers.
    Framing,
    /// Body or declared length over the limit, or over the aggregate budget.
    TooLarge,
    /// Body not received within the body deadline.
    Deadline,
    /// Receipt/memory capacity not available within the admission wait.
    Overload,
    /// Parse failure, aborted or empty body.
    Malformed,
    /// Well-formed but outside the supported matrix.
    Unsupported,
    /// A parse budget or count limit was exceeded.
    LimitExceeded,
    /// Admitted and validated, but forwarding does not exist yet.
    NotImplemented,
}

impl Reject {
    /// HTTP status and safe code for the rejection.
    #[must_use]
    pub const fn status_and_code(self) -> (StatusCode, SafeCode) {
        match self {
            Self::Method => (StatusCode::METHOD_NOT_ALLOWED, SafeCode::UnsupportedInput),
            Self::Target => (StatusCode::BAD_REQUEST, SafeCode::UnsupportedInput),
            Self::ContentType | Self::Encoding => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                SafeCode::UnsupportedInput,
            ),
            Self::Framing | Self::Malformed => (StatusCode::BAD_REQUEST, SafeCode::MalformedInput),
            Self::TooLarge | Self::LimitExceeded => {
                (StatusCode::PAYLOAD_TOO_LARGE, SafeCode::LimitExceeded)
            }
            Self::Deadline => (StatusCode::REQUEST_TIMEOUT, SafeCode::LimitExceeded),
            Self::Overload => (StatusCode::SERVICE_UNAVAILABLE, SafeCode::Overload),
            Self::Unsupported => (StatusCode::UNPROCESSABLE_ENTITY, SafeCode::UnsupportedInput),
            Self::NotImplemented => (StatusCode::NOT_IMPLEMENTED, SafeCode::NotImplemented),
        }
    }
}

impl IntoResponse for Reject {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = format!(r#"{{"error":{{"code":"{}"}}}}"#, code.as_str());
        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
        match self {
            Self::Method => {
                headers.insert(header::ALLOW, HeaderValue::from_static("POST"));
            }
            Self::Overload => {
                headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            }
            _ => {}
        }
        response
    }
}

impl From<AdmissionError> for Reject {
    fn from(error: AdmissionError) -> Self {
        match error {
            AdmissionError::InvalidReservation => Self::TooLarge,
            _ => Self::Overload,
        }
    }
}

impl From<ProtocolError> for Reject {
    fn from(error: ProtocolError) -> Self {
        match error {
            ProtocolError::Malformed => Self::Malformed,
            ProtocolError::LimitExceeded => Self::LimitExceeded,
            _ => Self::Unsupported,
        }
    }
}

/// A validated request bound to the operator-defined route id it was admitted on. This
/// is what #19 passes (with a complete inspection) to `boundary::approve`. The route id
/// comes from the startup plan, never from the request.
pub struct Admitted {
    validated: ValidatedRequest,
    route: RouteId,
}

impl Admitted {
    #[must_use]
    pub const fn validated(&self) -> &ValidatedRequest {
        &self.validated
    }

    /// The typed request (shorthand for `validated().chat()`).
    #[must_use]
    pub const fn chat(&self) -> &ChatRequest {
        self.validated.chat()
    }

    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Split into the pieces `boundary::approve` consumes.
    #[must_use]
    pub fn into_parts(self) -> (ValidatedRequest, RouteId) {
        (self.validated, self.route)
    }
}

impl fmt::Debug for Admitted {
    /// Never prints request content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admitted")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

/// The Chat Completions admission route. Built once at startup from the validated plan;
/// holds no request state and no credentials.
#[derive(Debug)]
pub struct ChatRoute {
    admission: Arc<Admission>,
    limits: RequestLimits,
    max_body: usize,
    route: RouteId,
}

impl ChatRoute {
    /// Compose the per-request limits with the aggregate budget: the largest accepted
    /// body is clamped to what one reservation can ever cover. `route` is the approved
    /// route id this endpoint is bound to (from the startup plan).
    #[must_use]
    pub fn new(admission: Arc<Admission>, limits: RequestLimits, route: RouteId) -> Self {
        let max_body = limits.effective_max_body(admission.memory_total_units());
        Self {
            admission,
            limits,
            max_body,
            route,
        }
    }

    /// The approved route id admitted requests are bound to.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    #[must_use]
    pub fn admission(&self) -> &Admission {
        &self.admission
    }

    #[must_use]
    pub const fn limits(&self) -> &RequestLimits {
        &self.limits
    }

    /// Largest body accepted after composing the per-request limit with the aggregate
    /// memory budget. Zero means every request is rejected as too large.
    #[must_use]
    pub const fn effective_max_body(&self) -> usize {
        self.max_body
    }

    /// Run admission and validation for one request and answer it. Always returns a local
    /// response; nothing is forwarded.
    pub async fn handle(&self, request: Request) -> Response {
        match self.admit(request).await {
            // #19 inspects and #20 forwards the validated request. Until then it is
            // dropped here, which releases its permits and reservation.
            Ok(validated) => {
                drop(validated);
                Reject::NotImplemented.into_response()
            }
            Err(reject) => reject.into_response(),
        }
    }

    /// Admit, receive, and validate. The only success value is an [`Admitted`] request
    /// (a [`ValidatedRequest`] plus its route id).
    ///
    /// # Errors
    /// A [`Reject`] for every admission, receipt, parse, and limit failure.
    pub async fn admit(&self, request: Request) -> Result<Admitted, Reject> {
        let (parts, body) = request.into_parts();
        let declared = self.check_head(&parts)?;
        let cap = declared.map_or(self.max_body, |d| d.min(self.max_body));
        // Reserve before the first body byte is touched.
        // A reservation that can never fit (zero effective cap) is `TooLarge`, a busy
        // budget is `Overload`.
        let ticket = self.admission.begin_body_receipt(cap, &self.limits).await?;
        let bytes = tokio::time::timeout(
            self.limits.body_deadline(),
            collect(body, ticket.body_cap(), declared),
        )
        .await
        .map_err(|_| Reject::Deadline)??;
        let received = ticket.complete(bytes).map_err(|_| Reject::TooLarge)?;
        let validated =
            protocol::validate_with(received, Protocol::ChatCompletionsText, &self.limits)?;
        Ok(Admitted {
            validated,
            route: self.route.clone(),
        })
    }

    /// Method, target, and framing/content headers. Returns the declared body length
    /// when a single valid `Content-Length` was sent. Reserves and reads nothing.
    fn check_head(&self, parts: &Parts) -> Result<Option<usize>, Reject> {
        if parts.method != Method::POST {
            return Err(Reject::Method);
        }
        let uri = &parts.uri;
        if uri.query().is_some() || uri.authority().is_some() || uri.scheme().is_some() {
            return Err(Reject::Target);
        }
        let headers = &parts.headers;
        let upgrade = headers.contains_key(header::UPGRADE)
            || headers.get_all(header::CONNECTION).iter().any(|v| {
                v.to_str()
                    .map_or(true, |s| s.to_ascii_lowercase().contains("upgrade"))
            });
        if upgrade {
            return Err(Reject::Target);
        }
        if !content_type_is_json(headers) {
            return Err(Reject::ContentType);
        }
        if headers.contains_key(header::CONTENT_ENCODING) {
            return Err(Reject::Encoding);
        }
        let chunked = chunked_framing(headers)?;
        let declared = declared_length(headers)?;
        match (chunked, declared) {
            (true, Some(_)) => Err(Reject::Framing),
            (true, None) => Ok(None),
            (false, Some(0) | None) => Err(Reject::Malformed),
            (false, Some(n)) => {
                if n > self.max_body {
                    Err(Reject::TooLarge)
                } else {
                    Ok(Some(n))
                }
            }
        }
    }
}

/// Exactly one `Content-Type`: `application/json`, optionally with `charset=utf-8`.
fn content_type_is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    let Ok(text) = value.to_str() else {
        return false;
    };
    let mut segments = text.split(';');
    if !segments
        .next()
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    segments.all(|param| {
        param.split_once('=').is_some_and(|(key, value)| {
            key.trim().eq_ignore_ascii_case("charset")
                && value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
        })
    })
}

/// `Ok(true)` for exactly `Transfer-Encoding: chunked`, `Ok(false)` when absent. Any
/// other transfer coding (for example `gzip, chunked`) is unsupported compression.
fn chunked_framing(headers: &HeaderMap) -> Result<bool, Reject> {
    let mut values = headers.get_all(header::TRANSFER_ENCODING).iter();
    match (values.next(), values.next()) {
        (None, _) => Ok(false),
        (Some(value), None) if value.as_bytes().eq_ignore_ascii_case(b"chunked") => Ok(true),
        _ => Err(Reject::Encoding),
    }
}

/// A single all-digit `Content-Length`, or `None` when absent.
fn declared_length(headers: &HeaderMap) -> Result<Option<usize>, Reject> {
    let mut values = headers.get_all(header::CONTENT_LENGTH).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return if headers.contains_key(header::CONTENT_LENGTH) {
            Err(Reject::Framing)
        } else {
            Ok(None)
        };
    };
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(Reject::Framing);
    }
    // All digits, so a parse failure can only be overflow: far above any limit.
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(Some)
        .ok_or(Reject::TooLarge)
}

/// Collect the body up to `cap` bytes. Reservation was made before this is first polled.
async fn collect(mut body: Body, cap: usize, declared: Option<usize>) -> Result<Vec<u8>, Reject> {
    let mut buffer = Vec::with_capacity(declared.unwrap_or(0).min(cap));
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let frame = frame.map_err(|_| Reject::Malformed)?;
        // Trailers are ignored and never forwarded.
        if let Ok(data) = frame.into_data() {
            if buffer.len().saturating_add(data.len()) > cap {
                return Err(Reject::TooLarge);
            }
            buffer.extend_from_slice(&data);
        }
    }
    if declared.is_some_and(|d| d != buffer.len()) {
        return Err(Reject::Malformed);
    }
    Ok(buffer)
}

/// Add the exact `POST /v1/chat/completions` route to `router`. Every method is routed
/// to the handler so a wrong method gets the documented local `405`, never a silent
/// fallthrough.
pub fn mount(router: Router, route: Arc<ChatRoute>) -> Router {
    router.route(
        CHAT_COMPLETIONS_PATH,
        any(move |request: Request| {
            let route = Arc::clone(&route);
            async move { route.handle(request).await }
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::admission::CapacityPlan;

    fn route(memory_units: u32) -> ChatRoute {
        let one = NonZeroU32::MIN;
        let units = NonZeroU32::new(memory_units).expect("nonzero");
        let admission = Arc::new(Admission::new(&CapacityPlan::new(
            NonZeroU32::new(4).expect("nonzero"),
            units,
            one,
            one,
            one,
        )));
        ChatRoute::new(
            admission,
            RequestLimits::provisional(),
            RouteId::new("synthetic-route"),
        )
    }

    #[test]
    fn per_request_limit_composes_with_the_aggregate_budget() {
        let limits = RequestLimits::provisional();
        let full = limits.reservation_units(1_048_576);
        // A budget that fits the maximum request leaves the per-request limit alone.
        assert_eq!(route(full).effective_max_body(), 1_048_576);
        assert_eq!(
            route(full.saturating_add(1000)).effective_max_body(),
            1_048_576
        );
        // A smaller budget clamps the body so one reservation always fits.
        let half = route(full / 2);
        let cap = half.effective_max_body();
        assert!(cap > 0 && cap < 1_048_576);
        assert!(limits.reservation_units(cap) <= full / 2);
        assert!(limits.reservation_units(cap.saturating_add(1)) > full / 2);
        // One unit (1 KiB) only covers a handful of bytes; no real request fits.
        assert!(route(1).effective_max_body() < 16);
    }

    #[test]
    fn status_mapping_is_fixed_and_safe() {
        for reject in [
            Reject::Method,
            Reject::Target,
            Reject::ContentType,
            Reject::Encoding,
            Reject::Framing,
            Reject::TooLarge,
            Reject::Deadline,
            Reject::Overload,
            Reject::Malformed,
            Reject::Unsupported,
            Reject::LimitExceeded,
            Reject::NotImplemented,
        ] {
            let (status, code) = reject.status_and_code();
            assert!(status.is_client_error() || status.is_server_error());
            assert!(!code.as_str().is_empty());
        }
    }
}
