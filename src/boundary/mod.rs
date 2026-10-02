//! Inspection orchestration and the only creator of [`SanitizedRequest`] (ADR 0002, 0005).
//!
//! [`approve`] is the single path from `ValidatedRequest` to `SanitizedRequest`. It
//! requires the three proofs the request-state contract names: a validated (classified)
//! request, a complete core inspection, and an approved route. Failure yields a safe
//! error and no sanitized value, so nothing can reach `transport`.

mod sealed;

use std::fmt;

pub use sealed::SanitizedRequest;

use crate::config::RouteId;
use crate::core_bridge::CompleteInspection;
use crate::protocol::ValidatedRequest;
use crate::telemetry::SafeCode;

/// Safe boundary failure. Carries no body or offending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BoundaryError {
    /// Transformed output is empty or larger than the memory reserved for it.
    OutputLimit,
}

impl BoundaryError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::OutputLimit => SafeCode::LimitExceeded,
        }
    }
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for BoundaryError {}

/// Approve a validated request whose core inspection completed.
///
/// The output must fit the memory already reserved for the request; the reservation
/// moves into the sanitized value and is released only when it is dropped.
///
/// # Errors
/// [`BoundaryError::OutputLimit`] when the output is empty or exceeds the reservation.
pub fn approve(
    validated: ValidatedRequest,
    inspection: CompleteInspection,
    route: RouteId,
) -> Result<SanitizedRequest, BoundaryError> {
    let output = inspection.into_output();
    let fits =
        u32::try_from(output.len()).is_ok_and(|len| len > 0 && len <= validated.memory().units());
    if !fits {
        return Err(BoundaryError::OutputLimit);
    }
    Ok(SanitizedRequest::new(
        output,
        route,
        validated.into_memory(),
    ))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::admission::{Admission, CapacityPlan};

    fn validated(units: u32) -> (Admission, ValidatedRequest) {
        let one = NonZeroU32::new(1).expect("nonzero");
        let big = NonZeroU32::new(64).expect("nonzero");
        let admission = Admission::new(&CapacityPlan::new(one, big, one, one, one));
        let memory = admission.try_reserve_memory(units).expect("reserve");
        let receipt = admission.try_receipt().expect("receipt");
        let v = ValidatedRequest::for_test(memory, receipt);
        (admission, v)
    }

    #[test]
    fn approve_binds_output_route_and_memory() {
        let (admission, v) = validated(16);
        let s = approve(
            v,
            CompleteInspection::for_test(b"{}".to_vec()),
            RouteId::new("synthetic-route"),
        )
        .expect("approved");
        assert_eq!(s.body(), b"{}");
        assert_eq!(s.route().as_str(), "synthetic-route");
        // Reservation is still held while the sanitized value lives.
        assert!(admission.try_reserve_memory(64).is_err());
        drop(s);
        assert!(admission.try_reserve_memory(64).is_ok());
    }

    #[test]
    fn approve_rejects_empty_or_oversized_output() {
        let (_a, v) = validated(4);
        let err = approve(
            v,
            CompleteInspection::for_test(vec![0; 5]),
            RouteId::new("r"),
        );
        assert_eq!(err.unwrap_err(), BoundaryError::OutputLimit);
        let (_a, v) = validated(4);
        let err = approve(
            v,
            CompleteInspection::for_test(Vec::new()),
            RouteId::new("r"),
        );
        assert_eq!(err.unwrap_err(), BoundaryError::OutputLimit);
    }

    #[test]
    fn sanitized_debug_hides_body() {
        let (_a, v) = validated(16);
        let s = approve(
            v,
            CompleteInspection::for_test(b"SYNTHETIC".to_vec()),
            RouteId::new("r"),
        )
        .expect("approved");
        assert!(!format!("{s:?}").contains("SYNTHETIC"));
    }
}
