//! Resource admission and permit ownership (ADR 0003, resource-limits contract).
//!
//! Five independently bounded capacities, each a RAII permit tied to the resource it
//! guards (never to an HTTP future):
//!
//! | Permit | Guards |
//! | --- | --- |
//! | [`ReceiptPermit`] | connection / concurrent body receipt |
//! | [`MemoryReservation`] | original, parsed, and transformed buffers |
//! | [`InspectionPermit`] | CPU inspection concurrency |
//! | [`UpstreamPermit`] | upstream in-flight requests |
//! | [`StreamPermit`] | active response streams |
//!
//! Acquisition order is fixed to avoid deadlock: receipt, then memory, then inspection,
//! then upstream, then stream. A request never waits for an earlier class while holding a
//! later one. This scaffold never waits at all: every acquisition is `try_*` and overload
//! is an immediate typed error, so there is no queue to grow. Bounded waiting with
//! deadlines is added in #18 and verified by #6.
//!
//! No capacity has a default. [`CapacityPlan`] needs every number from the caller; the
//! values come from validated configuration (#4) and measurements (#5, ADR 0008).

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::telemetry::SafeCode;

/// Capacity of each class, in permits. Memory is counted in caller-defined units
/// (granularity is chosen with #5/#18). No `Default`.
#[derive(Clone, Copy, Debug)]
pub struct CapacityPlan {
    receipt: NonZeroU32,
    memory_units: NonZeroU32,
    inspection: NonZeroU32,
    upstream: NonZeroU32,
    stream: NonZeroU32,
}

impl CapacityPlan {
    #[must_use]
    pub const fn new(
        receipt: NonZeroU32,
        memory_units: NonZeroU32,
        inspection: NonZeroU32,
        upstream: NonZeroU32,
        stream: NonZeroU32,
    ) -> Self {
        Self {
            receipt,
            memory_units,
            inspection,
            upstream,
            stream,
        }
    }
}

/// Typed admission failure. Carries no request content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionError {
    /// The class (or the aggregate memory budget) has no free capacity.
    Overload,
    /// A zero-sized or oversized reservation request.
    InvalidReservation,
}

impl AdmissionError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::Overload => SafeCode::Overload,
            Self::InvalidReservation => SafeCode::LimitExceeded,
        }
    }
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for AdmissionError {}

/// Permit for connection and body receipt.
#[derive(Debug)]
pub struct ReceiptPermit {
    _permit: OwnedSemaphorePermit,
}

/// Permit for inspection CPU. Ownership transfers to the worker for a job and ends only
/// when the job really finishes (ADR 0004).
#[derive(Debug)]
pub struct InspectionPermit {
    _permit: OwnedSemaphorePermit,
}

/// Permit for one in-flight upstream request.
#[derive(Debug)]
pub struct UpstreamPermit {
    _permit: OwnedSemaphorePermit,
}

/// Permit for one active response stream, held through stream cleanup.
#[derive(Debug)]
pub struct StreamPermit {
    _permit: OwnedSemaphorePermit,
}

/// Reservation against the aggregate memory budget, held while the buffers stay live.
#[derive(Debug)]
pub struct MemoryReservation {
    units: u32,
    _permit: OwnedSemaphorePermit,
}

impl MemoryReservation {
    #[must_use]
    pub const fn units(&self) -> u32 {
        self.units
    }
}

/// Owner of the five capacities. Cheap to share by reference; holds no request state.
#[derive(Debug)]
pub struct Admission {
    receipt: Arc<Semaphore>,
    memory: Arc<Semaphore>,
    inspection: Arc<Semaphore>,
    upstream: Arc<Semaphore>,
    stream: Arc<Semaphore>,
}

fn sized(n: NonZeroU32) -> Arc<Semaphore> {
    // `u32 -> usize` is lossless on every supported (>= 32-bit) target.
    Arc::new(Semaphore::new(
        usize::try_from(n.get()).unwrap_or(usize::MAX),
    ))
}

fn map_try(error: TryAcquireError) -> AdmissionError {
    match error {
        TryAcquireError::NoPermits | TryAcquireError::Closed => AdmissionError::Overload,
    }
}

impl Admission {
    #[must_use]
    pub fn new(plan: &CapacityPlan) -> Self {
        Self {
            receipt: sized(plan.receipt),
            memory: sized(plan.memory_units),
            inspection: sized(plan.inspection),
            upstream: sized(plan.upstream),
            stream: sized(plan.stream),
        }
    }

    /// Reserve receipt capacity and `memory_units` of memory *before* the body buffer is
    /// allocated. Do not trust `Content-Length`: reserve a conservative bounded size.
    ///
    /// # Errors
    /// [`AdmissionError::Overload`] when either class is exhausted;
    /// [`AdmissionError::InvalidReservation`] for zero units.
    pub fn begin_receipt(&self, memory_units: u32) -> Result<ReceiptTicket, AdmissionError> {
        let receipt = self.try_receipt()?;
        let memory = self.try_reserve_memory(memory_units)?;
        Ok(ReceiptTicket { receipt, memory })
    }

    /// # Errors
    /// [`AdmissionError::Overload`] when no receipt capacity is free.
    pub fn try_receipt(&self) -> Result<ReceiptPermit, AdmissionError> {
        let permit = Arc::clone(&self.receipt)
            .try_acquire_owned()
            .map_err(map_try)?;
        Ok(ReceiptPermit { _permit: permit })
    }

