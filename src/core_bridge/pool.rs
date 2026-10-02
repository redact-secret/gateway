//! Bounded offload prototype for synchronous core work (ADR 0004, issue #5 ADR gate).
//!
//! The pinned core has no cancellation, so a started call cannot be stopped. This pool
//! therefore makes the *job*, not the HTTP future, own the [`InspectionPermit`] and
//! [`MemoryReservation`]:
//!
//! - a job exists only with both permits, so queued plus running jobs never exceed the
//!   admission capacities;
//! - dropping a [`JobHandle`] marks the job cancelled. A queued job is then skipped, without
//!   running, when a worker next dequeues it, and its permits return then (the queue is
//!   bounded, so the delay is bounded by the queue ahead of it). A started job runs to its
//!   real end, then drops its permits;
//! - a cancelled job's result is discarded and is never delivered to anyone;
//! - shutdown rejects queued jobs, waits for started ones up to a deadline, and reports
//!   abandonment rather than hiding it.
//!
//! This is a measured candidate, not the selected strategy. Worker count and queue
//! capacity are parameters with no default; the probe report records the data and the
//! recommendation, and the maintainer decides (ADR 0004, ADR 0008). Each worker thread
//! builds its own [`Inspector`] because the core registry is `!Send`.

use std::future::Future;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::{Builder, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::{CoreBridgeError, InspectedText, Inspector, InspectorSpec, RequestScope};
use crate::admission::{InspectionPermit, MemoryReservation};

/// What an inspection job returns: the request scope (so numbering and limits stay
/// request-wide) and the leaf outcome.
pub type InspectOutcome = (RequestScope, Result<InspectedText, CoreBridgeError>);

/// Capacity a running or queued job holds. Dropped only when the job really ends.
struct Capacity {
    _permit: InspectionPermit,
    _memory: MemoryReservation,
}

/// Runs the job with `Some(inspector)`, or discards it with `None` (cancelled or shutdown).
type Work = Box<dyn FnOnce(Option<&Inspector>, Capacity) + Send>;

struct QueuedJob {
    cancelled: Arc<AtomicBool>,
    capacity: Capacity,
    work: Work,
}

struct Shared {
    closing: AtomicBool,
}

/// Handle to a submitted job. Await it for the result. Dropping it cancels the job: queued
/// work is skipped, started work finishes and its result is discarded.
#[derive(Debug)]
pub struct JobHandle<T> {
    receiver: oneshot::Receiver<T>,
    cancelled: Arc<AtomicBool>,
}

impl<T> JobHandle<T> {
    /// Block the calling (non-async) thread for the result.
    ///
    /// # Errors
    /// [`CoreBridgeError::Incomplete`] if the job was discarded or its worker failed.
    pub fn wait_blocking(mut self) -> Result<T, CoreBridgeError> {
        // Take the receiver out so `Drop` (which marks cancelled) still runs harmlessly.
        let (_, placeholder) = oneshot::channel();
        let receiver = std::mem::replace(&mut self.receiver, placeholder);
        receiver
            .blocking_recv()
            .map_err(|_| CoreBridgeError::Incomplete)
    }
}

impl<T> Future for JobHandle<T> {
    type Output = Result<T, CoreBridgeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.receiver).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(value)) => Poll::Ready(Ok(value)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(CoreBridgeError::Incomplete)),
        }
    }
}

