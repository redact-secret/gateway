//! HTTP admission for `POST /v1/chat/completions` (#18; ADR 0002, 0003, 0007) and, with the
//! same pipeline bound to another protocol, `POST /v1/responses` (#86).
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
//! 5. when an [`Inspection`] service is attached (production wiring), the request is
//!    inspected through the pinned core and approved into a `SanitizedRequest`
//!    ([`Inspection::inspect_and_approve`], #19); any failure is a fixed local rejection.
//!
//! 6. an approved request is forwarded (#20, ADR 0017) through the central transport only:
//!    after inspection has completed and released its permit, an [`UpstreamPermit`] is
//!    acquired independently (`try`, so a full upstream class is an immediate `overload`),
//!    and [`Upstream::forward`] sends the sealed body once. The provider's bounded JSON
//!    response (any status) is relayed with allowlisted headers and an unredacted body;
//!    every Gateway-side failure is a fixed safe code ([`Reject::Transport`]).
//!
//! `stream: true` (#21, ADR 0018) takes exactly the same road: the whole request is
//! received, admitted, and inspected first, and any rejection sends zero upstream body
//! bytes. After inspection has released its permit, the upstream permit and then an
//! independent stream permit are acquired (`try`, so a full class is an immediate
//! `overload`), and [`Upstream::forward_stream`] relays the provider's SSE bytes as they
//! arrive. The response headers are committed when the provider's headers arrive; from
//! then on failures cannot change the status and end the stream abruptly (see
//! [`crate::transport::stream`]). A route with no configured upstream ends in `501
//! not_implemented`. This module holds no HTTP client and builds no request; it hands the
//! sealed request, the request-local credential carried on [`Admitted`] (#24), and the
//! permits to the transport.
//!
//! The handler future owns the whole request until the response headers. When the caller disconnects, hyper drops it,
//! which cancels any pending wait, inspection await, or upstream exchange and releases
//! every permit and buffer through RAII. Nothing is spawned here. On shutdown,
//! [`ChatRoute::cancel_in_flight`] ends the same futures after the drain deadline and
//! also ends every response stream already past its headers, which owns its permits and
//! the provider connection and closes them when it is dropped.
//!
//! Every rejection closes the connection (`Connection: close`): a body that was not read
//! must not be parsed as the next request. Responses are fixed strings and contain no
//! request content. Status mappings and SDK retry implications are documented in
//! `docs/contracts/errors-and-telemetry.md`.

use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Version, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use tokio::sync::watch;

use crate::admission::{Admission, AdmissionError, RequestLimits};
use crate::boundary::{BoundaryError, Inspection, ProtocolRoute};
use crate::config::RouteId;
use crate::core_bridge::CoreBridgeError;
use crate::protocol::chat::ChatRequest;
use crate::protocol::{self, Protocol, ProtocolError, ValidatedRequest};
use crate::telemetry::{Metrics, SafeCode, Stage};
use crate::transport::headers::{HeaderReject, VettedHeaders, vet_inbound};
use crate::transport::local_auth::{AuthReject, LocalAuth};
use crate::transport::{Forwarded, StreamResponse, TransportError, Upstream, UpstreamResponse};

/// The exact Chat Completions path.
pub const CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";

/// The exact Responses path (#86). Reviewed and fixed; the caller never selects it.
pub const RESPONSES_PATH: &str = "/v1/responses";

