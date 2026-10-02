//! Capacity, overload, cleanup, and acquisition-order tests for `admission`
//! (ADR 0003, resource-limits contract; issue #6).
//!
//! Scope: the try-only admission scaffold. Bounded *waiting* with deadlines arrives in
//! #18; the scenarios here (independence, aggregate memory, bounded overload, cleanup,
//! fixed order, no stream-held inspection slot) must keep passing when it does.

// Test helpers (not `#[test]` fns) may unwrap/expect; production lints do not apply here.
#![allow(clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use redact_secret_gateway::admission::AdmissionError;
use support::permits::{ALL_CLASSES, Capacity, Class, OrderTracker, OrderViolation, Sizes};

fn sizes() -> Sizes {
    Sizes {
        receipt: 3,
        memory: 30,
        inspection: 2,
        upstream: 4,
        stream: 5,
    }
}

/// Hold every permit of one class (memory: the whole budget) and return the guards.
fn exhaust(cap: &Capacity, class: Class) -> Vec<Box<dyn std::any::Any>> {
    let a = &cap.admission;
    let n = cap.sizes.of(class);
    match class {
        Class::Memory => vec![Box::new(a.try_reserve_memory(n).expect("whole budget"))],
        Class::Receipt => (0..n)
            .map(|_| Box::new(a.try_receipt().expect("free")) as _)
            .collect(),
        Class::Inspection => (0..n)
            .map(|_| Box::new(a.try_inspection().expect("free")) as _)
            .collect(),
        Class::Upstream => (0..n)
            .map(|_| Box::new(a.try_upstream().expect("free")) as _)
            .collect(),
        Class::Stream => (0..n)
            .map(|_| Box::new(a.try_stream().expect("free")) as _)
            .collect(),
    }
}

fn try_one(cap: &Capacity, class: Class) -> Result<Box<dyn std::any::Any>, AdmissionError> {
    let a = &cap.admission;
    Ok(match class {
        Class::Memory => Box::new(a.try_reserve_memory(1)?),
        Class::Receipt => Box::new(a.try_receipt()?),
        Class::Inspection => Box::new(a.try_inspection()?),
        Class::Upstream => Box::new(a.try_upstream()?),
        Class::Stream => Box::new(a.try_stream()?),
    })
}

#[test]
fn each_class_is_bounded_and_independent_of_the_others() {
    for exhausted in ALL_CLASSES {
        let cap = Capacity::new(sizes());
        let held = exhaust(&cap, exhausted);

        // The exhausted class rejects with a typed overload error.
        assert_eq!(
            try_one(&cap, exhausted).err(),
            Some(AdmissionError::Overload)
        );
        // Every other class is untouched.
        for other in ALL_CLASSES.into_iter().filter(|c| *c != exhausted) {
            cap.check_free(other, cap.sizes.of(other))
                .unwrap_or_else(|v| panic!("{exhausted:?} exhaustion affected {v}"));
        }
        drop(held);
        cap.assert_all_free();
    }
}

#[test]
fn memory_budget_is_aggregate_and_exact_at_the_boundary() {
    let cap = Capacity::new(sizes());
    let a = &cap.admission;
    // Three requests that individually look small cannot jointly exceed the budget.
    let r1 = a.try_reserve_memory(12).expect("12/30");
    let r2 = a.try_reserve_memory(12).expect("24/30");
    assert_eq!(
        a.try_reserve_memory(7).unwrap_err(),
        AdmissionError::Overload
    );
    let r3 = a.try_reserve_memory(6).expect("exactly 30/30");
    assert_eq!(
        a.try_reserve_memory(1).unwrap_err(),
        AdmissionError::Overload
    );
    assert_eq!(r1.units() + r2.units() + r3.units(), cap.sizes.memory);

    // Releasing one makes exactly that much available again.
    drop(r2);
    assert!(a.try_reserve_memory(13).is_err());
    assert!(a.try_reserve_memory(12).is_ok());
    drop((r1, r3));
    cap.assert_all_free();
}

