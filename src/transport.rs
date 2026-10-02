//! Routing, TLS, headers, credentials, transmission, and response relay (ADR 0002, 0005,
//! 0009, 0013, 0017).
//!
//! This is the only module that may hold an HTTP client or credentials. Its entry point
//! accepts only [`SanitizedRequest`]: there is no overload for bytes, `http::Request`, or
//! generic JSON, and no raw-body fallback.
//!
//! Outbound authority (#23, docs/contracts/upstream-destinations.md): [`Upstream`] is built
//! once at startup from the immutable [`UpstreamAuthority`]. Destinations come only from a
//! reviewed provider profile ([`destination`]); the only way to obtain a destination or
//! request builder is by [`RouteId`]; no API accepts a caller-supplied URL. The one shared
//! client has redirects off, no proxy (inherited environment ignored), HTTPS-only, verified
//! certificates and hostnames (no toggle exists), and resolves names only through the
//! address-policy resolver ([`resolver`]). It carries no default headers, so credentials
//! stay request-local ([`credential`], [`headers`]; #24).
//!
//! Forwarding (#20, ADR 0017): [`Upstream::forward`] sends the sealed body once, reads the
//! provider's ordinary JSON response under finite connect, response-header, and total
//! deadlines and hard header and body byte bounds, and returns it as a complete bounded
//! [`UpstreamResponse`]. Rules:
//!
//! * **No retry, no replay.** One `send` per request. Idle connection pooling is off, so
//!   the HTTP client never re-sends a request on a stale reused connection either.
//! * **No raw fallback.** Every failure is a [`TransportError`] with a fixed safe code; the
//!   original body does not exist at this layer.
//! * **Cancellation is dropping.** `forward` is an ordinary future owned by the request
//!   handler; when the caller disconnects or the Gateway shuts down it is dropped, which
//!   aborts the exchange and releases the permit and buffers. Nothing is spawned. Bytes
//!   already written to the provider cannot be retracted.
//! * Provider response bodies are relayed unredacted and never logged or put in errors.

pub mod credential;
pub mod destination;
pub mod headers;
mod relay;
pub mod resolver;

use std::error::Error as _;
use std::fmt;
use std::sync::Arc;

use tokio::time::{Instant, timeout_at};

use crate::admission::{RequestLimits, UpstreamPermit};
use crate::boundary::SanitizedRequest;
use crate::config::{RouteId, RuntimePlan, UpstreamAuthority};
use crate::telemetry::{Metrics, SafeCode, Stage};

use destination::{Destination, RouteBinding};
use resolver::PolicyResolver;

pub use relay::{HeldBody, UpstreamResponse};

/// Most response header fields the HTTP parser accepts; more is a malformed response.
const MAX_RESPONSE_HEADER_FIELDS: usize = 64;

/// Shared upstream client holder. Built once at startup from the `RuntimePlan`.
#[derive(Debug)]
pub struct Upstream {
    client: reqwest::Client,
    routes: Box<[RouteBinding]>,
    limits: RequestLimits,
    metrics: Option<Arc<Metrics>>,
}

/// Safe transport failure. Carries no body, URL, header, or credential content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// Client construction failed (startup only).
    ClientInit,
    /// The route id is not in the static route table (including: no upstream configured).
    UnknownRoute,
    /// A connect, response-header, or total deadline elapsed.
    Timeout,
    /// The provider could not be reached (resolution, address policy, refusal).
    Connect,
    /// TLS to the provider failed (certificate, hostname, handshake).
    Tls,
    /// Malformed or truncated response, disconnect after sending, or unsupported coding.
    InvalidResponse,
    /// Response headers or body over the configured bounds.
    ResponseTooLarge,
}

impl TransportError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::ClientInit | Self::UnknownRoute => SafeCode::TransportFailure,
            Self::Timeout => SafeCode::UpstreamTimeout,
            Self::Connect => SafeCode::UpstreamUnavailable,
            Self::Tls => SafeCode::UpstreamTls,
            Self::InvalidResponse => SafeCode::UpstreamInvalidResponse,
            Self::ResponseTooLarge => SafeCode::UpstreamResponseTooLarge,
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for TransportError {}

