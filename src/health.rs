//! Liveness and readiness (ADR 0006, errors-and-telemetry contract).
//!
//! Scaffold: no route is registered and no listener is bound. #4 owns the endpoints and
//! ties readiness to a valid `RuntimePlan` plus initialized core.

use axum::Router;

/// Router for health endpoints. Intentionally empty until #4.
pub fn router() -> Router {
    Router::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn router_builds_without_routes() {
        let _router = super::router();
    }
}
