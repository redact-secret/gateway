//! Routing, TLS, headers, credentials, transmission, and response relay (ADR 0002, 0005).
//!
//! This is the only module that may hold an HTTP client or, later, credentials. Its entry
//! point accepts only [`SanitizedRequest`]: there is no overload for bytes, `http::Request`,
//! or generic JSON, and no raw-body fallback. Scaffold: no route exists and nothing is
//! sent. [`Upstream::forward`] consumes the request and reports `NotImplemented`.

use std::fmt;

use crate::boundary::SanitizedRequest;
use crate::telemetry::SafeCode;

/// Shared upstream client holder. Built once at startup from the `RuntimePlan` (#23-#25).
#[derive(Debug)]
pub struct Upstream {
    #[expect(dead_code, reason = "used by forwarding in #19/#23")]
    client: reqwest::Client,
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

impl Upstream {
    /// Build the client with redirects and proxy-from-environment disabled and certificate
    /// verification left at the verified default.
    ///
    /// # Errors
    /// [`TransportError::ClientInit`] if the TLS-capable client cannot be built.
    pub fn new() -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| TransportError::ClientInit)?;
        Ok(Self { client })
    }

    /// The only forwarding entry point. Accepts only the sealed [`SanitizedRequest`].
    ///
    /// # Errors
    /// Always [`TransportError::NotImplemented`] in this scaffold.
    #[expect(
        clippy::unused_async,
        reason = "async signature is the stable contract for #19"
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
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::admission::{Admission, CapacityPlan};
    use crate::boundary;
    use crate::config::RouteId;
    use crate::core_bridge::CompleteInspection;
    use crate::protocol::ValidatedRequest;

    #[tokio::test]
    async fn forward_is_not_implemented_and_sends_nothing() {
        let one = NonZeroU32::new(1).expect("nonzero");
        let admission = Admission::new(&CapacityPlan::new(one, one, one, one, one));
        let v = ValidatedRequest::for_test(
            admission.try_reserve_memory(1).expect("reserve"),
            admission.try_receipt().expect("receipt"),
        );
        let s = boundary::approve(
            v,
            CompleteInspection::for_test(b"x".to_vec()),
            RouteId::new("r"),
        )
        .expect("approved");
        let upstream = Upstream::new().expect("client");
        assert_eq!(
            upstream.forward(s).await.unwrap_err(),
            TransportError::NotImplemented
        );
    }
}
