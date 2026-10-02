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
//! later one. Every acquisition is `try_*`, except body receipt on the Chat Completions
//! route ([`Admission::begin_body_receipt`], #18): when capacity is not free it waits in
//! a bounded queue ([`RequestLimits::admission_queue`]) for at most
//! [`RequestLimits::admission_wait_ms`], then fails with [`AdmissionError::Overload`].
//! Waiters hold only the receipt permit while waiting for memory (earlier class before
//! later class), so the order cannot deadlock.
//!
//! No capacity has a default. [`CapacityPlan`] needs every number from the caller; the
//! values come from validated configuration (#4) and measurements (#5, ADR 0008).
//! [`RequestLimits`] holds the per-request limits; its provisional values are justified
//! in `docs/contracts/resource-limits.md` and are not measured.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

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

impl CapacityPlan {
    /// Number of inspection permits (CPU jobs queued plus running).
    #[must_use]
    pub const fn inspection_permits(&self) -> NonZeroU32 {
        self.inspection
    }

    /// Number of active-stream permits. With `stream_buffer_bytes` this bounds the memory
    /// that streamed provider bytes can hold in the relay (#21).
    #[must_use]
    pub const fn stream_permits(&self) -> NonZeroU32 {
        self.stream
    }
}

/// Bytes per memory unit. A request reservation is counted in these.
pub const MEMORY_UNIT_BYTES: usize = 1024;

/// Conservative bytes charged per parsed node (value or key). `Json` is 32 bytes, an
/// object entry 56 bytes, plus `Vec` growth slack and the hash entry for key uniqueness.
pub const NODE_COST_BYTES: usize = 128;

/// Buffer copies charged per body byte: the received buffer, the decoded strings, the
/// transient duplicate-key sets, and the transformed output reserved for #19.
pub const BUFFER_COPIES: usize = 4;

/// Per-request limits. Every value is finite; none is measured yet (ADR 0008), see
/// `docs/contracts/resource-limits.md`. They compose with the aggregate budget: the body
/// a request may carry is clamped to what one reservation can ever cover
/// ([`RequestLimits::effective_max_body`]), so a configured per-request limit can never
/// exceed the global memory budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestLimits {
    /// Maximum request body bytes (after HTTP framing).
    pub max_body_bytes: u32,
    /// Maximum JSON container nesting.
    pub max_depth: u32,
    /// Maximum parsed values plus object keys.
    pub max_nodes: u32,
    /// Maximum decoded bytes of a single string or key.
    pub max_string_bytes: u32,
    /// Maximum number of `messages`.
    pub max_messages: u32,
    /// Longest wait for receipt/memory capacity before an overload rejection.
    pub admission_wait_ms: u32,
    /// Most requests allowed to wait for capacity at once; more fail immediately.
    pub admission_queue: u32,
    /// Total time allowed to receive the body once capacity is reserved.
    pub body_deadline_ms: u32,
    /// Longest time to establish the TCP connection and TLS session to the provider (#20).
    pub upstream_connect_ms: u32,
    /// Longest time from initiating the upstream send to the provider's response headers.
    /// A non-streamed completion is answered only when generation ends, so this is long.
    pub upstream_header_ms: u32,
    /// Longest time from initiating the upstream send to the last buffered response byte.
    pub upstream_total_ms: u32,
    /// Largest provider response header block accepted (names plus values), in bytes.
    pub max_response_header_bytes: u32,
    /// Largest provider response body buffered for relay, in bytes.
    pub max_response_body_bytes: u32,
    /// Longest graceful drain after a shutdown signal before in-flight work is cancelled.
    pub shutdown_drain_ms: u32,
    /// Longest wait for the next upstream chunk of an SSE stream (#21). Counted only while
    /// the Gateway is waiting on the provider, never while the downstream is slow.
    pub stream_idle_ms: u32,
    /// Longest total life of one SSE stream, from the send to the end of the relay (#21).
    pub stream_lifetime_ms: u32,
    /// Longest a pending response write may make no progress before the connection is
    /// closed (slow or absent consumer, #21). Applies to every response write.
    pub stream_write_stall_ms: u32,
    /// Most provider bytes one stream may hold in the relay at once, in bytes. A single
    /// upstream chunk larger than this terminates the stream (#21).
    pub stream_buffer_bytes: u32,
}

