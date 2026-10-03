//! Inspection orchestration and the only creator of [`SanitizedRequest`] (ADR 0002, 0005,
//! 0015).
//!
//! [`Inspection::inspect_and_approve`] is the path from a `ValidatedRequest` to a
//! `SanitizedRequest`: every allowed text is inspected through the pinned core on the
//! bounded worker pool, the typed request is serialized afresh with a bounded writer, and
//! [`approve`] (the only constructor of the sealed type) requires the three proofs the
//! request-state contract names: a validated request, a complete core inspection, and an
//! approved route. Any failure yields a safe error and no sanitized value, so nothing can
//! reach `transport`, and there is no raw-body or original-body fallback anywhere.

mod sealed;

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

pub use sealed::SanitizedRequest;

use crate::admission::{Admission, CapacityPlan, MEMORY_UNIT_BYTES, RequestLimits};
use crate::config::{ContentPolicy, OnWarn, RouteId};
use crate::core_bridge::pool::InspectionPool;
use crate::core_bridge::{
    CompleteInspection, CoreBridgeError, Inspector, InspectorSpec, RequestScope,
};
use crate::protocol::ValidatedRequest;
use crate::protocol::chat::{SerializeError, SlotMode};
use crate::telemetry::{Metrics, SafeCode, Stage};

/// Most inspection worker threads, whatever the configured inspection capacity.
const MAX_WORKERS: usize = 16;
/// Largest job queue, whatever the configured inspection capacity. Queued plus running jobs
/// can never exceed the inspection permits, so this only matters for huge capacities.
const MAX_QUEUE: usize = 1024;

/// Safe boundary failure. Carries no body or offending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BoundaryError {
    /// Transformed output is empty or larger than its bound.
    OutputLimit,
    /// The transformed request could not be serialized.
    Serialization,
    /// Core inspection did not complete successfully (limit, finding limit, detector,
    /// policy or placeholder failure, `Block`/`Warn` rejection, overload, shutdown).
    Core(CoreBridgeError),
}

impl BoundaryError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::OutputLimit => SafeCode::LimitExceeded,
            Self::Serialization => SafeCode::UnsupportedInput,
            Self::Core(e) => e.code(),
        }
    }
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for BoundaryError {}

impl From<CoreBridgeError> for BoundaryError {
    fn from(e: CoreBridgeError) -> Self {
        Self::Core(e)
    }
}

/// Startup-built inspection service: the bounded worker pool (one core registry per worker
/// thread, built here so readiness depends on core initialization), the immutable spec, and
/// the capacity owner that hands out inspection permits.
#[derive(Debug)]
pub struct Inspection {
    pool: InspectionPool,
    spec: Arc<InspectorSpec>,
    admission: Arc<Admission>,
    max_output: usize,
    metrics: Option<Arc<Metrics>>,
    /// Test-only: parks a started job on a worker until opened (see [`test_gate`]).
    #[cfg(test)]
    gate: Option<Arc<test_gate::Gate>>,
}

impl Inspection {
    /// Build the core inspectors. Workers and queue are derived from the configured
    /// inspection capacity (queued plus running jobs never exceed it), not new settings.
    ///
    /// # Errors
    /// The [`CoreBridgeError`] when the profile, PII selection, or limits are refused by the
    /// pinned core, or a worker cannot start.
    pub fn start(
        admission: Arc<Admission>,
        content: &ContentPolicy,
        limits: &RequestLimits,
        capacity: &CapacityPlan,
    ) -> Result<Self, CoreBridgeError> {
        let pii: Vec<&str> = content.pii().iter().map(String::as_str).collect();
        let max_input = usize::try_from(limits.max_body_bytes).unwrap_or(usize::MAX);
        let max_findings = usize::try_from(content.max_findings()).unwrap_or(usize::MAX);
        let spec = InspectorSpec::for_profile(content.profile(), &pii, max_input, max_findings)?
            .with_warning_rejection(content.on_warn() == OnWarn::Reject);
        let permits = usize::try_from(capacity.inspection_permits().get()).unwrap_or(usize::MAX);
        let parallelism = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let workers = NonZeroUsize::new(permits.min(parallelism).min(MAX_WORKERS))
            .unwrap_or(NonZeroUsize::MIN);
        let queue = NonZeroUsize::new(permits.min(MAX_QUEUE)).unwrap_or(NonZeroUsize::MIN);
        let pool = InspectionPool::start(&spec, workers, queue)?;
        Ok(Self {
            pool,
            spec: Arc::new(spec),
            admission,
            max_output: max_input,
            metrics: None,
            #[cfg(test)]
            gate: None,
        })
    }

