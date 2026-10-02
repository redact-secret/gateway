//! The sealed final request type (ADR 0002, request-state contract).
//!
//! `SanitizedRequest` has private fields and a constructor visible only to the parent
//! `boundary` module (`pub(super)`). It derives no `Clone`, `Default`, or `Deserialize`
//! and has no setter or body mutation.

use std::fmt;

use crate::admission::MemoryReservation;
use crate::config::RouteId;

/// Core processing complete, output structurally and size validated, bound to an approved
/// route. The only type `transport` accepts.
pub struct SanitizedRequest {
    body: Vec<u8>,
    route: RouteId,
    _memory: MemoryReservation,
}

impl SanitizedRequest {
    pub(super) fn new(body: Vec<u8>, route: RouteId, memory: MemoryReservation) -> Self {
        Self {
            body,
            route,
            _memory: memory,
        }
    }

    /// The approved, immutable body. Outbound length is recomputed from this.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }
}

impl fmt::Debug for SanitizedRequest {
    /// Never prints body content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SanitizedRequest")
            .field("route", &self.route)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}