impl RequestLimits {
    /// Provisional values, pending quiet-host measurement.
    #[must_use]
    pub const fn provisional() -> Self {
        Self {
            max_body_bytes: 1_048_576,
            max_depth: 16,
            max_nodes: 16_384,
            max_string_bytes: 524_288,
            max_messages: 256,
            admission_wait_ms: 250,
            admission_queue: 16,
            body_deadline_ms: 10_000,
            upstream_connect_ms: 5_000,
            upstream_header_ms: 120_000,
            upstream_total_ms: 300_000,
            max_response_header_bytes: 32_768,
            max_response_body_bytes: 4_194_304,
            shutdown_drain_ms: 10_000,
            stream_idle_ms: 120_000,
            stream_lifetime_ms: 900_000,
            stream_write_stall_ms: 30_000,
            stream_buffer_bytes: 1_048_576,
        }
    }

    /// Memory charged up front for a body of at most `body_cap` bytes, in bytes. The
    /// parsed node count cannot exceed the body length, so small bodies reserve little.
    #[must_use]
    pub fn reservation_bytes(&self, body_cap: usize) -> usize {
        let nodes = usize::try_from(self.max_nodes)
            .unwrap_or(usize::MAX)
            .min(body_cap);
        body_cap
            .saturating_mul(BUFFER_COPIES)
            .saturating_add(nodes.saturating_mul(NODE_COST_BYTES))
    }

    /// [`Self::reservation_bytes`] rounded up to memory units, saturating at `u32::MAX`.
    #[must_use]
    pub fn reservation_units(&self, body_cap: usize) -> u32 {
        u32::try_from(self.reservation_bytes(body_cap).div_ceil(MEMORY_UNIT_BYTES))
            .unwrap_or(u32::MAX)
    }

    /// Largest body (at most `max_body_bytes`) whose reservation fits `total_units`. This
    /// is the composition rule between the per-request and the aggregate limits.
    #[must_use]
    pub fn effective_max_body(&self, total_units: u32) -> usize {
        let mut lo = 0_usize;
        let mut hi = usize::try_from(self.max_body_bytes).unwrap_or(usize::MAX);
        while lo < hi {
            let mid = lo.saturating_add(hi.saturating_sub(lo).saturating_add(1) / 2);
            if self.reservation_units(mid) <= total_units {
                lo = mid;
            } else {
                hi = mid.saturating_sub(1);
            }
        }
        lo
    }

    #[must_use]
    pub fn admission_wait(&self) -> Duration {
        Duration::from_millis(u64::from(self.admission_wait_ms))
    }

    #[must_use]
    pub fn body_deadline(&self) -> Duration {
        Duration::from_millis(u64::from(self.body_deadline_ms))
    }

    #[must_use]
    pub fn upstream_connect(&self) -> Duration {
        Duration::from_millis(u64::from(self.upstream_connect_ms))
    }

    #[must_use]
    pub fn upstream_header(&self) -> Duration {
        Duration::from_millis(u64::from(self.upstream_header_ms))
    }

    #[must_use]
    pub fn upstream_total(&self) -> Duration {
        Duration::from_millis(u64::from(self.upstream_total_ms))
    }

    #[must_use]
    pub fn shutdown_drain(&self) -> Duration {
        Duration::from_millis(u64::from(self.shutdown_drain_ms))
    }

    #[must_use]
    pub fn stream_idle(&self) -> Duration {
        Duration::from_millis(u64::from(self.stream_idle_ms))
    }

    #[must_use]
    pub fn stream_lifetime(&self) -> Duration {
        Duration::from_millis(u64::from(self.stream_lifetime_ms))
    }

    #[must_use]
    pub fn stream_write_stall(&self) -> Duration {
        Duration::from_millis(u64::from(self.stream_write_stall_ms))
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

/// Permit for one active response stream, held through stream cleanup (#21: owned by the
/// streaming response body from the response headers until the stream ends or is dropped).
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
    memory_total: u32,
    waiting: Arc<AtomicU32>,
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
            memory_total: plan.memory_units.get(),
            waiting: Arc::new(AtomicU32::new(0)),
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
        // Low-level form: one unit is one body byte.
        let body_cap = usize::try_from(memory_units).unwrap_or(usize::MAX);
        self.begin_receipt_sized(memory_units, body_cap)
    }

