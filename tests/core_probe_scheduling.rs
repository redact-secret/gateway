//! Cancellation and capacity-ownership probe for the offload candidate (issue #5, ADR 0004).
//!
//! The pinned core cannot be interrupted, so the tests use a controllable job that blocks a
//! worker the way a long core call would. They prove that capacity returns only on real
//! completion, never when the awaiting future is dropped. All inputs are synthetic.

// Test helpers may use expect() and plain arithmetic; production code may not (clippy.toml
// covers only #[test] functions).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use redact_secret_gateway::admission::{Admission, AdmissionError, CapacityPlan};
use redact_secret_gateway::core_bridge::pool::InspectionPool;
use redact_secret_gateway::core_bridge::{CoreBridgeError, InspectorSpec, RequestScope};

fn nz32(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("nonzero")
}

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("nonzero")
}

/// Synthetic capacities for the probe only. These are not recommended defaults.
fn admission(inspection: u32, memory: u32) -> Admission {
    Admission::new(&CapacityPlan::new(
        nz32(8),
        nz32(memory),
        nz32(inspection),
        nz32(1),
        nz32(1),
    ))
}

fn pool(workers: usize, queue: usize) -> InspectionPool {
    let spec = InspectorSpec::new("full", &[], 1 << 20, 1000).expect("spec");
    InspectionPool::start(&spec, nz(workers), nz(queue)).expect("pool")
}

/// A job that holds its worker until `release` is signalled, like a long core call.
struct Gate {
    started: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

fn gated_job() -> (
    impl FnOnce(&redact_secret_gateway::core_bridge::Inspector) -> u32 + Send + 'static,
    Gate,
) {
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let job = move |_: &redact_secret_gateway::core_bridge::Inspector| {
        let _ = started_tx.send(());
        let _ = release_rx.recv();
        7
    };
    (job, Gate { started, release })
}

fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn dropping_the_awaiter_does_not_return_capacity_while_the_job_runs() {
    let admission = admission(1, 10);
    let pool = pool(1, 1);
    let permit = admission.try_inspection().expect("permit");
    let memory = admission.try_reserve_memory(10).expect("memory");
    let (job, gate) = gated_job();
    let handle = pool.submit_with(permit, memory, job).expect("submitted");
    gate.started.recv().expect("job started");

    // Cancel: the HTTP future is gone. The core call is still running.
    drop(handle);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        admission.try_inspection().unwrap_err(),
        AdmissionError::Overload,
        "inspection capacity must stay held"
    );
    assert_eq!(
        admission.try_reserve_memory(1).unwrap_err(),
        AdmissionError::Overload,
        "memory reservation must stay held"
    );

    // Real completion returns both.
    gate.release.send(()).expect("release");
    assert!(eventually(|| admission.try_inspection().is_ok()));
    assert!(eventually(|| admission.try_reserve_memory(10).is_ok()));
}

#[test]
fn a_cancelled_result_is_discarded_and_never_delivered() {
    let admission = admission(2, 20);
    let pool = pool(1, 1);
    let permit = admission.try_inspection().expect("permit");
    let memory = admission.try_reserve_memory(10).expect("memory");
    let dropped = Arc::new(AtomicUsize::new(0));
    let witness = Arc::new(());
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let seen = Arc::clone(&witness);
    let counter = Arc::clone(&dropped);
    let handle = pool
        .submit_with(permit, memory, move |_| {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            // The value this job produces. If it were delivered, the awaiter could forward it.
            Token { seen, counter }
        })
        .expect("submitted");
    started.recv().expect("started");
    drop(handle);
    release.send(()).expect("release");
    // The produced value is dropped by the worker: nobody could receive it.
    assert!(eventually(|| dropped.load(Ordering::SeqCst) == 1));
    assert_eq!(Arc::strong_count(&witness), 1);
}

struct Token {
    seen: Arc<()>,
    counter: Arc<AtomicUsize>,
}

impl Drop for Token {
    fn drop(&mut self) {
        let _ = &self.seen;
        self.counter.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_queued_job_cancelled_before_start_never_runs_and_returns_capacity() {
    let admission = admission(2, 20);
    let pool = pool(1, 1);
    let (running, gate) = gated_job();
    let first = pool
        .submit_with(
            admission.try_inspection().expect("p1"),
            admission.try_reserve_memory(10).expect("m1"),
            running,
        )
        .expect("first");
    gate.started.recv().expect("first started");

    let ran = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&ran);
    let queued = pool
        .submit_with(
            admission.try_inspection().expect("p2"),
            admission.try_reserve_memory(10).expect("m2"),
            move |_| {
                flag.fetch_add(1, Ordering::SeqCst);
            },
        )
        .expect("queued");
    // Both permits and all memory are in use by the running and the queued job.
    assert!(admission.try_inspection().is_err());
    drop(queued);

    // The cancelled queued job still occupies its (bounded) slot until a worker dequeues it.
    assert!(admission.try_inspection().is_err());
    gate.release.send(()).expect("release");
    assert_eq!(first.wait_blocking().expect("first result"), 7);
    assert!(eventually(|| admission.try_inspection().is_ok()));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "cancelled job must not run");
}

