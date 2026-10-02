//! Capacity probes, a controllable non-interruptible inspection job, and acquisition-order
//! tracking for permit tests (ADR 0003, ADR 0004).
//!
//! `Admission` is try-only today, so probes learn how much is free by acquiring and
//! releasing. Probes are only meaningful at quiescent points (no concurrent acquirers).
//!
//! The job here is a *reference worker*: it models what the real inspection worker (#5,
//! #18) must do (own the permits until the work really ends). Tests built on it pin the
//! contract; when the real worker exists, run the same scenarios against it by supplying
//! it through the same shape: `start(permits) -> (JobControl, waiter)`.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use redact_secret_gateway::admission::{
    Admission, CapacityPlan, InspectionPermit, MemoryReservation, ReceiptPermit,
};
use tokio::sync::oneshot;

/// The five independently bounded classes, in the fixed acquisition order (ADR 0003).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Receipt = 0,
    Memory = 1,
    Inspection = 2,
    Upstream = 3,
    Stream = 4,
}

pub const ALL_CLASSES: [Class; 5] = [
    Class::Receipt,
    Class::Memory,
    Class::Inspection,
    Class::Upstream,
    Class::Stream,
];

/// Synthetic capacities (not defaults; the gateway has none).
#[derive(Clone, Copy, Debug)]
pub struct Sizes {
    pub receipt: u32,
    pub memory: u32,
    pub inspection: u32,
    pub upstream: u32,
    pub stream: u32,
}

impl Sizes {
    #[must_use]
    pub fn of(self, class: Class) -> u32 {
        match class {
            Class::Receipt => self.receipt,
            Class::Memory => self.memory,
            Class::Inspection => self.inspection,
            Class::Upstream => self.upstream,
            Class::Stream => self.stream,
        }
    }
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("capacity must be nonzero")
}

/// An `Admission` plus the sizes it was built with, so tests can ask "how much is free".
pub struct Capacity {
    pub admission: Arc<Admission>,
    pub sizes: Sizes,
}

impl fmt::Debug for Capacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capacity")
            .field("sizes", &self.sizes)
            .finish()
    }
}

/// A capacity check failed. Counts only.
#[derive(Debug, PartialEq, Eq)]
pub struct CapacityViolation {
    pub class: Class,
    pub expected_free: u32,
    pub actual_free: u32,
}

impl fmt::Display for CapacityViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: expected {} free, found {} free",
            self.class, self.expected_free, self.actual_free
        )
    }
}

impl std::error::Error for CapacityViolation {}

impl Capacity {
    #[must_use]
    pub fn new(sizes: Sizes) -> Self {
        let plan = CapacityPlan::new(
            nz(sizes.receipt),
            nz(sizes.memory),
            nz(sizes.inspection),
            nz(sizes.upstream),
            nz(sizes.stream),
        );
        Self {
            admission: Arc::new(Admission::new(&plan)),
            sizes,
        }
    }

    /// Number of permits (memory: units) currently free in `class`.
    #[must_use]
    pub fn free(&self, class: Class) -> u32 {
        let a = &self.admission;
        match class {
            Class::Memory => (1..=self.sizes.memory)
                .rev()
                .find(|&n| a.try_reserve_memory(n).is_ok())
                .unwrap_or(0),
            Class::Receipt => count_until_err(|| a.try_receipt().ok()),
            Class::Inspection => count_until_err(|| a.try_inspection().ok()),
            Class::Upstream => count_until_err(|| a.try_upstream().ok()),
            Class::Stream => count_until_err(|| a.try_stream().ok()),
        }
    }

    /// Check that exactly `expected_free` is free in `class`.
    pub fn check_free(&self, class: Class, expected_free: u32) -> Result<(), CapacityViolation> {
        let actual_free = self.free(class);
        if actual_free == expected_free {
            Ok(())
        } else {
            Err(CapacityViolation {
                class,
                expected_free,
                actual_free,
            })
        }
    }

    /// Every class is back at full capacity: nothing leaked.
    pub fn check_all_free(&self) -> Result<(), CapacityViolation> {
        ALL_CLASSES
            .iter()
            .try_for_each(|&c| self.check_free(c, self.sizes.of(c)))
    }

    pub fn assert_all_free(&self) {
        if let Err(v) = self.check_all_free() {
            panic!("permit leak: {v}");
        }
    }
}

/// Acquire until failure, return how many succeeded, then release them all.
fn count_until_err<T>(mut acquire: impl FnMut() -> Option<T>) -> u32 {
    let mut held = Vec::new();
    while let Some(p) = acquire() {
        held.push(p);
    }
    u32::try_from(held.len()).unwrap_or(u32::MAX)
}

/// Result of a job. Never carries request content in these tests.
pub type JobResult = Vec<u8>;

/// Shared counters for jobs started from one [`JobControl`] family.
#[derive(Debug, Default)]
pub struct JobCounters {
    running: AtomicUsize,
    peak_running: AtomicUsize,
    finished: AtomicUsize,
    delivered: AtomicUsize,
    disposed: AtomicUsize,
}

impl JobCounters {
    #[must_use]
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
    #[must_use]
    pub fn peak_running(&self) -> usize {
        self.peak_running.load(Ordering::SeqCst)
    }
    #[must_use]
    pub fn finished(&self) -> usize {
        self.finished.load(Ordering::SeqCst)
    }
    /// Results handed to a live waiter.
    #[must_use]
    pub fn delivered(&self) -> usize {
        self.delivered.load(Ordering::SeqCst)
    }
    /// Results dropped because the waiter was gone (cancelled request).
    #[must_use]
    pub fn disposed(&self) -> usize {
        self.disposed.load(Ordering::SeqCst)
    }
}

