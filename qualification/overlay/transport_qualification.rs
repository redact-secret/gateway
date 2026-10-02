//! QUALIFICATION BUILD ONLY (ADR 0020). RSG-QUALIFICATION-BUILD-NOT-FOR-RELEASE.
//!
//! This file does not exist in the shipped source tree. `qualification/build.sh` copies it
//! into a generated crate named `redact-secret-gateway-qualification` next to the seam patch.
//! It builds an [`Upstream`] whose single reviewed route points at a loopback fake
//! provider over plain HTTP, so the OpenAI SDKs can be exercised end to end without a
//! provider key or network cost. Everything else (the route table shape, the hardened
//! client builder, limits, deadlines, metrics, permits, header policy, the sealed request
//! path) is the production code: only the destination origin and the address policy for
//! loopback differ.
//!
//! Production address and TLS policy for real destinations is untouched: the reviewed
//! provider profile still names `https://api.openai.com`, and nothing here weakens
//! `Origin::parse`, the resolver for that host, or TLS verification. The fake address must
//! be a loopback IP literal; anything else is refused at startup.

use std::net::SocketAddr;
use std::sync::Arc;

use super::destination::{self, Destination, Origin, RouteBinding};
use super::resolver::{AddressPolicy, AddressSource, PolicyResolver};
use super::{TransportError, Upstream, hardened_builder};
use crate::config::{RouteId, RuntimePlan};

/// Never resolves anything: the fake origin is an IP literal, so no lookup is needed.
struct NoSource;

impl AddressSource for NoSource {
    fn lookup(
        &self,
        _host: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<std::net::IpAddr>>> + Send>>
    {
        Box::pin(async { None })
    }
}

impl Upstream {
    /// As [`Upstream::from_plan`], except the reviewed route's origin is the loopback fake
    /// at `fake`. When the plan configures no upstream the route table stays empty, exactly
    /// as in production.
    ///
    /// # Errors
    /// [`TransportError::ClientInit`] when `fake` is not a loopback address or the client
    /// cannot be built.
    pub(crate) fn from_plan_with_fake_provider(
        plan: &RuntimePlan,
        fake: SocketAddr,
    ) -> Result<Self, TransportError> {
        if !fake.ip().is_loopback() {
            return Err(TransportError::ClientInit);
        }
        let limits = *plan.resources().limits();
        let routes = match plan.deployment().upstream() {
            Some(authority) => {
                let reviewed = destination::reviewed_routes(authority.provider())
                    .map_err(|_| TransportError::ClientInit)?;
                let path = reviewed
                    .first()
                    .map(|r| r.destination().path())
                    .ok_or(TransportError::ClientInit)?;
                let dest = Destination::for_test(Origin::for_test_http(fake), path)
                    .map_err(|_| TransportError::ClientInit)?;
                vec![RouteBinding::for_test(
                    RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE),
                    dest,
                )]
            }
            None => Vec::new(),
        };
        let resolver =
            PolicyResolver::new(vec![], Arc::new(NoSource), AddressPolicy::PublicOrLoopback);
        let client = hardened_builder(resolver)
            .https_only(false)
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
}