/// Settings every client in this crate must have. One place, so the production builder and
/// the unit-test builder cannot drift on security-relevant options.
fn hardened_builder(resolver: PolicyResolver) -> reqwest::ClientBuilder {
    #[cfg(test)]
    tests::note_client_build();
    reqwest::Client::builder()
        // A redirect would move a credential-bearing request to an unreviewed destination.
        .redirect(reqwest::redirect::Policy::none())
        // Never inherit HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY or system proxy settings.
        .no_proxy()
        // Defense in depth over the https-only destination table.
        .https_only(true)
        .referer(false)
        // No idle pooling: every request uses a fresh connection, so the client can never
        // replay a request on a stale reused connection (#20, "never replay").
        .pool_max_idle_per_host(0)
        .http1_max_headers(MAX_RESPONSE_HEADER_FIELDS)
        // Resolution only through the allowlist + address policy; connect uses its output.
        .dns_resolver(resolver)
    // Certificate and hostname verification stay at the verified default; the
    // `danger_*` builder toggles are never called anywhere in this crate.
}

impl Upstream {
    /// Build the client and the static route table from deployment authority with the
    /// provisional limits. `None` (no upstream configured) yields a client with an empty
    /// route table: every route lookup fails closed.
    ///
    /// # Errors
    /// [`TransportError::ClientInit`] if the reviewed profile or TLS-capable client cannot
    /// be built (reported at startup, never per request).
    pub fn new(authority: Option<&UpstreamAuthority>) -> Result<Self, TransportError> {
        Self::with_limits(authority, RequestLimits::provisional())
    }

    /// As [`Upstream::new`] with explicit deadlines and bounds. The connect deadline is a
    /// property of the client and so is fixed here, once.
    ///
    /// # Errors
    /// See [`Upstream::new`].
    pub fn with_limits(
        authority: Option<&UpstreamAuthority>,
        limits: RequestLimits,
    ) -> Result<Self, TransportError> {
        let routes = match authority {
            Some(a) => destination::reviewed_routes(a.provider())
                .map_err(|_| TransportError::ClientInit)?,
            None => Vec::new(),
        };
        let resolver = PolicyResolver::system(destination::allowed_hosts(&routes));
        let client = hardened_builder(resolver)
            .connect_timeout(limits.upstream_connect())
            .build()
            .map_err(|_| TransportError::ClientInit)?;
        Ok(Self {
            client,
            routes: routes.into_boxed_slice(),
            limits,
            metrics: None,
        })
    }

    /// Build from the immutable plan's deployment authority and resource limits.
    ///
    /// # Errors
    /// See [`Upstream::new`].
    pub fn from_plan(plan: &RuntimePlan) -> Result<Self, TransportError> {
        Self::with_limits(plan.deployment().upstream(), *plan.resources().limits())
    }

    /// Record upstream stage timings and attempts into `metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The reviewed destination bound to `route`. The route id is operator-defined and
    /// bound by `boundary`; it is never parsed from a URL, header, or body.
    ///
    /// # Errors
    /// [`TransportError::UnknownRoute`] for any id outside the static table.
    pub fn destination(&self, route: &RouteId) -> Result<&Destination, TransportError> {
        self.routes
            .iter()
            .find(|b| b.id() == route)
            .map(RouteBinding::destination)
            .ok_or(TransportError::UnknownRoute)
    }

    /// A request builder for the route's fixed method and URL on the shared client.
    /// Callers add request-local headers and the sealed body; they cannot change the
    /// URL or method. No default headers exist on the client.
    ///
    /// # Errors
    /// [`TransportError::UnknownRoute`] for any id outside the static table.
    pub fn post(&self, route: &RouteId) -> Result<reqwest::RequestBuilder, TransportError> {
        let d = self.destination(route)?;
        Ok(self.client.post(d.url().clone()))
    }

    /// The vetted request for `request`: the route's fixed method and URL, the complete
    /// regenerated header set ([`headers::wire_headers`]), and the sealed body, with
    /// `Content-Length` computed from exactly those bytes. Consumes the request-local
    /// [`headers::VettedHeaders`] so the credential cannot outlive the request or be reused
    /// for a retry (there are none). `Host` comes from the reviewed URL only.
    ///
    /// The body is copied once from the sealed buffer; the caller keeps the
    /// [`SanitizedRequest`] (and its memory reservation) alive until the send completes.
    ///
    /// # Errors
    /// [`TransportError::UnknownRoute`] for any id outside the static table.
    pub fn outbound(
        &self,
        headers: headers::VettedHeaders,
        request: &SanitizedRequest,
    ) -> Result<reqwest::RequestBuilder, TransportError> {
        let body = request.body();
        let wire = headers::wire_headers(headers, body.len());
        Ok(self
            .post(request.route())?
            .headers(wire)
            .body(body.to_vec()))
    }

