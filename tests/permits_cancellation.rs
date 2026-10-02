//! Cancellation and non-interruptible inspection tests (ADR 0004; issue #6).
//!
//! Dropping an async future does not stop synchronous work that already started. If the
//! HTTP waiter owned the CPU and memory permits, dropping it would return capacity while
//! the work still ran, and repeated cancelled requests could exceed the real limits.
//! These tests use a controllable reference worker (`support::permits::start_job`) that
//! models the required ownership: the *worker* owns the permits until the job really
//! finishes. When the real inspection worker exists (#5/#18), run the same scenarios
//! through it.
//!
//! Not covered here because the code does not exist yet: removing a queued (not started)
//! job on cancellation, the pre-upstream cancellation check, and shutdown deadlines (#18).

mod support;

use std::sync::Arc;
use std::time::Duration;

use redact_secret_gateway::admission::AdmissionError;
use support::fake_upstream::{Behavior, FakeUpstream};
use support::permits::{
    Capacity, Class, JobCounters, JobPermits, Sizes, start_job,
    start_job_with_permits_tied_to_waiter,
};
use support::raw_http::{exchange, post};

fn sizes() -> Sizes {
    Sizes {
        receipt: 4,
        memory: 40,
        inspection: 2,
        upstream: 2,
        stream: 2,
    }
}

fn permits(cap: &Capacity, units: u32) -> Result<JobPermits, AdmissionError> {
    let receipt = cap.admission.try_receipt()?;
    let memory = cap.admission.try_reserve_memory(units)?;
    let inspection = cap.admission.try_inspection()?;
    Ok(JobPermits {
        inspection,
        memory,
        receipt,
    })
}

#[tokio::test]
async fn dropping_the_http_waiter_does_not_release_running_job_permits() {
    let cap = Capacity::new(sizes());
    let counters = Arc::new(JobCounters::default());
    let (mut job, rx) = start_job(permits(&cap, 10).unwrap(), &counters, b"result".to_vec());

    // The "HTTP handler" awaits the job result; the client disconnects and the handler
    // future is dropped while the job is still running.
    let handler = tokio::spawn(async move { rx.await.ok() });
    handler.abort();
    let _ = handler.await;

    assert_eq!(
        counters.running(),
        1,
        "job keeps running after the waiter is gone"
    );
    assert!(!job.is_finished());
    cap.check_free(Class::Inspection, cap.sizes.inspection - 1)
        .expect("inspection permit released early");
    cap.check_free(Class::Memory, cap.sizes.memory - 10)
        .expect("memory reservation released early");
    cap.check_free(Class::Receipt, cap.sizes.receipt - 1)
        .expect("receipt permit released early");

    job.release_and_join();
    assert!(job.is_finished());
    cap.assert_all_free();
    // The cancelled job's result was disposed, never delivered.
    assert_eq!((counters.delivered(), counters.disposed()), (0, 1));
}

#[tokio::test]
async fn a_cancelled_result_never_becomes_an_upstream_request() {
    let upstream = FakeUpstream::start(Behavior::ok_json()).await;
    let addr = upstream.addr();
    let cap = Capacity::new(sizes());
    let counters = Arc::new(JobCounters::default());
    let (mut job, rx) = start_job(permits(&cap, 5).unwrap(), &counters, b"{}".to_vec());

    // Handler shape the gateway must follow: forward only a delivered result.
    let handler = tokio::spawn(async move {
        if let Ok(body) = rx.await {
            let _ = exchange(addr, &post("/x", &[], &body), Duration::from_secs(2)).await;
        }
    });
    handler.abort();
    let _ = handler.await;

    job.release_and_join();
    assert_eq!(counters.disposed(), 1);
    upstream.assert_nothing_sent();
    cap.assert_all_free();
}

#[tokio::test]
async fn delivered_results_do_reach_a_live_waiter() {
    // Control for the two tests above: without cancellation the result is delivered.
    let cap = Capacity::new(sizes());
    let counters = Arc::new(JobCounters::default());
    let (mut job, rx) = start_job(permits(&cap, 5).unwrap(), &counters, b"ok".to_vec());
    job.release();
    assert_eq!(rx.await.unwrap(), b"ok");
    job.join();
    assert_eq!((counters.delivered(), counters.disposed()), (1, 0));
    cap.assert_all_free();
}

#[tokio::test]
async fn repeated_cancellations_stay_within_inspection_and_memory_bounds() {
    let cap = Capacity::new(sizes());
    let counters = Arc::new(JobCounters::default());
    let mut jobs = Vec::new();
    let mut rejected = 0_usize;

    // 50 clients each start a request and immediately disconnect.
    for _ in 0..50 {
        match permits(&cap, 10) {
            Ok(p) => {
                let (job, rx) = start_job(p, &counters, Vec::new());
                drop(rx); // waiter cancelled at once
                jobs.push(job);
            }
            Err(e) => {
                assert_eq!(e, AdmissionError::Overload);
                rejected += 1;
            }
        }
    }

    // Cancelled requests cannot start more running jobs than inspection capacity.
    let limit = usize::try_from(cap.sizes.inspection).unwrap();
    assert_eq!(jobs.len(), limit);
    assert_eq!(rejected, 50 - limit);
    assert!(counters.peak_running() <= limit);
    cap.check_free(Class::Inspection, 0).unwrap();
    // Memory still reserved for every running job; none was released by cancellation.
    cap.check_free(Class::Memory, cap.sizes.memory - 10 * cap.sizes.inspection)
        .unwrap();

    for job in &mut jobs {
        job.release_and_join();
    }
    assert_eq!(counters.finished(), limit);
    assert_eq!(counters.disposed(), limit);
    cap.assert_all_free();
}

/// Negative control: if permits follow the waiter (the bug ADR 0004 forbids), the same
/// early-release check must fail, proving it can detect the defect.
#[tokio::test]
async fn early_release_check_detects_permits_tied_to_the_waiter() {
    let cap = Capacity::new(sizes());
    let counters = Arc::new(JobCounters::default());
    let (mut job, waiter) =
        start_job_with_permits_tied_to_waiter(permits(&cap, 10).unwrap(), &counters);

    // While the waiter lives, capacity is held, as it should be.
    cap.check_free(Class::Inspection, cap.sizes.inspection - 1)
        .unwrap();

    // Client disconnects: the buggy design frees capacity although the job still runs.
    drop(waiter);
    assert_eq!(counters.running(), 1);
    let violation = cap
        .check_free(Class::Inspection, cap.sizes.inspection - 1)
        .expect_err("early release must be detected");
    assert_eq!(violation.class, Class::Inspection);
    assert_eq!(violation.actual_free, cap.sizes.inspection);

    job.release_and_join();
}

#[tokio::test]
async fn a_new_request_is_rejected_not_queued_while_a_job_holds_the_only_slot() {
    let cap = Capacity::new(Sizes {
        receipt: 2,
        memory: 20,
        inspection: 1,
        upstream: 1,
        stream: 1,
    });
    let counters = Arc::new(JobCounters::default());
    let (mut job, rx) = start_job(permits(&cap, 10).unwrap(), &counters, Vec::new());
    drop(rx);

    let err = permits(&cap, 10)
        .err()
        .expect("second job must not be admitted");
    assert_eq!(err, AdmissionError::Overload);

    job.release_and_join();
    assert!(
        permits(&cap, 10).is_ok(),
        "capacity returns after the job really finishes"
    );
}