    /// # Errors
    /// [`AdmissionError::InvalidReservation`] for zero units;
    /// [`AdmissionError::Overload`] when the aggregate budget cannot cover `units`.
    pub fn try_reserve_memory(&self, units: u32) -> Result<MemoryReservation, AdmissionError> {
        if units == 0 {
            return Err(AdmissionError::InvalidReservation);
        }
        let permit = Arc::clone(&self.memory)
            .try_acquire_many_owned(units)
            .map_err(map_try)?;
        Ok(MemoryReservation {
            units,
            _permit: permit,
        })
    }

    /// # Errors
    /// [`AdmissionError::Overload`] when no inspection capacity is free.
    pub fn try_inspection(&self) -> Result<InspectionPermit, AdmissionError> {
        let permit = Arc::clone(&self.inspection)
            .try_acquire_owned()
            .map_err(map_try)?;
        Ok(InspectionPermit { _permit: permit })
    }

    /// # Errors
    /// [`AdmissionError::Overload`] when no upstream capacity is free.
    pub fn try_upstream(&self) -> Result<UpstreamPermit, AdmissionError> {
        let permit = Arc::clone(&self.upstream)
            .try_acquire_owned()
            .map_err(map_try)?;
        Ok(UpstreamPermit { _permit: permit })
    }

    /// # Errors
    /// [`AdmissionError::Overload`] when no stream capacity is free.
    pub fn try_stream(&self) -> Result<StreamPermit, AdmissionError> {
        let permit = Arc::clone(&self.stream)
            .try_acquire_owned()
            .map_err(map_try)?;
        Ok(StreamPermit { _permit: permit })
    }
}

/// Capacity reserved for one request body that has not been received yet.
#[derive(Debug)]
pub struct ReceiptTicket {
    receipt: ReceiptPermit,
    memory: MemoryReservation,
}

impl ReceiptTicket {
    /// Attach the received bytes. The body must fit the reservation made up front.
    ///
    /// # Errors
    /// [`AdmissionError::InvalidReservation`] if `body` exceeds the reserved units.
    pub fn complete(self, body: Vec<u8>) -> Result<ReceivedRequest, AdmissionError> {
        let fits = u32::try_from(body.len()).is_ok_and(|len| len <= self.memory.units());
        if !fits {
            return Err(AdmissionError::InvalidReservation);
        }
        Ok(ReceivedRequest {
            body,
            receipt: self.receipt,
            memory: self.memory,
        })
    }
}

/// A complete bounded body received on an admitted route. Nothing is parsed or trusted.
/// No path from this type to a transport call exists (request-state contract).
pub struct ReceivedRequest {
    body: Vec<u8>,
    receipt: ReceiptPermit,
    memory: MemoryReservation,
}

impl ReceivedRequest {
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    /// Split into the memory reservation (which the next state must keep alive for as
    /// long as any derived buffer lives) and the receipt permit; the body is dropped.
    #[expect(dead_code, reason = "consumed by #18/#19 when validation succeeds")]
    pub(crate) fn into_reservation(self) -> (MemoryReservation, ReceiptPermit) {
        (self.memory, self.receipt)
    }
}

impl fmt::Debug for ReceivedRequest {
    /// Never prints body content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivedRequest")
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("test value is nonzero")
    }

    // Synthetic capacities for tests only; not defaults.
    fn admission() -> Admission {
        Admission::new(&CapacityPlan::new(nz(2), nz(10), nz(1), nz(1), nz(1)))
    }

    #[test]
    fn classes_are_independent() {
        let a = admission();
        let _inspection = a.try_inspection().expect("inspection free");
        assert_eq!(a.try_inspection().unwrap_err(), AdmissionError::Overload);
        // Other classes are unaffected by an exhausted inspection class.
        assert!(a.try_upstream().is_ok());
        assert!(a.try_stream().is_ok());
        assert!(a.try_receipt().is_ok());
    }

    #[test]
    fn memory_is_aggregate_and_released_on_drop() {
        let a = admission();
        let first = a.try_reserve_memory(6).expect("fits");
        assert_eq!(
            a.try_reserve_memory(5).unwrap_err(),
            AdmissionError::Overload
        );
        drop(first);
        assert!(a.try_reserve_memory(5).is_ok());
    }

    #[test]
    fn zero_and_oversized_reservations_are_rejected() {
        let a = admission();
        assert_eq!(
            a.try_reserve_memory(0).unwrap_err(),
            AdmissionError::InvalidReservation
        );
        assert_eq!(
            a.try_reserve_memory(11).unwrap_err(),
            AdmissionError::Overload
        );
    }

    #[test]
    fn ticket_rejects_body_larger_than_reservation() {
        let a = admission();
        let ticket = a.begin_receipt(4).expect("reserve");
        let err = ticket.complete(vec![0; 5]).unwrap_err();
        assert_eq!(err, AdmissionError::InvalidReservation);
        // Failure path released everything.
        assert!(a.begin_receipt(10).is_ok());
    }

    #[test]
    fn received_request_debug_hides_body() {
        let a = admission();
        let received = a
            .begin_receipt(10)
            .expect("reserve")
            .complete(b"SYNTHETIC".to_vec())
            .expect("fits");
        let shown = format!("{received:?}");
        assert!(!shown.contains("SYNTHETIC"));
    }
}