    /// The only forwarding entry point. Accepts only the sealed [`SanitizedRequest`], the
    /// request-local vetted headers, and an [`UpstreamPermit`] the caller acquired
    /// independently of inspection capacity (ADR 0003). The permit is held until the
    /// returned response's body is dropped, or until this future ends in an error.
    ///
    /// The sealed request (and its memory reservation) is kept alive until the exchange
    /// ends. Exactly one send is attempted; no failure is retried.
    ///
    /// Dropping the returned future cancels the exchange and releases everything.
    ///
    /// # Errors
    /// A [`TransportError`] for an unknown route (before any permit-held work), an elapsed
    /// deadline, a connect/TLS failure, an unrelayable response, or an over-bound
    /// response. Nothing from the provider's response is carried in the error.
    pub async fn forward(
        &self,
        request: SanitizedRequest,
        headers: headers::VettedHeaders,
        permit: UpstreamPermit,
    ) -> Result<UpstreamResponse, TransportError> {
        let builder = self.outbound(headers, &request)?;
        let started = Instant::now();
        // Limits are validated finite (at most an hour), so the sums cannot overflow; an
        // impossible overflow is an immediate timeout rather than an unbounded wait.
        let total = self.limits.upstream_total();
        let (Some(total_deadline), Some(header_deadline)) = (
            started.checked_add(total),
            started.checked_add(self.limits.upstream_header().min(total)),
        ) else {
            return Err(TransportError::Timeout);
        };
        // Cancellation is dropping this future, so there is nothing to poll for it; the
        // deadline is checked right before the send is initiated, as the last step.
        if Instant::now() >= total_deadline {
            return Err(TransportError::Timeout);
        }
        if let Some(m) = &self.metrics {
            m.note_upstream_attempt();
        }
        let result = self
            .exchange(builder, started, header_deadline, total_deadline)
            .await;
        if let Some(m) = &self.metrics {
            m.record(Stage::UpstreamTotal, started.elapsed());
        }
        // The sealed request (memory reservation) lives until here.
        drop(request);
        result.map(|(status, headers, body)| UpstreamResponse::new(status, headers, body, permit))
    }

    /// One send, then the bounded read of the response. Returns relayable parts only.
    async fn exchange(
        &self,
        builder: reqwest::RequestBuilder,
        started: Instant,
        header_deadline: Instant,
        total_deadline: Instant,
    ) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>), TransportError> {
        let mut response = timeout_at(header_deadline, builder.send())
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|e| classify(&e))?;
        if let Some(m) = &self.metrics {
            m.record(Stage::UpstreamFirstResponse, started.elapsed());
        }
        let header_cap =
            usize::try_from(self.limits.max_response_header_bytes).unwrap_or(usize::MAX);
        let body_cap = usize::try_from(self.limits.max_response_body_bytes).unwrap_or(usize::MAX);
        if header_bytes(response.headers()) > header_cap {
            return Err(TransportError::ResponseTooLarge);
        }
        let status = response.status();
        let relayed = headers::relay_response_headers(response.headers())
            .map_err(|_| TransportError::InvalidResponse)?;
        let declared = response
            .content_length()
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        if declared.is_some_and(|n| n > body_cap) {
            return Err(TransportError::ResponseTooLarge);
        }
        let mut body: Vec<u8> = Vec::with_capacity(declared.unwrap_or(0).min(body_cap));
        timeout_at(total_deadline, async {
            while let Some(chunk) = response.chunk().await.map_err(|e| classify(&e))? {
                if body.len().saturating_add(chunk.len()) > body_cap {
                    return Err(TransportError::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(())
        })
        .await
        .map_err(|_| TransportError::Timeout)??;
        Ok((status, relayed, body))
    }
}

/// Bytes the response header block occupies: each name, value, and the `": "` and CRLF.
fn header_bytes(headers: &reqwest::header::HeaderMap) -> usize {
    headers.iter().fold(0_usize, |total, (name, value)| {
        total
            .saturating_add(name.as_str().len())
            .saturating_add(value.len())
            .saturating_add(4)
    })
}

/// Map a client error to a fixed safe category. Looks at the error's structure and, for TLS
/// only, at the lowercase text of its source chain; none of that text is ever stored or
/// returned.
fn classify(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        return TransportError::Timeout;
    }
    if error.is_connect() {
        return if mentions_tls(error) {
            TransportError::Tls
        } else {
            TransportError::Connect
        };
    }
    if error.is_builder() {
        return TransportError::Connect;
    }
    TransportError::InvalidResponse
}

/// Whether anything in the error's source chain is a TLS failure.
fn mentions_tls(error: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(e) = current {
        let invalid_data = e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::InvalidData);
        let text = e.to_string().to_ascii_lowercase();
        if invalid_data
            || ["certificate", "tls", "handshake", "invalid peer"]
                .iter()
                .any(|needle| text.contains(needle))
        {
            return true;
        }
        current = e.source();
    }
    false
}

#[cfg(test)]
mod tests;