impl<T> Drop for JobHandle<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Outcome of [`InspectionPool::shutdown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Every worker finished inside the deadline.
    pub drained: bool,
    /// Workers still running a core call at the deadline. Their work is abandoned, not
    /// interrupted: the core cannot be cancelled.
    pub abandoned_workers: usize,
}

/// Fixed set of dedicated inspection threads fed by a bounded queue.
#[derive(Debug)]
pub struct InspectionPool {
    sender: Option<SyncSender<QueuedJob>>,
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    done: Receiver<()>,
    worker_count: usize,
}

impl std::fmt::Debug for QueuedJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedJob").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl InspectionPool {
    /// Start `workers` threads, each building its own [`Inspector`] from `spec`, behind a
    /// queue of `queue_capacity` waiting jobs. Neither number has a default.
    ///
    /// # Errors
    /// The worker's [`CoreBridgeError`] if an inspector cannot be built;
    /// [`CoreBridgeError::Incomplete`] if a thread cannot be spawned.
    pub fn start(
        spec: &InspectorSpec,
        workers: NonZeroUsize,
        queue_capacity: NonZeroUsize,
    ) -> Result<Self, CoreBridgeError> {
        let (sender, receiver) = mpsc::sync_channel::<QueuedJob>(queue_capacity.get());
        let receiver = Arc::new(Mutex::new(receiver));
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
        });
        let (done_tx, done) = mpsc::channel::<()>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), CoreBridgeError>>();
        let mut pool = Self {
            sender: Some(sender),
            shared: Arc::clone(&shared),
            workers: Vec::new(),
            done,
            worker_count: workers.get(),
        };
        for index in 0..workers.get() {
            let spec = spec.clone();
            let receiver = Arc::clone(&receiver);
            let shared = Arc::clone(&shared);
            let done_tx = done_tx.clone();
            let init_tx = init_tx.clone();
            let handle = Builder::new()
                .name(format!("core-inspect-{index}"))
                .spawn(move || worker_loop(&spec, &receiver, &shared, &init_tx, &done_tx))
                .map_err(|_| CoreBridgeError::Incomplete)?;
            pool.workers.push(handle);
        }
        drop(init_tx);
        for _ in 0..workers.get() {
            match init_rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(CoreBridgeError::Incomplete),
            }
        }
        Ok(pool)
    }

    /// Number of worker threads.
    #[must_use]
    pub const fn workers(&self) -> usize {
        self.worker_count
    }

    /// Queue `work` against the owned capacity. The permits move into the job; if the queue is
    /// full or the pool is closing they are dropped here (nothing started, so returning them
    /// is correct).
    ///
    /// # Errors
    /// [`CoreBridgeError::Overload`] when the queue is full or the pool is closing.
    pub fn submit_with<T, F>(
        &self,
        permit: InspectionPermit,
        memory: MemoryReservation,
        work: F,
    ) -> Result<JobHandle<T>, CoreBridgeError>
    where
        T: Send + 'static,
        F: FnOnce(&Inspector) -> T + Send + 'static,
    {
        let sender = self.sender.as_ref().ok_or(CoreBridgeError::Overload)?;
        if self.shared.closing.load(Ordering::SeqCst) {
            return Err(CoreBridgeError::Overload);
        }
        let (reply, receiver) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let erased: Work = Box::new(move |inspector, capacity| {
            let Some(inspector) = inspector else {
                // Discarded before start: capacity returns, `reply` drops, awaiter sees Incomplete.
                drop(capacity);
                return;
            };
            let value = work(inspector);
            // Real completion: only now does capacity return.
            drop(capacity);
            if !flag.load(Ordering::SeqCst) {
                let _ = reply.send(value);
            }
        });
        let job = QueuedJob {
            cancelled: Arc::clone(&cancelled),
            capacity: Capacity {
                _permit: permit,
                _memory: memory,
            },
            work: erased,
        };
        match sender.try_send(job) {
            Ok(()) => Ok(JobHandle {
                receiver,
                cancelled,
            }),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                Err(CoreBridgeError::Overload)
            }
        }
    }

    /// Inspect one decoded leaf on a worker, moving `scope` in and returning it with the
    /// result so numbering and limits stay request-wide.
    ///
    /// # Errors
    /// As [`submit_with`](Self::submit_with).
    pub fn submit_inspect(
        &self,
        permit: InspectionPermit,
        memory: MemoryReservation,
        scope: RequestScope,
        text: String,
    ) -> Result<JobHandle<InspectOutcome>, CoreBridgeError> {
        self.submit_with(permit, memory, move |inspector| {
            let mut scope = scope;
            let outcome = inspector.inspect_text(&mut scope, &text);
            (scope, outcome)
        })
    }

    fn begin_close(&mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.sender = None;
    }

    /// Stop accepting work, reject queued jobs, and wait up to `timeout` for started jobs.
    /// Workers still inside a core call at the deadline are abandoned and reported.
    pub fn shutdown(mut self, timeout: Duration) -> ShutdownReport {
        self.begin_close();
        let deadline = Instant::now().checked_add(timeout);
        let mut finished = 0_usize;
        while finished < self.worker_count {
            let remaining = deadline.map_or(Duration::ZERO, |d| {
                d.saturating_duration_since(Instant::now())
            });
            if self.done.recv_timeout(remaining).is_err() {
                break;
            }
            finished = finished.saturating_add(1);
        }
        let drained = finished >= self.worker_count;
        if drained {
            for handle in std::mem::take(&mut self.workers) {
                let _ = handle.join();
            }
        }
        ShutdownReport {
            drained,
            abandoned_workers: self.worker_count.saturating_sub(finished),
        }
    }
}

impl Drop for InspectionPool {
    fn drop(&mut self) {
        self.begin_close();
    }
}

fn worker_loop(
    spec: &InspectorSpec,
    receiver: &Mutex<Receiver<QueuedJob>>,
    shared: &Shared,
    init: &mpsc::Sender<Result<(), CoreBridgeError>>,
    done: &mpsc::Sender<()>,
) {
    match Inspector::new(spec) {
        Ok(inspector) => {
            let _ = init.send(Ok(()));
            loop {
                let next = match receiver.lock() {
                    Ok(guard) => guard.recv(),
                    Err(_) => break,
                };
                let Ok(job) = next else { break };
                let QueuedJob {
                    cancelled,
                    capacity,
                    work,
                } = job;
                if shared.closing.load(Ordering::SeqCst) || cancelled.load(Ordering::SeqCst) {
                    work(None, capacity);
                } else {
                    // A panic must not kill the worker or leak the job's capacity: capacity
                    // is owned by the call frame and drops during unwinding. The awaiter sees
                    // a dropped reply, which maps to Incomplete.
                    let _ = catch_unwind(AssertUnwindSafe(|| work(Some(&inspector), capacity)));
                }
            }
        }
        Err(error) => {
            let _ = init.send(Err(error));
        }
    }
    let _ = done.send(());
}