#[test]
fn begin_receipt_failure_on_memory_does_not_leak_the_receipt_permit() {
    let cap = Capacity::new(sizes());
    let _memory = cap.admission.try_reserve_memory(25).expect("reserve");
    // 10 units do not fit in the remaining 5: begin_receipt must fail and give back the
    // receipt permit it took first.
    assert_eq!(
        cap.admission.begin_receipt(10).unwrap_err(),
        AdmissionError::Overload
    );
    cap.check_free(Class::Receipt, cap.sizes.receipt)
        .expect("receipt permit leaked on partial acquisition");
}

#[test]
fn overload_is_immediate_and_bounded_not_queued() {
    let cap = Capacity::new(sizes());
    let started = Instant::now();
    let mut admitted = Vec::new();
    let mut rejected = 0_usize;
    for _ in 0..10_000 {
        match cap.admission.try_inspection() {
            Ok(p) => admitted.push(p),
            Err(e) => {
                assert_eq!(e, AdmissionError::Overload);
                rejected += 1;
            }
        }
    }
    // Exactly the capacity was admitted; everything else was rejected on the spot.
    assert_eq!(
        admitted.len(),
        usize::try_from(cap.sizes.inspection).unwrap()
    );
    assert_eq!(rejected, 10_000 - admitted.len());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "try_* must not wait"
    );
    drop(admitted);
    cap.assert_all_free();
}

/// ADR 0003 invariant 5: a completed inspection does not hold its slot while the
/// response streams.
#[test]
fn stream_occupancy_does_not_hold_a_completed_inspection_slot() {
    let cap = Capacity::new(Sizes {
        receipt: 1,
        memory: 10,
        inspection: 1,
        upstream: 1,
        stream: 1,
    });
    let a = &cap.admission;

    // Request A: inspect, finish inspecting, then stream for a long time.
    let inspection = a.try_inspection().expect("A inspects");
    drop(inspection); // inspection really completed
    let _upstream = a.try_upstream().expect("A upstream");
    let _stream_a = a.try_stream().expect("A streams");

    // While A streams, the inspection slot is free for request B.
    cap.check_free(Class::Inspection, 1)
        .expect("stream occupancy held the inspection slot");
    let inspection_b = a.try_inspection().expect("B can inspect while A streams");
    // ...but stream capacity is independently exhausted for B.
    assert_eq!(a.try_stream().unwrap_err(), AdmissionError::Overload);
    drop(inspection_b);
}

#[test]
fn every_early_exit_stage_returns_all_permits() {
    // Walk the fixed flow and bail out after each stage, as an error path would.
    for stop_after in 0..5 {
        let cap = Capacity::new(sizes());
        {
            let a = &cap.admission;
            let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
            for (i, class) in ALL_CLASSES.into_iter().enumerate() {
                if i == stop_after {
                    break;
                }
                held.push(try_one(&cap, class).expect("free"));
            }
            let _ = a;
        }
        cap.assert_all_free();
    }
}

#[test]
fn permits_are_returned_when_the_holder_panics() {
    let cap = Capacity::new(sizes());
    let admission = Arc::clone(&cap.admission);
    let result = std::panic::catch_unwind(move || {
        let _ticket = admission.begin_receipt(10).expect("admit");
        let _inspection = admission.try_inspection().expect("inspect");
        panic!("synthetic failure while holding permits");
    });
    assert!(result.is_err());
    cap.assert_all_free();
}

#[test]
fn leak_detector_reports_a_held_permit() {
    // Negative control: the all-free check must fail while something is still held.
    let cap = Capacity::new(sizes());
    let leaked = cap.admission.try_upstream().expect("free");
    let violation = cap.check_all_free().unwrap_err();
    assert_eq!(violation.class, Class::Upstream);
    assert_eq!(violation.actual_free, cap.sizes.upstream - 1);
    drop(leaked);
    cap.assert_all_free();
}