/// The one exact inbound path for a protocol.
#[must_use]
pub const fn path_for(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::ChatCompletionsText => CHAT_COMPLETIONS_PATH,
        Protocol::ResponsesText => RESPONSES_PATH,
    }
}

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
    /// Not served by this build or deployment: no upstream is configured for the route.
    NotImplemented,
    /// A Gateway-side forwarding failure (#20). The provider's own error responses are
    /// relayed as responses and never take this path.
    Transport(TransportError),
    /// The Gateway is shutting down and its drain deadline passed; in-flight work was
    /// cancelled.
    ShuttingDown,
    /// Inspection or approval failed (fail closed; see [`BoundaryError`]).
    Inspection(BoundaryError),
    /// No usable provider `Authorization` header (#24).
    MissingCredential,
    /// Local caller token (`X-Gateway-Local-Token`) absent or removed by `Connection` (#63).
    LocalAuthRequired,
    /// Local caller token duplicate, malformed, out of bounds, or wrong (#63).
    LocalAuthInvalid,
    /// Duplicate or malformed `Authorization`, organization/project, or `Connection` (#24).
    Header,
    /// Request headers over the byte limits (#24).
    HeaderTooLarge,
    /// `Expect` other than `100-continue` (#24).
    Expectation,
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
            Self::ShuttingDown => (StatusCode::SERVICE_UNAVAILABLE, SafeCode::NotReady),
            Self::Transport(error) => match error {
                TransportError::Timeout => (StatusCode::GATEWAY_TIMEOUT, SafeCode::UpstreamTimeout),
                TransportError::UnknownRoute => {
                    (StatusCode::NOT_IMPLEMENTED, SafeCode::NotImplemented)
                }
                other => (StatusCode::BAD_GATEWAY, other.code()),
            },
            Self::Inspection(error) => match error.code() {
                SafeCode::UnsupportedInput => {
                    (StatusCode::UNPROCESSABLE_ENTITY, SafeCode::UnsupportedInput)
                }
                SafeCode::LimitExceeded => (StatusCode::PAYLOAD_TOO_LARGE, SafeCode::LimitExceeded),
                SafeCode::Overload => (StatusCode::SERVICE_UNAVAILABLE, SafeCode::Overload),
                code => (StatusCode::INTERNAL_SERVER_ERROR, code),
            },
            Self::MissingCredential => (StatusCode::UNAUTHORIZED, SafeCode::MissingCredential),
            Self::LocalAuthRequired => (StatusCode::UNAUTHORIZED, SafeCode::LocalAuthRequired),
            Self::LocalAuthInvalid => (StatusCode::UNAUTHORIZED, SafeCode::LocalAuthInvalid),
            Self::Header => (StatusCode::BAD_REQUEST, SafeCode::MalformedInput),
            Self::HeaderTooLarge => (
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
                SafeCode::LimitExceeded,
            ),
            Self::Expectation => (StatusCode::EXPECTATION_FAILED, SafeCode::UnsupportedInput),
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
            Self::Overload | Self::Inspection(BoundaryError::Core(CoreBridgeError::Overload)) => {
                headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            }
            Self::MissingCredential => {
                headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            _ => {}
        }
        response
    }
}

impl From<AuthReject> for Reject {
    fn from(error: AuthReject) -> Self {
        match error {
            AuthReject::Required => Self::LocalAuthRequired,
            AuthReject::Invalid => Self::LocalAuthInvalid,
        }
    }
}

impl From<HeaderReject> for Reject {
    fn from(error: HeaderReject) -> Self {
        match error {
            HeaderReject::MissingCredential => Self::MissingCredential,
            HeaderReject::TooLarge => Self::HeaderTooLarge,
            HeaderReject::Expectation => Self::Expectation,
            _ => Self::Header,
        }
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
    /// Request-local provider credential and reviewed metadata (#24). Taken by #20.
    headers: Option<VettedHeaders>,
    /// The request used HTTP/1.0 or older. A streamed response to such a caller would be
    /// close-delimited, so a cut stream would look like a finished one (#21).
    legacy_http: bool,
}

impl Admitted {
    /// Take the request-local vetted headers (provider credential, organization/project)
    /// for outbound wire construction. Returns `None` the second time.
    pub const fn take_headers(&mut self) -> Option<VettedHeaders> {
        self.headers.take()
    }

    #[must_use]
    pub const fn validated(&self) -> &ValidatedRequest {
        &self.validated
    }