#[test]
fn repeated_cancelled_requests_cannot_exceed_concurrency_or_reservations() {
    const INSPECTION: u32 = 2;
    const MEMORY_PER_JOB: u32 = 10;
    let admission = admission(INSPECTION, MEMORY_PER_JOB * INSPECTION);
    let pool = pool(2, 2);
    let running_now = Arc::new(AtomicUsize::new(0));
    let high_water = Arc::new(AtomicUsize::new(0));
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let mut accepted = 0_usize;
    let mut refused = 0_usize;

    for _ in 0..200 {
        let Ok(permit) = admission.try_inspection() else {
            refused += 1;
            continue;
        };
        let Ok(memory) = admission.try_reserve_memory(MEMORY_PER_JOB) else {
            refused += 1;
            continue;
        };
        let now = Arc::clone(&running_now);
        let high = Arc::clone(&high_water);
        let gate = Arc::clone(&release_rx);
        let (started_tx, started) = mpsc::channel();
        let handle = pool.submit_with(permit, memory, move |_| {
            let current = now.fetch_add(1, Ordering::SeqCst) + 1;
            high.fetch_max(current, Ordering::SeqCst);
            let _ = started_tx.send(());
            if let Ok(rx) = gate.lock() {
                let _ = rx.recv();
            }
            now.fetch_sub(1, Ordering::SeqCst);
        });
        match handle {
            Ok(handle) => {
                accepted += 1;
                started.recv().expect("started");
                // The client disconnects while the core call is running.
                drop(handle);
            }
            Err(_) => refused += 1,
        }
    }
    // Both workers picked up a job and block; every further attempt is refused by admission
    // because the cancelled, still-running jobs hold their permits.
    assert_eq!(accepted, INSPECTION as usize);
    assert_eq!(refused, 200 - accepted);
    assert!(high_water.load(Ordering::SeqCst) <= INSPECTION as usize);
    assert!(admission.try_reserve_memory(1).is_err());

    // Release everything; capacity comes back only after real completion.
    for _ in 0..INSPECTION {
        release.send(()).expect("release");
    }
    assert!(eventually(|| {
        admission.try_inspection().is_ok()
            && admission
                .try_reserve_memory(MEMORY_PER_JOB * INSPECTION)
                .is_ok()
    }));
}

#[test]
fn a_full_queue_refuses_and_returns_the_permits_it_never_started() {
    let admission = admission(3, 30);
    let pool = pool(1, 1);
    let (running, gate) = gated_job();
    let first = pool
        .submit_with(
            admission.try_inspection().expect("p1"),
            admission.try_reserve_memory(10).expect("m1"),
            running,
        )
        .expect("first");
    gate.started.recv().expect("started");
    let second = pool
        .submit_with(
            admission.try_inspection().expect("p2"),
            admission.try_reserve_memory(10).expect("m2"),
            |_| (),
        )
        .expect("queued");
    // Queue (capacity 1) is full: refusal drops the permits immediately.
    let refused = pool.submit_with(
        admission.try_inspection().expect("p3"),
        admission.try_reserve_memory(10).expect("m3"),
        |_| (),
    );
    assert_eq!(refused.unwrap_err(), CoreBridgeError::Overload);
    assert!(
        admission.try_inspection().is_ok(),
        "refused job released its permit"
    );
    gate.release.send(()).expect("release");
    assert_eq!(first.wait_blocking().expect("first"), 7);
    second.wait_blocking().expect("second");
}

#[test]
fn a_panicking_job_returns_capacity_and_the_worker_survives() {
    let admission = admission(1, 10);
    let pool = pool(1, 1);
    let handle = pool
        .submit_with(
            admission.try_inspection().expect("p"),
            admission.try_reserve_memory(10).expect("m"),
            |_| -> u32 { std::panic::resume_unwind(Box::new("synthetic worker failure")) },
        )
        .expect("submitted");
    assert_eq!(
        handle.wait_blocking().unwrap_err(),
        CoreBridgeError::Incomplete
    );
    assert!(eventually(|| admission.try_inspection().is_ok()));
    // The same single worker still serves jobs.
    let (permit, memory) = (
        admission.try_inspection().expect("p"),
        admission.try_reserve_memory(10).expect("m"),
    );
    let ok = pool
        .submit_with(permit, memory, |_| 5_u32)
        .expect("submitted");
    assert_eq!(ok.wait_blocking().expect("result"), 5);
}