#[test]
fn order_tracker_accepts_the_documented_order_and_flags_reversals() {
    let ok = OrderTracker::default();
    for class in ALL_CLASSES {
        ok.note_acquire(class).expect("fixed order is allowed");
    }

    // Negative control: waiting for an earlier class while holding a later one.
    let bad = OrderTracker::default();
    bad.note_acquire(Class::Inspection).unwrap();
    assert_eq!(
        bad.note_acquire(Class::Memory).unwrap_err(),
        OrderViolation {
            held: Class::Inspection,
            acquiring: Class::Memory
        }
    );

    // After releasing the later class, going back to an earlier one is fine.
    bad.note_release(Class::Inspection);
    assert!(bad.note_acquire(Class::Memory).is_ok());
}

/// Contention stress in the fixed order with retry-while-holding-earlier-classes. A
/// reversed or hold-and-wait-on-earlier path would deadlock or exceed a bound; this
/// completes within a deadline, never exceeds any capacity, and leaves nothing held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_fixed_order_flow_neither_deadlocks_nor_exceeds_capacity() {
    let cap = Arc::new(Capacity::new(sizes()));
    let peaks: Arc<[AtomicUsize; 5]> = Arc::new(Default::default());
    let live: Arc<[AtomicUsize; 5]> = Arc::new(Default::default());

    async fn acquire_retry<T>(
        mut f: impl FnMut() -> Result<T, AdmissionError>,
        class: Class,
        tracker: &OrderTracker,
        live: &[AtomicUsize; 5],
        peaks: &[AtomicUsize; 5],
    ) -> T {
        tracker
            .note_acquire(class)
            .expect("fixed acquisition order");
        loop {
            if let Ok(v) = f() {
                let now = live[class as usize].fetch_add(1, Ordering::SeqCst) + 1;
                peaks[class as usize].fetch_max(now, Ordering::SeqCst);
                return v;
            }
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    }

    let mut tasks = Vec::new();
    for _ in 0..48 {
        let (cap, peaks, live) = (Arc::clone(&cap), Arc::clone(&peaks), Arc::clone(&live));
        tasks.push(tokio::spawn(async move {
            let a = &cap.admission;
            let tracker = OrderTracker::default();
            let receipt =
                acquire_retry(|| a.try_receipt(), Class::Receipt, &tracker, &live, &peaks).await;
            let memory = acquire_retry(
                || a.try_reserve_memory(10),
                Class::Memory,
                &tracker,
                &live,
                &peaks,
            )
            .await;
            let inspection = acquire_retry(
                || a.try_inspection(),
                Class::Inspection,
                &tracker,
                &live,
                &peaks,
            )
            .await;
            tokio::time::sleep(Duration::from_micros(200)).await; // "inspection work"
            live[Class::Inspection as usize].fetch_sub(1, Ordering::SeqCst);
            tracker.note_release(Class::Inspection);
            drop(inspection); // released at real completion, before upstream/stream
            let upstream = acquire_retry(
                || a.try_upstream(),
                Class::Upstream,
                &tracker,
                &live,
                &peaks,
            )
            .await;
            let stream =
                acquire_retry(|| a.try_stream(), Class::Stream, &tracker, &live, &peaks).await;
            tokio::time::sleep(Duration::from_micros(200)).await; // relay
            for class in [
                Class::Stream,
                Class::Upstream,
                Class::Memory,
                Class::Receipt,
            ] {
                live[class as usize].fetch_sub(1, Ordering::SeqCst);
            }
            drop((stream, upstream, memory, receipt));
        }));
    }

    let all = async {
        for t in tasks {
            t.await.expect("task panicked");
        }
    };
    tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .expect("deadlock: flow did not complete");

    // Memory permits are counted as whole 10-unit reservations here.
    let limits = [
        cap.sizes.receipt,
        cap.sizes.memory / 10,
        cap.sizes.inspection,
        cap.sizes.upstream,
        cap.sizes.stream,
    ];
    for (i, limit) in limits.iter().enumerate() {
        assert!(
            peaks[i].load(Ordering::SeqCst) <= usize::try_from(*limit).unwrap(),
            "class {i} exceeded its capacity"
        );
    }
    cap.assert_all_free();
}