    /// Test-only: park every started inspection job on `gate` after its worker picked it
    /// up and before any core call, so a test can hold a request in the "inspection
    /// running" state deterministically. Compiled only under `cfg(test)`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_test_gate(mut self, gate: Arc<test_gate::Gate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Record the serialization stage timing (measured on the worker) into `metrics`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Number of inspection worker threads (each owns one core registry).
    #[must_use]
    pub const fn workers(&self) -> usize {
        self.pool.workers()
    }

    /// The worker pool, for capacity and cancellation observation in tests. Jobs submitted
    /// here cannot produce a `SanitizedRequest`: only [`Self::inspect_and_approve`] can.
    #[must_use]
    pub const fn pool(&self) -> &InspectionPool {
        &self.pool
    }

    /// Inspect every allowed text of `validated` on the worker pool, serialize the
    /// transformed request, and approve it.
    ///
    /// The job (not this future) owns the inspection permit and, through the moved request,
    /// the memory reservation until the core call really finishes: dropping this future
    /// cancels only the wait, a started job runs to its end, and its result is discarded and
    /// never becomes a `SanitizedRequest` (ADR 0004).
    ///
    /// # Errors
    /// [`BoundaryError`] for overload, any core failure or rejection, or an output that
    /// cannot be serialized within its bound.
    pub async fn inspect_and_approve(
        &self,
        validated: ValidatedRequest,
        route: RouteId,
    ) -> Result<SanitizedRequest, BoundaryError> {
        let permit = self
            .admission
            .try_inspection()
            .map_err(|_| BoundaryError::Core(CoreBridgeError::Overload))?;
        let spec = Arc::clone(&self.spec);
        let max_output = self.max_output;
        let metrics = self.metrics.clone();
        #[cfg(test)]
        let gate = self.gate.clone();
        let handle = self.pool.submit_job(permit, move |inspector| {
            #[cfg(test)]
            if let Some(gate) = &gate {
                gate.enter();
            }
            inspect_request(inspector, &spec, max_output, validated, metrics.as_deref())
        })?;
        let (validated, inspection) = handle.await??;
        approve(validated, inspection, route)
    }
}

/// Worker-side work for one request: inspect in deterministic order, replace text in place,
/// serialize once under the output bound, and mint the completeness proof. Any failure drops
/// the request here, after the core call has ended.
fn inspect_request(
    inspector: &Inspector,
    spec: &InspectorSpec,
    max_output: usize,
    mut validated: ValidatedRequest,
    metrics: Option<&Metrics>,
) -> Result<(ValidatedRequest, CompleteInspection), BoundaryError> {
    // `model` is a validated identifier that is never rewritten; any finding rejects.
    inspector.reject_if_findings(validated.chat().model())?;
    let expected_redact = validated.chat().redactable_count();
    let expected_all = validated.chat().text_count();
    let mut scope = RequestScope::new(spec);
    let mut failure: Option<CoreBridgeError> = None;
    let mut visited = 0_usize;
    validated.chat_mut().for_each_text_mut(|slot, text| {
        if failure.is_some() {
            return;
        }
        visited = visited.saturating_add(1);
        match slot.mode() {
            // Structural labels (identifiers, keys, enum values) are never rewritten:
            // any finding blocks the request (ADR 0025).
            SlotMode::DetectOnly => {
                if let Err(e) = inspector.reject_if_findings(text) {
                    failure = Some(e);
                }
            }
            SlotMode::Redact => match inspector.inspect_text(&mut scope, text) {
                Ok(redacted) => *text = redacted.into_string(),
                Err(e) => failure = Some(e),
            },
        }
    });
    if let Some(e) = failure {
        return Err(e.into());
    }
    // Every allowed text must have been inspected, no more and no fewer.
    if visited != expected_all || scope.summary().leaves != expected_redact {
        return Err(CoreBridgeError::Incomplete.into());
    }
    // Replacement can break bounds or derived structure; fail closed before serializing.
    validated.chat().revalidate().map_err(|e| match e {
        SerializeError::Limit => BoundaryError::OutputLimit,
        SerializeError::Invalid => BoundaryError::Serialization,
    })?;
    let bound = max_output.min(reserved_bytes(&validated));
    let serializing = std::time::Instant::now();
    let output = validated
        .chat()
        .serialize_bounded(bound)
        .map_err(|e| match e {
            SerializeError::Limit => BoundaryError::OutputLimit,
            SerializeError::Invalid => BoundaryError::Serialization,
        })?;
    if let Some(m) = metrics {
        m.record(Stage::Serialization, serializing.elapsed());
    }
    let inspection = scope.finish(output)?;
    Ok((validated, inspection))
}