    /// The typed request (shorthand for `validated().chat()`).
    #[must_use]
    pub const fn chat(&self) -> Option<&ChatRequest> {
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

/// One inbound endpoint: the shared bounded admission, inspection, forwarding and relay
/// pipeline bound to exactly one [`Protocol`] and one operator route id (#86). Chat
/// Completions and Responses are two instances of this one type, never two pipelines.
/// Built once at startup from the validated plan; holds no request state and no
/// credentials.
#[derive(Debug)]
pub struct EndpointRoute {
    protocol: Protocol,
    admission: Arc<Admission>,
    limits: RequestLimits,
    max_body: usize,
    route: RouteId,
    inspection: Option<Arc<Inspection>>,
    upstream: Option<Arc<Upstream>>,
    metrics: Arc<Metrics>,
    /// Local caller authentication, the first decision for a `POST` (#63).
    local_auth: LocalAuth,
    /// Flipped once by [`Self::cancel_in_flight`]; every request future selects on it.
    cancel: watch::Sender<bool>,
}

/// The Chat Completions route (`POST /v1/chat/completions`).
pub type ChatRoute = EndpointRoute;

/// The Responses route (`POST /v1/responses`, #86): the same type, bound to
/// [`Protocol::ResponsesText`].
pub type ResponsesRoute = EndpointRoute;

impl EndpointRoute {
    /// A Chat Completions endpoint. Compose the per-request limits with the aggregate
    /// budget: the largest accepted body is clamped to what one reservation can ever
    /// cover. `route` is the approved route id this endpoint is bound to (from the startup
    /// plan).
    #[must_use]
    pub fn new(admission: Arc<Admission>, limits: RequestLimits, route: RouteId) -> Self {
        Self::for_protocol(Protocol::ChatCompletionsText, admission, limits, route)
    }

    /// A Responses endpoint (#86), otherwise identical to [`Self::new`].
    #[must_use]
    pub fn responses(admission: Arc<Admission>, limits: RequestLimits, route: RouteId) -> Self {
        Self::for_protocol(Protocol::ResponsesText, admission, limits, route)
    }

    /// An endpoint for `protocol`. The protocol is fixed here, at startup, and decides the
    /// exact path, the request matrix, and the [`ProtocolRoute`] built for approval.
    #[must_use]
    pub fn for_protocol(
        protocol: Protocol,
        admission: Arc<Admission>,
        limits: RequestLimits,
        route: RouteId,
    ) -> Self {
        let max_body = limits.effective_max_body(admission.memory_total_units());
        Self {
            protocol,
            admission,
            limits,
            max_body,
            route,
            inspection: None,
            upstream: None,
            metrics: Arc::new(Metrics::new()),
            local_auth: LocalAuth::Disabled,
            cancel: watch::channel(false).0,
        }
    }

    /// Attach the startup-built inspection service. Without it an admitted request ends in
    /// `not_implemented` without inspection (it still reaches nothing).
    #[must_use]
    pub fn with_inspection(mut self, inspection: Arc<Inspection>) -> Self {
        self.inspection = Some(inspection);
        self
    }

    /// Attach the startup-built transport. Without it (or when it has no route for this
    /// endpoint) an approved request ends in `not_implemented` and nothing is sent.
    #[must_use]
    pub fn with_upstream(mut self, upstream: Arc<Upstream>) -> Self {
        self.upstream = Some(upstream);
        self
    }

    /// Share the stage-timing counters with the other startup-built services.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    pub(crate) fn shared_metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }

    /// Attach the startup-built local caller authentication (#63). Without it the route is
    /// the Alpha behavior (`Disabled`), which the plan allows on a loopback listener only.
    #[must_use]
    pub fn with_local_auth(mut self, local_auth: LocalAuth) -> Self {
        self.local_auth = local_auth;
        self
    }

    #[must_use]
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Cancel every in-flight request future (shutdown after the drain deadline). Each one
    /// answers a local `503 not_ready` if it can, and releases its permits and buffers when
    /// dropped. Bytes already written to the provider cannot be retracted.
    pub fn cancel_in_flight(&self) {
        self.cancel.send_replace(true);
    }

    /// The protocol this endpoint serves.
    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The one exact inbound path.
    #[must_use]
    pub const fn path(&self) -> &'static str {
        path_for(self.protocol)
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

    /// Run the whole request and answer it: admission, validation, inspection, forwarding,
    /// and relay. The returned future owns every permit and buffer the request holds;
    /// dropping it (caller disconnect) or [`Self::cancel_in_flight`] ends them all.
    pub async fn handle(&self, request: Request) -> Response {
        // Local caller authentication is the first decision (#63, ADR 0030): before the
        // head checks below, any `100 Continue`, reservation, body read, or upstream
        // contact. The body is never polled on this path.
        if let Err(rejected) = self.local_auth.screen(request.method(), request.headers()) {
            self.metrics.record_local_auth_rejection(rejected.code());
            return Reject::from(rejected).into_response();
        }
        let mut cancel = self.cancel.subscribe();
        let work = async {
            match self.admit(request).await {
                Ok(admitted) => self.process(admitted).await,
                Err(reject) => reject.into_response(),
            }
        };
        tokio::select! {
            biased;
            () = async {
                if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => Reject::ShuttingDown.into_response(),
            response = work => response,
        }
    }

    /// Inspect, approve, forward, and relay an admitted request. Every Gateway-side
    /// failure is a fixed safe rejection; a provider answer is relayed as received.
    async fn process(&self, mut admitted: Admitted) -> Response {
        let streaming = admitted.validated().body().stream() == Some(true);
        // A streamed response to an HTTP/1.0 caller would end with the connection, so an
        // interrupted stream could not be told from a finished one: not served.
        if streaming && admitted.legacy_http {
            return Reject::Unsupported.into_response();
        }
        let Some(inspection) = &self.inspection else {
            return Reject::NotImplemented.into_response();
        };
        let headers = admitted.take_headers();
        let (validated, route) = admitted.into_parts();
        let started = Instant::now();
        let route = ProtocolRoute::new(self.protocol, route);
        let sanitized = match inspection.inspect_and_approve(validated, route).await {
            Ok(sanitized) => sanitized,
            Err(error) => return Reject::Inspection(error).into_response(),
        };
        self.metrics.record(Stage::Inspection, started.elapsed());
        // Inspection capacity is already released: the worker job ended before the result
        // reached this await. Upstream capacity is a separate class, acquired now.
        let Some(upstream) = &self.upstream else {
            return Reject::NotImplemented.into_response();
        };
        if upstream.destination(sanitized.route()).is_err() {
            return Reject::NotImplemented.into_response();
        }
        let Some(headers) = headers else {
            return Reject::Header.into_response();
        };
        let permit = match self.admission.try_upstream() {
            Ok(permit) => permit,
            Err(_) => return Reject::Overload.into_response(),
        };
        if !streaming {
            return match upstream.forward(sanitized, headers, permit).await {
                Ok(response) => relay(response),
                Err(error) => Reject::Transport(error).into_response(),
            };
        }
        // Order (ADR 0003): receipt, memory, inspection, upstream, then stream. Inspection
        // finished above and released its permit; the stream permit is independent of it.
        let stream_permit = match self.admission.try_stream() {
            Ok(permit) => permit,
            Err(_) => return Reject::Overload.into_response(),
        };
        match upstream
            .forward_stream(sanitized, headers, permit, stream_permit)
            .await
        {
            Ok(Forwarded::Stream(stream)) => relay_stream(stream, self.cancel.subscribe()),
            Ok(Forwarded::Buffered(response)) => relay(response),
            Err(error) => Reject::Transport(error).into_response(),
        }
    }

    /// Admit, receive, and validate. The only success value is an [`Admitted`] request
    /// (a [`ValidatedRequest`] plus its route id).
    ///
    /// # Errors
    /// A [`Reject`] for every admission, receipt, parse, and limit failure.
    pub async fn admit(&self, request: Request) -> Result<Admitted, Reject> {
        let (parts, body) = request.into_parts();
        let legacy_http = parts.version < Version::HTTP_11;
        let declared = self.check_head(&parts)?;
        let headers = vet_inbound(&parts.headers)?;
        let cap = declared.map_or(self.max_body, |d| d.min(self.max_body));
        // Reserve before the first body byte is touched.
        // A reservation that can never fit (zero effective cap) is `TooLarge`, a busy
        // budget is `Overload`.
        let waited = Instant::now();
        let ticket = self.admission.begin_body_receipt(cap, &self.limits).await?;
        self.metrics.record(Stage::AdmissionWait, waited.elapsed());
        let bytes = tokio::time::timeout(
            self.limits.body_deadline(),
            collect(body, ticket.body_cap(), declared),
        )
        .await
        .map_err(|_| Reject::Deadline)??;
        let received = ticket.complete(bytes).map_err(|_| Reject::TooLarge)?;
        let parsed = Instant::now();
        let validated = protocol::validate_with(received, self.protocol, &self.limits)?;
        self.metrics.record(Stage::Parse, parsed.elapsed());
        Ok(Admitted {
            validated,
            route: self.route.clone(),
            headers: Some(headers),
            legacy_http,
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

/// The provider's response as the caller's response: status and allowlisted headers as
/// received, the body unredacted. The framing headers are regenerated by the HTTP server
/// from the exact body length. The body keeps the upstream permit until it is written or
/// abandoned.
fn relay(response: UpstreamResponse) -> Response {
    let (status, headers, body) = response.into_parts();
    let mut out = Response::new(Body::new(body));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// The provider's event stream as the caller's response: status and allowlisted headers
/// as received, the body relayed incrementally and unredacted. The server frames it (no
/// length; chunked). The body owns both permits and the provider connection, and ends the
/// stream abruptly on any failure after this point.
fn relay_stream(response: StreamResponse, cancel: watch::Receiver<bool>) -> Response {
    let (status, headers, body) = response.into_parts(cancel);
    let mut out = Response::new(Body::new(body));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
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

/// Add the endpoint's exact route (`POST /v1/chat/completions` or `POST /v1/responses`) to
/// `router`. Every method is routed to the handler so a wrong method gets the documented
/// local `405`, never a silent fallthrough.
pub fn mount(router: Router, route: Arc<EndpointRoute>) -> Router {
    let path = route.path();
    router.route(
        path,
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
            Reject::ShuttingDown,
            Reject::Transport(TransportError::Timeout),
            Reject::Transport(TransportError::Connect),
            Reject::Transport(TransportError::Tls),
            Reject::Transport(TransportError::InvalidResponse),
            Reject::Transport(TransportError::ResponseTooLarge),
            Reject::Transport(TransportError::UnknownRoute),
            Reject::Inspection(BoundaryError::OutputLimit),
            Reject::Inspection(BoundaryError::Serialization),
            Reject::Inspection(BoundaryError::RouteMismatch),
            Reject::Inspection(BoundaryError::Core(CoreBridgeError::Blocked)),
            Reject::Inspection(BoundaryError::Core(CoreBridgeError::Incomplete)),
            Reject::Inspection(BoundaryError::Core(CoreBridgeError::Overload)),
            Reject::MissingCredential,
            Reject::LocalAuthRequired,
            Reject::LocalAuthInvalid,
            Reject::Header,
            Reject::HeaderTooLarge,
            Reject::Expectation,
        ] {
            let (status, code) = reject.status_and_code();
            assert!(status.is_client_error() || status.is_server_error());
            assert!(!code.as_str().is_empty());
        }
    }
}
