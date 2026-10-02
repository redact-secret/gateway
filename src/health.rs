//! Liveness and readiness (ADR 0006, errors-and-telemetry contract).
//!
//! Local only. Neither endpoint calls an upstream, reads configuration, or uses
//! credentials. Every other path and method is rejected locally with the gateway's
//! `unsupported_input` safe code: there is no proxy route, no forwarding, and no
//! pass-through in the skeleton, and health paths never become upstream routes.
//!
//! Readiness is an explicit state, not process existence: it is true only when a validated
//! [`RuntimePlan`] is held, required core/transport initialization has been marked
//! complete, and the server is accepting work (cleared again when shutdown begins).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::config::RuntimePlan;

/// Liveness path.
pub const LIVENESS_PATH: &str = "/healthz";
/// Readiness path.
pub const READINESS_PATH: &str = "/readyz";

/// Shared readiness state. Holds the validated plan (so "ready" cannot exist without one)
/// and two explicit flags. No configuration is stored or reread here.
#[derive(Debug)]
pub struct HealthState {
    plan: Arc<RuntimePlan>,
    initialized: AtomicBool,
    accepting: AtomicBool,
}

impl HealthState {
    /// New state for a validated plan. Starts **not ready**.
    #[must_use]
    pub const fn new(plan: Arc<RuntimePlan>) -> Self {
        Self {
            plan,
            initialized: AtomicBool::new(false),
            accepting: AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn plan(&self) -> &RuntimePlan {
        &self.plan
    }

    /// Record that required core and transport initialization succeeded.
    pub fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    /// Record whether the listener is accepting work. Cleared when shutdown begins.
    pub fn set_accepting(&self, accepting: bool) {
        self.accepting.store(accepting, Ordering::Release);
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.initialized.load(Ordering::Acquire) && self.accepting.load(Ordering::Acquire)
    }
}

/// Router for the health endpoints plus the local rejection fallback.
pub fn router(state: Arc<HealthState>) -> Router {
    Router::new()
        .route(LIVENESS_PATH, get(liveness))
        .route(
            READINESS_PATH,
            get(move || {
                let ready = state.is_ready();
                async move { readiness(ready) }
            }),
        )
        .method_not_allowed_fallback(unsupported)
        .fallback(unsupported)
}

fn json(status: StatusCode, body: &'static str) -> Response {
    let mut resp = (status, body).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn liveness() -> Response {
    json(StatusCode::OK, r#"{"status":"live"}"#)
}

fn readiness(ready: bool) -> Response {
    if ready {
        json(StatusCode::OK, r#"{"status":"ready"}"#)
    } else {
        json(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"code":"not_ready"}}"#,
        )
    }
}

/// Local rejection for every route or method that is not a health endpoint. Takes no
/// extractors, so it never reads the request body, headers, or URL, and the response is
/// a fixed string that cannot contain request content.
async fn unsupported() -> Response {
    json(
        StatusCode::NOT_FOUND,
        r#"{"error":{"code":"unsupported_input"}}"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::CapacityPlan;
    use crate::config::{ContentPolicy, DeploymentAuthority, ResourcePolicy};
    use crate::telemetry::SafeCode;
    use std::num::NonZeroU32;

    fn plan() -> Arc<RuntimePlan> {
        let one = NonZeroU32::MIN;
        Arc::new(RuntimePlan::new(
            DeploymentAuthority::placeholder(),
            ContentPolicy::new(crate::core_bridge::parse_profile("common").expect("profile")),
            ResourcePolicy::new(CapacityPlan::new(one, one, one, one, one)),
        ))
    }

    #[test]
    fn ready_needs_both_flags() {
        let s = HealthState::new(plan());
        assert!(!s.is_ready());
        s.mark_initialized();
        assert!(!s.is_ready());
        s.set_accepting(true);
        assert!(s.is_ready());
        s.set_accepting(false);
        assert!(!s.is_ready());
    }

    #[test]
    fn router_builds() {
        let _router = router(Arc::new(HealthState::new(plan())));
    }

    #[test]
    fn fixed_bodies_match_safe_codes() {
        assert!(r#"{"error":{"code":"not_ready"}}"#.contains(SafeCode::NotReady.as_str()));
        assert!(
            r#"{"error":{"code":"unsupported_input"}}"#
                .contains(SafeCode::UnsupportedInput.as_str())
        );
    }
}