/// Bytes of the memory reservation the request holds.
fn reserved_bytes(validated: &ValidatedRequest) -> usize {
    usize::try_from(validated.memory().units())
        .unwrap_or(usize::MAX)
        .saturating_mul(MEMORY_UNIT_BYTES)
}

/// Approve a validated request whose core inspection completed.
///
/// The output must be non-empty and fit the memory already reserved for the request; the
/// reservation moves into the sanitized value and is released only when it is dropped.
///
/// # Errors
/// [`BoundaryError::OutputLimit`] when the output is empty or exceeds the reservation.
pub fn approve(
    validated: ValidatedRequest,
    inspection: CompleteInspection,
    route: RouteId,
) -> Result<SanitizedRequest, BoundaryError> {
    let output = inspection.into_output();
    if output.is_empty() || output.len() > reserved_bytes(&validated) {
        return Err(BoundaryError::OutputLimit);
    }
    Ok(SanitizedRequest::new(
        output,
        route,
        validated.into_memory(),
    ))
}

/// A re-closable barrier a started inspection job parks on (test-only).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod test_gate {
    use std::sync::{Condvar, Mutex};

    use tokio::sync::Semaphore;

    /// Jobs call [`Gate::enter`] on the worker thread; the test awaits [`Gate::entered`]
    /// for each, then [`Gate::open`]s the gate. Events, not sleeps.
    #[derive(Debug)]
    pub(crate) struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
        entered: Semaphore,
    }

    impl Gate {
        pub(crate) fn new_closed() -> Self {
            Self {
                open: Mutex::new(false),
                cv: Condvar::new(),
                entered: Semaphore::new(0),
            }
        }

        /// Worker side: announce the start, then block until the gate is open.
        pub(crate) fn enter(&self) {
            self.entered.add_permits(1);
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.cv.wait(open).unwrap();
            }
        }

        /// Test side: wait until one more job has started and parked.
        pub(crate) async fn entered(&self) {
            self.entered.acquire().await.unwrap().forget();
        }

        pub(crate) fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }

        pub(crate) fn close(&self) {
            *self.open.lock().unwrap() = false;
        }
    }
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
        // 4 units of 1 KiB: the bound is bytes, not units.
        let (_a, v) = validated(4);
        let err = approve(
            v,
            CompleteInspection::for_test(vec![0; 4 * 1024 + 1]),
            RouteId::new("r"),
        );
        assert_eq!(err.unwrap_err(), BoundaryError::OutputLimit);
        let (_a, v) = validated(4);
        let ok = approve(
            v,
            CompleteInspection::for_test(vec![b'x'; 4 * 1024]),
            RouteId::new("r"),
        );
        assert!(ok.is_ok());
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

    #[test]
    fn boundary_errors_map_to_fixed_safe_codes() {
        assert_eq!(BoundaryError::OutputLimit.code(), SafeCode::LimitExceeded);
        assert_eq!(
            BoundaryError::Serialization.code(),
            SafeCode::UnsupportedInput
        );
        for (error, code) in [
            (CoreBridgeError::LimitExceeded, SafeCode::LimitExceeded),
            (CoreBridgeError::Blocked, SafeCode::UnsupportedInput),
            (CoreBridgeError::Warned, SafeCode::UnsupportedInput),
            (CoreBridgeError::Incomplete, SafeCode::IncompleteInspection),
            (CoreBridgeError::Overload, SafeCode::Overload),
        ] {
            assert_eq!(BoundaryError::Core(error).code(), code);
        }
    }
}