#[test]
fn shutdown_rejects_queued_work_waits_for_started_work_and_reports_abandonment() {
    let admission = admission(2, 20);
    let pool = pool(1, 1);
    let (running, gate) = gated_job();
    let first = pool
        .submit_with(
            admission.try_inspection().expect("p1"),
            admission.try_reserve_memory(10).expect("m1"),
            running,
        )
        .expect("first");
    gate.started.recv().expect("started");
    let queued = pool
        .submit_with(
            admission.try_inspection().expect("p2"),
            admission.try_reserve_memory(10).expect("m2"),
            |_| 1_u32,
        )
        .expect("queued");

    // Deadline expires while the uninterruptible job is still running: abandoned, reported.
    let report = pool.shutdown(Duration::from_millis(100));
    assert!(!report.drained);
    assert_eq!(report.abandoned_workers, 1);
    // The started job still holds its capacity: the process would exit, nothing was returned.
    assert!(admission.try_inspection().is_err());

    // Letting the job finish after shutdown: its result is still produced for a live awaiter,
    // the queued job was rejected (never ran), and everything is returned.
    gate.release.send(()).expect("release");
    assert_eq!(first.wait_blocking().expect("started job completes"), 7);
    assert_eq!(
        queued.wait_blocking().unwrap_err(),
        CoreBridgeError::Incomplete
    );
    assert!(eventually(|| admission.try_inspection().is_ok()));
}

#[test]
fn shutdown_with_idle_workers_drains_immediately() {
    let pool = pool(3, 3);
    let report = pool.shutdown(Duration::from_secs(5));
    assert!(report.drained);
    assert_eq!(report.abandoned_workers, 0);
}

#[test]
fn inspection_runs_on_the_worker_with_request_scope_returned() {
    let admission = admission(1, 10);
    let spec = InspectorSpec::new("full", &[], 1 << 20, 1000).expect("spec");
    let pool = InspectionPool::start(&spec, nz(1), nz(1)).expect("pool");
    let scope = RequestScope::new(&spec);
    let text = format!("k=ghp_SYNTHETICREVOKED{:020}", 61_u32);
    let handle = pool
        .submit_inspect(
            admission.try_inspection().expect("p"),
            admission.try_reserve_memory(10).expect("m"),
            scope,
            text,
        )
        .expect("submitted");
    let (scope, outcome) = handle.wait_blocking().expect("completed");
    assert_eq!(outcome.expect("inspected").as_str(), "k=<SECRET_1>");
    assert_eq!(scope.summary().redactions, 1);
    assert!(scope.finish(b"{}".to_vec()).is_ok());
}

#[test]
fn each_worker_builds_its_own_registry_concurrently() {
    // Four workers released together all complete: no shared registry or lock serializes them.
    let admission = admission(4, 40);
    let spec = InspectorSpec::new("full", &[], 1 << 20, 1000).expect("spec");
    let pool = InspectionPool::start(&spec, nz(4), nz(4)).expect("pool");
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4_u32)
        .map(|_| {
            let b = Arc::clone(&barrier);
            pool.submit_with(
                admission.try_inspection().expect("p"),
                admission.try_reserve_memory(10).expect("m"),
                move |_| {
                    // Reaches the barrier only if all four jobs run at the same time.
                    b.wait();
                    1_u32
                },
            )
            .expect("submitted")
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.wait_blocking().expect("done"), 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn awaiting_and_dropping_a_handle_in_async_code_behaves_the_same() {
    let admission = admission(1, 10);
    let pool = pool(1, 1);
    let (job, gate) = gated_job();
    let handle = pool
        .submit_with(
            admission.try_inspection().expect("p"),
            admission.try_reserve_memory(10).expect("m"),
            job,
        )
        .expect("submitted");
    gate.started.recv().expect("started");

    // A timeout cancels the await; the reactor thread is not blocked by the worker.
    let waited = tokio::time::timeout(Duration::from_millis(30), handle).await;
    assert!(waited.is_err(), "timed out while the job is still running");
    assert!(
        admission.try_inspection().is_err(),
        "capacity still held after timeout"
    );

    gate.release.send(()).expect("release");
    assert!(eventually(|| admission.try_inspection().is_ok()));
}