/// Handle to a started, non-interruptible job. The job blocks a real OS thread until
/// [`JobControl::release`] is called; dropping the waiter or this handle does not stop it.
#[derive(Debug)]
pub struct JobControl {
    release: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    finished: Arc<AtomicBool>,
}

impl JobControl {
    /// Let the job finish, wait for the thread to end, and return once its permits have
    /// been dropped.
    pub fn release_and_join(&mut self) {
        self.release();
        self.join();
    }

    /// Signal the job to finish without waiting.
    pub fn release(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
    }

    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            t.join().expect("job thread panicked");
        }
    }

    /// The job body has completed (permits are dropped right after this flips).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

impl Drop for JobControl {
    /// Never leave a blocked thread behind if a test fails.
    fn drop(&mut self) {
        self.release();
        self.join();
    }
}

/// Permits a worker owns for one job (ADR 0004 invariant 1).
pub struct JobPermits {
    pub inspection: InspectionPermit,
    pub memory: MemoryReservation,
    pub receipt: ReceiptPermit,
}

/// Start a non-interruptible job that owns `permits` until it really finishes.
///
/// Returns the control handle and the waiter an HTTP handler would await. Dropping the
/// waiter models a client disconnect: the job keeps running and holding the permits, and
/// its result is disposed.
pub fn start_job(
    permits: JobPermits,
    counters: &Arc<JobCounters>,
    result: JobResult,
) -> (JobControl, oneshot::Receiver<JobResult>) {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (result_tx, result_rx) = oneshot::channel::<JobResult>();
    let finished = Arc::new(AtomicBool::new(false));
    let counters = Arc::clone(counters);
    let finished_flag = Arc::clone(&finished);

    let thread = std::thread::spawn(move || {
        // The worker owns the permits for the whole job.
        let _owned = permits;
        let now = counters.running.fetch_add(1, Ordering::SeqCst) + 1;
        counters.peak_running.fetch_max(now, Ordering::SeqCst);
        let _ = started_tx.send(());
        // Non-interruptible: only `release` ends it; a safety cap avoids hung CI.
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
        counters.running.fetch_sub(1, Ordering::SeqCst);
        counters.finished.fetch_add(1, Ordering::SeqCst);
        match result_tx.send(result) {
            Ok(()) => counters.delivered.fetch_add(1, Ordering::SeqCst),
            // Waiter gone: result is disposed and must never be forwarded.
            Err(_) => counters.disposed.fetch_add(1, Ordering::SeqCst),
        };
        finished_flag.store(true, Ordering::SeqCst);
        // `_owned` drops here, after the work and result disposal.
    });
    started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("job did not start");
    (
        JobControl {
            release: Some(release_tx),
            thread: Some(thread),
            finished,
        },
        result_rx,
    )
}

/// A deliberately **wrong** worker used as a negative control: its permits live in the
/// waiter, so dropping the waiter releases capacity while the job still runs. Tests use
/// it to prove the early-release check can fail.
pub struct BuggyWaiter {
    pub result: oneshot::Receiver<JobResult>,
    _permits: JobPermits,
}

/// Start a job whose permits are (incorrectly) tied to the waiter instead of the worker.
pub fn start_job_with_permits_tied_to_waiter(
    permits: JobPermits,
    counters: &Arc<JobCounters>,
) -> (JobControl, BuggyWaiter) {
    // The thread owns no permits, so capacity follows the waiter's lifetime.
    let dummy_sizes = Sizes {
        receipt: 1,
        memory: 1,
        inspection: 1,
        upstream: 1,
        stream: 1,
    };
    let dummy = Capacity::new(dummy_sizes);
    let owned = JobPermits {
        inspection: dummy.admission.try_inspection().expect("free"),
        memory: dummy.admission.try_reserve_memory(1).expect("free"),
        receipt: dummy.admission.try_receipt().expect("free"),
    };
    let (control, rx) = start_job(owned, counters, Vec::new());
    (
        control,
        BuggyWaiter {
            result: rx,
            _permits: permits,
        },
    )
}

/// A violation of the fixed acquisition order.
#[derive(Debug, PartialEq, Eq)]
pub struct OrderViolation {
    pub held: Class,
    pub acquiring: Class,
}

impl fmt::Display for OrderViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "acquired {:?} while holding later class {:?}",
            self.acquiring, self.held
        )
    }
}

/// Records the classes one request holds and rejects any acquisition that is earlier in
/// the fixed order than one already held (a request must never wait for an earlier class
/// while holding a later one). Tests wrap flow code with it; #18 should run its real
/// admission flow through the same tracker.
#[derive(Debug, Default)]
pub struct OrderTracker {
    held: Mutex<Vec<Class>>,
}

impl OrderTracker {
    pub fn note_acquire(&self, class: Class) -> Result<(), OrderViolation> {
        let mut held = self.held.lock().unwrap();
        if let Some(&max) = held.iter().max()
            && class < max
        {
            return Err(OrderViolation {
                held: max,
                acquiring: class,
            });
        }
        held.push(class);
        Ok(())
    }

    pub fn note_release(&self, class: Class) {
        let mut held = self.held.lock().unwrap();
        if let Some(i) = held.iter().rposition(|&c| c == class) {
            held.remove(i);
        }
    }
}
