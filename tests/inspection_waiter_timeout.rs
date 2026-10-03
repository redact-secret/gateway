//! A waiter that gives up (a deadline, not a disconnect) cannot return the permits of
//! synchronous inspection work that has already started (#58, ADR 0004, ADR 0027).
//!
//! The pinned core cannot be interrupted, so a job that is running holds its inspection
//! permit and memory reservation until it really ends. These tests use jobs that block a real
//! worker thread on a channel (no sleeps as synchronization: a job ends only when the test
//! releases it, and the end is observed by joining the pool's workers through `shutdown`),
//! wrap the wait in `tokio::time::timeout`, and check the real [`Admission`] counters.
//! All inputs are synthetic.

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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use redact_secret_gateway::admission::{Admission, AdmissionError, CapacityPlan};
use redact_secret_gateway::core_bridge::pool::InspectionPool;
use redact_secret_gateway::core_bridge::{Inspector, InspectorSpec};

fn nz32(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("nonzero")
}

/// Synthetic capacities for the test only; not recommended values.
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
    InspectionPool::start(
        &spec,
        NonZeroUsize::new(workers).expect("nonzero"),
        NonZeroUsize::new(queue).expect("nonzero"),
    )
    .expect("pool")
}

const GENEROUS: Duration = Duration::from_secs(30);
/// The waiter's deadline. The job it waits for cannot finish before the test releases it, so
/// this always elapses; its length only sets how long the test takes.
const WAITER_DEADLINE: Duration = Duration::from_millis(15);

#[tokio::test]
async fn a_waiter_that_times_out_does_not_return_the_permits_of_a_started_job() {
    let admission = admission(1, 10);
    let pool = pool(1, 1);
    let (started_tx, started) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let permit = admission.try_inspection().expect("permit");
    let memory = admission.try_reserve_memory(10).expect("memory");
    let handle = pool
        .submit_with(permit, memory, move |_: &Inspector| {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            7_u32
        })
        .expect("submitted");
    started.recv().expect("the worker is inside the job");

    // The waiter's deadline passes while the synchronous work runs; the future is dropped.
    let waited = tokio::time::timeout(WAITER_DEADLINE, handle).await;
    assert!(waited.is_err(), "the waiter timed out");

    // Nothing was returned: a new request is refused, the reservation is still held.
    assert_eq!(
        admission.try_inspection().unwrap_err(),
        AdmissionError::Overload
    );
    assert_eq!(
        admission.try_reserve_memory(1).unwrap_err(),
        AdmissionError::Overload
    );
    assert_eq!(admission.load().inspection_in_use, 1);
    assert_eq!(admission.load().memory_units_in_use, 10);

    // Real completion (observed by joining the worker) returns both.
    release.send(()).expect("release");
    let report = pool.shutdown(GENEROUS);
    assert!(report.drained, "the worker finished");
    assert_eq!(admission.load().inspection_in_use, 0);
    assert_eq!(admission.load().memory_units_in_use, 0);
    assert!(admission.try_inspection().is_ok());
    assert!(admission.try_reserve_memory(10).is_ok());
}

#[tokio::test]
async fn queued_jobs_whose_waiters_time_out_never_run_and_return_permits_when_skipped() {
    // One worker, three inspection permits: one job runs, two wait in the queue.
    let admission = admission(3, 30);
    let pool = pool(1, 2);
    let ran = Arc::new(AtomicUsize::new(0));
    let (started_tx, started) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));

    let first = pool
        .submit_with(
            admission.try_inspection().expect("permit"),
            admission.try_reserve_memory(10).expect("memory"),
            {
                let ran = Arc::clone(&ran);
                move |_: &Inspector| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    let _ = release_rx.lock().map(|rx| rx.recv());
                }
            },
        )
        .expect("submitted");
    started.recv().expect("the first job is running");

    let mut queued = Vec::new();
    for _ in 0..2 {
        queued.push(
            pool.submit_with(
                admission.try_inspection().expect("permit"),
                admission.try_reserve_memory(10).expect("memory"),
                {
                    let ran = Arc::clone(&ran);
                    move |_: &Inspector| {
                        ran.fetch_add(1, Ordering::SeqCst);
                    }
                },
            )
            .expect("queued"),
        );
    }
    assert_eq!(admission.load().inspection_in_use, 3);

    // Every waiter gives up. The queued jobs are cancelled, the running one cannot be.
    for handle in queued {
        assert!(tokio::time::timeout(WAITER_DEADLINE, handle).await.is_err());
    }
    assert!(
        tokio::time::timeout(WAITER_DEADLINE, first).await.is_err(),
        "the running job's waiter also timed out"
    );
    // A cancelled queued job holds its permit until a worker skips it, and the only worker is
    // busy: the full count is still held, and a new job is refused.
    assert_eq!(admission.load().inspection_in_use, 3);
    assert_eq!(admission.load().memory_units_in_use, 30);
    assert_eq!(
        admission.try_inspection().unwrap_err(),
        AdmissionError::Overload
    );

    // The running job ends; the worker then dequeues and skips both cancelled jobs.
    release.send(()).expect("release");
    let report = pool.shutdown(GENEROUS);
    assert!(report.drained);
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "the cancelled queued jobs never ran"
    );
    assert_eq!(admission.load().inspection_in_use, 0);
    assert_eq!(admission.load().memory_units_in_use, 0);
}

#[tokio::test]
async fn a_full_queue_refuses_and_every_rejected_attempt_returns_what_it_took() {
    let admission = admission(2, 20);
    let pool = pool(1, 1);
    let (started_tx, started) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let running = pool
        .submit_with(
            admission.try_inspection().expect("permit"),
            admission.try_reserve_memory(10).expect("memory"),
            move |_: &Inspector| {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            },
        )
        .expect("submitted");
    started.recv().expect("running");
    let queued = pool
        .submit_with(
            admission.try_inspection().expect("permit"),
            admission.try_reserve_memory(10).expect("memory"),
            |_: &Inspector| (),
        )
        .expect("queued");
    // Capacity is exhausted: further attempts are refused by admission, not queued, and
    // repeated refusals leave the counters exactly where they were.
    for _ in 0..100 {
        assert_eq!(
            admission.try_inspection().unwrap_err(),
            AdmissionError::Overload
        );
    }
    assert_eq!(admission.load().inspection_in_use, 2);
    assert_eq!(admission.load().memory_units_in_use, 20);
    drop(queued);
    drop(running);
    release.send(()).expect("release");
    assert!(pool.shutdown(GENEROUS).drained);
    assert_eq!(admission.load().inspection_in_use, 0);
    assert_eq!(admission.load().memory_units_in_use, 0);
}