    fn begin_receipt_sized(
        &self,
        memory_units: u32,
        body_cap: usize,
    ) -> Result<ReceiptTicket, AdmissionError> {
        let receipt = self.try_receipt()?;
        let memory = self.try_reserve_memory(memory_units)?;
        Ok(ReceiptTicket {
            receipt,
            memory,
            body_cap,
        })
    }

    /// Total aggregate memory budget in units.
    #[must_use]
    pub const fn memory_total_units(&self) -> u32 {
        self.memory_total
    }

    /// Reserve receipt capacity and the conservative memory for a body of at most
    /// `body_cap` bytes **before** any body byte is collected (ADR 0003). The reservation
    /// comes from `limits`, never from a declared length alone: callers pass a declared
    /// length only as an upper bound that collection then enforces.
    ///
    /// When capacity is not free the call joins a bounded wait queue and gives up after
    /// `limits.admission_wait()`; when the queue is full it fails immediately.
    ///
    /// # Errors
    /// [`AdmissionError::InvalidReservation`] when `body_cap` is zero or its reservation
    /// can never fit the aggregate budget; [`AdmissionError::Overload`] when capacity is
    /// not available in time or the queue is full.
    pub async fn begin_body_receipt(
        &self,
        body_cap: usize,
        limits: &RequestLimits,
    ) -> Result<ReceiptTicket, AdmissionError> {
        let units = limits.reservation_units(body_cap);
        if body_cap == 0 || units == 0 || units > self.memory_total {
            return Err(AdmissionError::InvalidReservation);
        }
        match self.begin_receipt_sized(units, body_cap) {
            Err(AdmissionError::Overload) => {}
            other => return other,
        }
        let _slot = WaitSlot::enter(&self.waiting, limits.admission_queue)?;
        let acquire = async {
            let receipt = Arc::clone(&self.receipt)
                .acquire_owned()
                .await
                .map_err(|_| AdmissionError::Overload)?;
            let memory = Arc::clone(&self.memory)
                .acquire_many_owned(units)
                .await
                .map_err(|_| AdmissionError::Overload)?;
            Ok::<_, AdmissionError>((receipt, memory))
        };
        let (receipt, memory) = tokio::time::timeout(limits.admission_wait(), acquire)
            .await
            .map_err(|_| AdmissionError::Overload)??;
        Ok(ReceiptTicket {
            receipt: ReceiptPermit { _permit: receipt },
            memory: MemoryReservation {
                units,
                _permit: memory,
            },
            body_cap,
        })
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

/// Slot in the bounded admission wait queue. Released on drop.
struct WaitSlot {
    waiting: Arc<AtomicU32>,
}

impl WaitSlot {
    fn enter(waiting: &Arc<AtomicU32>, queue_cap: u32) -> Result<Self, AdmissionError> {
        waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < queue_cap).then(|| n.saturating_add(1))
            })
            .map_err(|_| AdmissionError::Overload)?;
        Ok(Self {
            waiting: Arc::clone(waiting),
        })
    }
}

impl Drop for WaitSlot {
    fn drop(&mut self) {
        self.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Capacity reserved for one request body that has not been received yet.
#[derive(Debug)]
pub struct ReceiptTicket {
    receipt: ReceiptPermit,
    memory: MemoryReservation,
    body_cap: usize,
}

impl ReceiptTicket {
    /// Most body bytes the reservation covers. Collection must stop at this bound.
    #[must_use]
    pub const fn body_cap(&self) -> usize {
        self.body_cap
    }

    /// Attach the received bytes. The body must fit the reservation made up front.
    ///
    /// # Errors
    /// [`AdmissionError::InvalidReservation`] if `body` exceeds the reserved body cap.
    pub fn complete(self, body: Vec<u8>) -> Result<ReceivedRequest, AdmissionError> {
        if body.len() > self.body_cap {
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
