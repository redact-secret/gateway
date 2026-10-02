//! Routing, TLS, headers, credentials, transmission, and response relay (ADR 0002, 0005,
//! 0009, 0013).
//!
//! This is the only module that may hold an HTTP client or, later, credentials. Its entry
//! point accepts only [`SanitizedRequest`]: there is no overload for bytes, `http::Request`,
//! or generic JSON, and no raw-body fallback.
//!
//! Outbound authority (#23, docs/contracts/upstream-destinations.md): [`Upstream`] is built
//! once at startup from the immutable [`UpstreamAuthority`]. Destinations come only from a
//! reviewed provider profile ([`destination`]); the only way to obtain a destination or
//! request builder is by [`RouteId`]; no API accepts a caller-supplied URL. The one shared
//! client has redirects off, no proxy (inherited environment ignored), HTTPS-only, verified
//! certificates and hostnames (no toggle exists), and resolves names only through the
//! address-policy resolver ([`resolver`]). It carries no default headers, so credentials
//! stay request-local ([`credential`], [`headers`]; #24). Forwarding of request bodies is #20; `forward` is still a stub.

pub mod credential;
pub mod destination;
pub mod headers;
pub mod resolver;

use std::fmt;

use crate::boundary::SanitizedRequest;
use crate::config::{RouteId, RuntimePlan, UpstreamAuthority};
use crate::telemetry::SafeCode;

use destination::{Destination, RouteBinding};
use resolver::PolicyResolver;

/// Shared upstream client holder. Built once at startup from the `RuntimePlan`.
#[derive(Debug)]
pub struct Upstream {
    client: reqwest::Client,
    routes: Box<[RouteBinding]>,
}

/// Placeholder for the relayed upstream response (#19/#20).
#[derive(Debug)]
#[non_exhaustive]
pub struct UpstreamResponse {}

/// Safe transport failure. Carries no body, URL, header, or credential content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// No forwarding route exists in this scaffold.
    NotImplemented,
    /// Client construction failed.
    ClientInit,
    /// The route id is not in the static route table (including: no upstream configured).
    UnknownRoute,
}

impl TransportError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        SafeCode::TransportFailure
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
    reqwest::Client::builder()
        // A redirect would move a credential-bearing request to an unreviewed destination.
        .redirect(reqwest::redirect::Policy::none())
        // Never inherit HTTP_PROXY/HTTPS_PROXY/ALL_PROXY/NO_PROXY or system proxy settings.
        .no_proxy()
        // Defense in depth over the https-only destination table.
        .https_only(true)
        .referer(false)
        // Resolution only through the allowlist + address policy; connect uses its output.
        .dns_resolver(resolver)
    // Certificate and hostname verification stay at the verified default; the
    // `danger_*` builder toggles are never called anywhere in this crate.
}

impl Upstream {
    /// Build the client and the static route table from deployment authority. `None`
    /// (no upstream configured) yields a client with an empty route table: every route
    /// lookup fails closed.
    ///
    /// # Errors
    /// [`TransportError::ClientInit`] if the reviewed profile or TLS-capable client cannot
    /// be built (reported at startup, never per request).
    pub fn new(authority: Option<&UpstreamAuthority>) -> Result<Self, TransportError> {
        let routes = match authority {
            Some(a) => destination::reviewed_routes(a.provider())
                .map_err(|_| TransportError::ClientInit)?,
            None => Vec::new(),
        };
        let resolver = PolicyResolver::system(destination::allowed_hosts(&routes));
        let client = hardened_builder(resolver)
            .build()
            .map_err(|_| TransportError::ClientInit)?;
        Ok(Self {
            client,
            routes: routes.into_boxed_slice(),
        })
    }

    /// Build from the immutable plan's deployment authority.
    ///
    /// # Errors
    /// See [`Upstream::new`].
    pub fn from_plan(plan: &RuntimePlan) -> Result<Self, TransportError> {
        Self::new(plan.deployment().upstream())
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
    /// Callers add request-local headers and the sealed body (#20); they cannot change the
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

    /// The only forwarding entry point. Accepts only the sealed [`SanitizedRequest`].
    ///
    /// # Errors
    /// Always [`TransportError::NotImplemented`] in this scaffold (#20).
    #[expect(
        clippy::unused_async,
        reason = "async signature is the stable contract for #20"
    )]
    pub async fn forward(
        &self,
        request: SanitizedRequest,
    ) -> Result<UpstreamResponse, TransportError> {
        drop(request);
        Err(TransportError::NotImplemented)
    }
}

#[cfg(test)]
mod tests;
