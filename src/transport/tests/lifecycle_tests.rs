//! Request-lifecycle hardening (#59). Compiled only under `cfg(test)`.
//!
//! One test (or a few) per transition of the lifecycle matrix in
//! `docs/contracts/request-lifecycle.md`. The whole chat route runs with the real pinned-core
//! inspection pool against a purpose-built scripted upstream (`Up`) that exposes *events*
//! (request received, peer closed) instead of sleeps, so the assertions gate on those events
//! and on capacity counters, never on a fixed delay. Where a race is the point (timeout
//! versus disconnect) a series of trials moves the cancellation across the deadline and
//! every trial must satisfy the same invariants. Real time is used only for the deadlines
//! under test, with wide margins. Every payload, key, and event text is synthetic.
#![allow(clippy::items_after_statements)]

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http_body::{Body as HttpBodyTrait, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinHandle;

use super::forward_tests::{KEY, TOKEN, alive_tasks, chat_body, raw_post, request};
use super::leak::{BODY_MARKER, Markers};
use super::*;
use crate::boundary::Inspection;
use crate::boundary::test_gate::Gate;
use crate::chat_route::{self, ChatRoute};
use crate::head_guard::HeadGuardListener;
use crate::telemetry::{Metrics, Stage, StreamEnd};
use crate::write_stall::StallIo;
use redact_secret::Profile;

const MEM: u32 = 8192;
const EVENT: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"tick\"}}]}\n\n";

// ------------------------------------------------------------------ scripted upstream

/// What the scripted upstream does after it has read the (sanitized) request.
#[derive(Clone)]
enum Script {
    /// Read the whole request, never answer, wait for the peer to close.
    HoldHeaders,
    /// Read the head and half of the body, then close without answering.
    ResetAfterPartialBody,
    /// `200 text/event-stream`, `first` at once, then `count` times `chunk` every `gap`,
    /// then either a clean end or silence until the peer closes.
    Sse {
        first: Vec<u8>,
        chunk: Vec<u8>,
        gap: Duration,
        count: usize,
        finish: bool,
    },
    /// `200 application/json` with a body of this many bytes.
    JsonBig(usize),
}

struct UpState {
    connections: AtomicUsize,
    requests: AtomicUsize,
    partial_requests: AtomicUsize,
    closed: AtomicUsize,
    received: Semaphore,
    closed_events: Semaphore,
    bytes: Mutex<Vec<u8>>,
}

/// Loopback upstream whose behavior per connection is chosen by index.
struct Up {
    addr: SocketAddr,
    state: Arc<UpState>,
    task: JoinHandle<()>,
}

impl Drop for Up {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Up {
    async fn start(choose: impl Fn(usize) -> Script + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(UpState {
            connections: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            partial_requests: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
            received: Semaphore::new(0),
            closed_events: Semaphore::new(0),
            bytes: Mutex::new(Vec::new()),
        });
        let shared = Arc::clone(&state);
        let choose = Arc::new(choose);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let index = shared.connections.fetch_add(1, Ordering::SeqCst);
                let script = choose(index);
                tokio::spawn(serve_one(stream, script, Arc::clone(&shared)));
            }
        });
        Self { addr, state, task }
    }

    fn requests(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }

    fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }

    /// Wait for one more request to have been fully received (an event, not a delay).
    async fn received(&self) {
        within(self.state.received.acquire())
            .await
            .unwrap()
            .forget();
    }

    /// Wait until the gateway closed one more upstream connection.
    async fn peer_closed(&self) {
        within(self.state.closed_events.acquire())
            .await
            .unwrap()
            .forget();
    }

    fn assert_never_saw(&self, markers: &Markers) {
        let bytes = self.state.bytes.lock().unwrap();
        markers.assert_clean("bytes the upstream received", &bytes);
    }
}

async fn within<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), f)
        .await
        .expect("event did not happen in time")
}

async fn serve_one(mut stream: TcpStream, script: Script, state: Arc<UpState>) {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => {
                state.closed.fetch_add(1, Ordering::SeqCst);
                state.closed_events.add_permits(1);
                return;
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let want = if matches!(script, Script::ResetAfterPartialBody) {
        length / 2
    } else {
        length
    };
    while buf.len() < head_end + want {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    state.bytes.lock().unwrap().extend_from_slice(&buf);
    if buf.len() < head_end + length {
        state.partial_requests.fetch_add(1, Ordering::SeqCst);
    }
    state.requests.fetch_add(1, Ordering::SeqCst);
    state.received.add_permits(1);
    match script {
        Script::ResetAfterPartialBody => {
            drop(stream);
            return;
        }
        Script::HoldHeaders => {}
        Script::JsonBig(n) => {
            let body = format!(r#"{{"d":"{}"}}"#, "a".repeat(n));
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).await.is_ok() {
                let _ = stream.write_all(body.as_bytes()).await;
            }
        }
        Script::Sse {
            first,
            chunk,
            gap,
            count,
            finish,
        } => {
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            let framed = |d: &[u8]| {
                let mut out = format!("{:x}\r\n", d.len()).into_bytes();
                out.extend_from_slice(d);
                out.extend_from_slice(b"\r\n");
                out
            };
            if stream.write_all(head.as_bytes()).await.is_err() {
                return closed(&state);
            }
            if !first.is_empty() && stream.write_all(&framed(&first)).await.is_err() {
                return closed(&state);
            }
            for _ in 0..count {
                tokio::select! {
                    () = tokio::time::sleep(gap) => {
                        if stream.write_all(&framed(&chunk)).await.is_err() {
                            return closed(&state);
                        }
                    }
                    read = stream.read(&mut tmp) => {
                        if matches!(read, Ok(0) | Err(_)) {
                            return closed(&state);
                        }
                    }
                }
            }
            if finish {
                let _ = stream.write_all(b"0\r\n\r\n").await;
                let _ = stream.shutdown().await;
                return;
            }
        }
    }
    // Hold: wait for the gateway to close its side.
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    closed(&state);
}

fn closed(state: &UpState) {
    state.closed.fetch_add(1, Ordering::SeqCst);
    state.closed_events.add_permits(1);
}

// ------------------------------------------------------------------------------ the lab

struct Caps4 {
    receipt: u32,
    inspection: u32,
    upstream: u32,
    stream: u32,
}

const ROOMY: Caps4 = Caps4 {
    receipt: 64,
    inspection: 2,
    upstream: 16,
    stream: 8,
};

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

fn plan(c: &Caps4) -> CapacityPlan {
    CapacityPlan::new(
        nz(c.receipt),
        nz(MEM),
        nz(c.inspection),
        nz(c.upstream),
        nz(c.stream),
    )
}

fn limits_with(f: impl FnOnce(&mut RequestLimits)) -> RequestLimits {
    let mut limits = RequestLimits::provisional();
    f(&mut limits);
    limits
}

struct Lab {
    route: Arc<ChatRoute>,
    admission: Arc<Admission>,
    inspection: Arc<Inspection>,
    metrics: Arc<Metrics>,
    gate: Arc<Gate>,
    up: Up,
    caps: Caps4,
}

impl Lab {
    async fn new(script: Script, limits: RequestLimits) -> Self {
        Self::with(move |_| script.clone(), limits, ROOMY, None).await
    }

    /// `worker_plan` sizes the inspection pool independently of the admission capacity, so
    /// a test can have fewer workers than permits (a job can then wait in the queue).
    async fn with(
        choose: impl Fn(usize) -> Script + Send + Sync + 'static,
        limits: RequestLimits,
        caps: Caps4,
        worker_plan: Option<Caps4>,
    ) -> Self {
        let up = Up::start(choose).await;
        let upstream = http_upstream_with(up.addr, limits);
        let metrics = Arc::new(Metrics::new());
        let upstream = Upstream {
            metrics: Some(Arc::clone(&metrics)),
            ..upstream
        };
        let admission = Arc::new(Admission::new(&plan(&caps)));
        // Open by default; a test that needs a job parked in "running" closes it first.
        let gate = Arc::new(Gate::new_closed());
        gate.open();
        let inspection = Arc::new(
            Inspection::start(
                Arc::clone(&admission),
                &crate::config::ContentPolicy::new(Profile::Full),
                &limits,
                &plan(worker_plan.as_ref().unwrap_or(&caps)),
            )
            .unwrap()
            .with_metrics(Arc::clone(&metrics))
            .with_test_gate(Arc::clone(&gate)),
        );
        let route = Arc::new(
            ChatRoute::new(Arc::clone(&admission), limits, RouteId::new(ROUTE))
                .with_inspection(Arc::clone(&inspection))
                .with_upstream(Arc::new(upstream))
                .with_metrics(Arc::clone(&metrics)),
        );
        Self {
            route,
            admission,
            inspection,
            metrics,
            gate,
            up,
            caps,
        }
    }

    /// Spawn the request as hyper would: a task whose abort is the caller's disconnect.
    fn spawn(&self, request: Request) -> JoinHandle<Response> {
        let route = Arc::clone(&self.route);
        tokio::spawn(async move { route.handle(request).await })
    }

    /// Free units per class, observed by trying to take them all (synchronously, so no
    /// other task can interleave). Order: receipt, inspection, upstream, stream, and `1`
    /// when the whole memory budget is free.
    fn free(&self) -> [usize; 5] {
        let mut keep: Vec<Box<dyn std::any::Any>> = Vec::new();
        macro_rules! take {
            ($n:expr, $f:expr) => {{
                let mut got = 0_usize;
                for _ in 0..$n {
                    if let Ok(permit) = $f {
                        keep.push(Box::new(permit));
                        got += 1;
                    }
                }
                got
            }};
        }
        let receipts = take!(self.caps.receipt, self.admission.try_receipt());
        let inspections = take!(self.caps.inspection, self.admission.try_inspection());
        let upstreams = take!(self.caps.upstream, self.admission.try_upstream());
        let streams = take!(self.caps.stream, self.admission.try_stream());
        let memory = usize::from(self.admission.try_reserve_memory(MEM).is_ok());
        [receipts, inspections, upstreams, streams, memory]
    }

    fn baseline(&self) -> [usize; 5] {
        [
            self.caps.receipt as usize,
            self.caps.inspection as usize,
            self.caps.upstream as usize,
            self.caps.stream as usize,
            1,
        ]
    }

    fn is_baseline(&self) -> bool {
        self.free() == self.baseline()
    }

    /// Capacity held by owned work is returned: poll the counters until they are at
    /// baseline (bounded). The counters are the event; there is no fixed delay.
    async fn settle(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !self.is_baseline() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "capacity was not returned: free {:?}, baseline {:?}",
                self.free(),
                self.baseline()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait for a predicate over the counters (bounded).
    async fn until(&self, what: &str, mut ok: impl FnMut(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !ok(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never: {what} (free {:?})",
                self.free()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn assert_zero_forward(&self) {
        assert_eq!(self.up.connections(), 0, "no upstream connection");
        assert_eq!(self.up.requests(), 0, "no upstream request");
        assert_eq!(self.metrics.upstream_attempts(), 0, "no send attempt");
    }

    fn ended(&self) -> u64 {
        [
            StreamEnd::Completed,
            StreamEnd::UpstreamError,
            StreamEnd::IdleTimeout,
            StreamEnd::LifetimeExceeded,
            StreamEnd::BufferExceeded,
            StreamEnd::Shutdown,
            StreamEnd::Abandoned,
        ]
        .into_iter()
        .map(|e| self.metrics.streams_ended(e))
        .sum()
    }
}

fn markers() -> Markers {
    Markers::standard().with("key", KEY).with("token", TOKEN)
}

fn marked_request() -> Request {
    request(&chat_body(BODY_MARKER), KEY, &[])
}

fn marked_stream_request() -> Request {
    let body = format!(
        r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{BODY_MARKER}"}}],"stream":true}}"#
    );
    request(&body, KEY, &[])
}

async fn collect(response: Response) -> (u16, Vec<u8>) {
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 24).await.unwrap();
    (parts.status.as_u16(), bytes.to_vec())
}

fn assert_code(status_body: &(u16, Vec<u8>), status: u16, code: &str) {
    assert_eq!(status_body.0, status, "status");
    assert_eq!(
        String::from_utf8_lossy(&status_body.1),
        format!(r#"{{"error":{{"code":"{code}"}}}}"#)
    );
    markers().assert_clean("gateway error body", &status_body.1);
}

/// A body that delivers ten bytes and then never completes: a caller stuck mid-body.
struct StuckBody {
    sent: bool,
}

impl HttpBodyTrait for StuckBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.sent {
            Poll::Pending
        } else {
            self.sent = true;
            Poll::Ready(Some(Ok(Frame::data(axum::body::Bytes::from_static(
                b"{\"model\":\"g",
            )))))
        }
    }
}

fn stuck_request() -> Request {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("content-length", "400")
        .header("authorization", format!("Bearer {KEY}"))
        .body(Body::new(StuckBody { sent: false }))
        .unwrap()
}

// ------------------------------------------------------------ a real, production server

struct Served {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), crate::server::StartupError>>,
}

impl Served {
    async fn start(lab: &Lab, drain: Duration) -> Self {
        let plan = Arc::new(
            crate::config::parse(
                br#"{"schema_version":1,
                  "deployment":{"listener":{"address":"127.0.0.1:0"}},
                  "content":{"profile":"common"},
                  "resources":{"capacity":{"receipt":1,"memory_units":1,"inspection":1,
                    "upstream":1,"stream":1}}}"#,
            )
            .unwrap(),
        );
        let services = crate::server::Services {
            admission: Arc::clone(&lab.admission),
            chat: Arc::clone(&lab.route),
            drain,
        };
        let bound = crate::server::bind(plan, move |_| Ok(services))
            .await
            .unwrap();
        let addr = bound.local_addr().unwrap();
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(bound.serve(async move {
            let _ = rx.await;
        }));
        Self {
            addr,
            stop: Some(stop),
            task,
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Read until the peer closes or `limit` elapses. Returns the bytes and whether the close
/// was seen.
async fn read_close(client: &mut TcpStream, limit: Duration) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut tmp = [0_u8; 8192];
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match tokio::time::timeout_at(deadline, client.read(&mut tmp)).await {
            Err(_) => return (out, false),
            Ok(Ok(0) | Err(_)) => return (out, true),
            Ok(Ok(n)) => out.extend_from_slice(&tmp[..n]),
        }
    }
}

/// Read until the end of the response head.
async fn read_head(client: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    loop {
        let n = within(client.read(&mut tmp)).await.unwrap();
        assert!(n > 0, "closed before the response head");
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return buf;
        }
    }
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

// ====================================================== stage: connection accept / head wait

#[tokio::test]
async fn disconnect_while_the_head_is_incomplete_returns_the_connection_slot() {
    // One slot: a second request can be served only after the abandoned head's slot returns.
    let limits = limits_with(|l| l.max_connections = 1);
    let lab = Lab::new(Script::JsonBig(64), limits).await;
    let served = Served::start(&lab, Duration::from_secs(5)).await;
    let mut half = served.connect().await;
    half.write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\n")
        .await
        .unwrap();
    drop(half);
    // The slot is free again once the server has dropped the abandoned connection: the next
    // request is served (a refused connect closes at once with no bytes, so retry on that).
    let served_ok = within(async {
        loop {
            let mut c = served.connect().await;
            c.write_all(&raw_post(&chat_body("hello"), KEY))
                .await
                .unwrap();
            let (bytes, _) = read_close(&mut c, Duration::from_secs(10)).await;
            if bytes.starts_with(b"HTTP/1.1 200") {
                return true;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(served_ok);
    assert_eq!(
        lab.up.requests(),
        1,
        "only the complete request reached upstream"
    );
    lab.settle().await;
}

// ============================================================ stage: body receipt

#[tokio::test]
async fn disconnect_during_body_receipt_releases_reservation_and_forwards_nothing() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    let handler = lab.spawn(stuck_request());
    lab.until("receipt reservation held", |l| !l.is_baseline())
        .await;
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    lab.settle().await;
    lab.assert_zero_forward();
}

#[tokio::test]
async fn shutdown_during_body_receipt_answers_503_and_returns_capacity() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    let handler = lab.spawn(stuck_request());
    lab.until("receipt reservation held", |l| !l.is_baseline())
        .await;
    lab.route.cancel_in_flight();
    let out = collect(within(handler).await.unwrap()).await;
    assert_code(&out, 503, "not_ready");
    lab.settle().await;
    lab.assert_zero_forward();
}

#[tokio::test]
async fn body_deadline_during_receipt_is_a_408_and_returns_capacity() {
    let limits = limits_with(|l| l.body_deadline_ms = 300);
    let lab = Lab::new(Script::HoldHeaders, limits).await;
    let out = collect(within(lab.spawn(stuck_request())).await.unwrap()).await;
    assert_code(&out, 408, "limit_exceeded");
    lab.settle().await;
    lab.assert_zero_forward();
}

// ================================================ stage: inspection queue (job not started)

/// Park the only worker on a job the test controls (no permits other than its own).
struct Blocker {
    release: std::sync::mpsc::Sender<()>,
    handle: crate::core_bridge::pool::JobHandle<()>,
}

async fn block_the_only_worker(lab: &Lab) -> Blocker {
    let started = Arc::new(Semaphore::new(0));
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let permit = lab.admission.try_inspection().unwrap();
    let flag = Arc::clone(&started);
    let handle = lab
        .inspection
        .pool()
        .submit_job(permit, move |_| {
            flag.add_permits(1);
            let _ = wait.recv();
        })
        .unwrap();
    within(started.acquire()).await.unwrap().forget();
    Blocker { release, handle }
}

fn one_worker() -> Caps4 {
    Caps4 {
        receipt: 1,
        inspection: 1,
        upstream: 1,
        stream: 1,
    }
}

#[tokio::test]
async fn a_queued_inspection_cancelled_by_disconnect_is_skipped_and_returns_its_capacity() {
    let lab = Lab::with(
        |_| Script::HoldHeaders,
        RequestLimits::provisional(),
        ROOMY,
        Some(one_worker()),
    )
    .await;
    let blocker = block_the_only_worker(&lab).await;
    let handler = lab.spawn(marked_request());
    // The request holds the second inspection permit, which it can only have taken to queue.
    lab.until("request queued behind the running job", |l| {
        l.free()[1] == 0
    })
    .await;
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    // The waiter is gone, the queued job still owns its permit and memory until a worker
    // dequeues it.
    assert_eq!(lab.free()[1], 0, "queued job keeps the inspection permit");
    assert_eq!(lab.free()[4], 0, "queued job keeps the memory reservation");
    blocker.release.send(()).unwrap();
    within(blocker.handle).await.unwrap();
    lab.settle().await;
    // Skipped, not run: the worker never serialized it, and nothing was sent.
    assert_eq!(lab.metrics.stage(Stage::Serialization).count, 0);
    lab.assert_zero_forward();
}

#[tokio::test]
async fn shutdown_while_inspection_is_queued_answers_503_and_the_job_is_skipped() {
    let lab = Lab::with(
        |_| Script::HoldHeaders,
        RequestLimits::provisional(),
        ROOMY,
        Some(one_worker()),
    )
    .await;
    let blocker = block_the_only_worker(&lab).await;
    let handler = lab.spawn(marked_request());
    lab.until("request queued", |l| l.free()[1] == 0).await;
    lab.route.cancel_in_flight();
    let out = collect(within(handler).await.unwrap()).await;
    assert_code(&out, 503, "not_ready");
    assert_eq!(lab.free()[1], 0, "the queued job still owns its permit");
    blocker.release.send(()).unwrap();
    within(blocker.handle).await.unwrap();
    lab.settle().await;
    assert_eq!(lab.metrics.stage(Stage::Serialization).count, 0);
    lab.assert_zero_forward();
}

// ===================== stage: inspection running (synchronous core, non-interruptible)

#[tokio::test]
async fn a_started_inspection_keeps_cpu_and_memory_after_the_waiter_is_dropped() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    lab.gate.close();
    let handler = lab.spawn(marked_request());
    lab.gate.entered().await; // the worker is inside the job
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    // The waiter is gone, the work is not: both permits are still held.
    assert_eq!(
        lab.free()[1],
        1,
        "inspection permit held by the running job"
    );
    assert_eq!(
        lab.free()[4],
        0,
        "memory reservation held by the running job"
    );
    assert_eq!(
        lab.metrics.stage(Stage::Serialization).count,
        0,
        "not finished"
    );
    lab.gate.open();
    lab.settle().await;
    // The job ran to its real end (serialization recorded) and its result went nowhere.
    assert_eq!(lab.metrics.stage(Stage::Serialization).count, 1);
    lab.assert_zero_forward();
}

#[tokio::test]
async fn shutdown_deadline_of_the_waiter_is_not_the_end_of_the_synchronous_work() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    lab.gate.close();
    let handler = lab.spawn(marked_request());
    lab.gate.entered().await;
    lab.route.cancel_in_flight();
    // The waiter ends at the cancellation with the documented answer ...
    let out = collect(within(handler).await.unwrap()).await;
    assert_code(&out, 503, "not_ready");
    // ... but the running job still owns its capacity until it really finishes.
    assert_eq!(lab.free()[1], 1);
    assert_eq!(lab.free()[4], 0);
    lab.gate.open();
    lab.settle().await;
    assert_eq!(lab.metrics.stage(Stage::Serialization).count, 1);
    lab.assert_zero_forward();
}

#[tokio::test]
async fn a_new_request_is_refused_not_queued_while_running_jobs_hold_every_permit() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    lab.gate.close();
    let first = lab.spawn(marked_request());
    let second = lab.spawn(marked_request());
    lab.gate.entered().await;
    lab.gate.entered().await;
    let out = collect(within(lab.spawn(marked_request())).await.unwrap()).await;
    assert_code(&out, 503, "overload");
    lab.gate.open();
    first.abort();
    second.abort();
    let _ = first.await;
    let _ = second.await;
    lab.settle().await;
    lab.assert_zero_forward();
}

#[tokio::test]
async fn repeated_start_stop_cycles_return_every_counter_and_task_to_baseline() {
    let limits = limits_with(|l| l.upstream_header_ms = 60_000);
    let lab = Lab::with(|_| Script::HoldHeaders, limits, ROOMY, Some(one_worker())).await;
    let tasks = alive_tasks();
    let mut upstream_cycles = 0_usize;
    for cycle in 0..24 {
        match cycle % 4 {
            // stuck body receipt, abandoned
            0 => {
                let h = lab.spawn(stuck_request());
                lab.until("receipt held", |l| !l.is_baseline()).await;
                h.abort();
                let _ = h.await;
            }
            // queued behind a running job, abandoned
            1 => {
                let blocker = block_the_only_worker(&lab).await;
                let h = lab.spawn(marked_request());
                lab.until("queued", |l| l.free()[1] == 0).await;
                h.abort();
                let _ = h.await;
                blocker.release.send(()).unwrap();
                within(blocker.handle).await.unwrap();
            }
            // running (parked on the gate), abandoned
            2 => {
                lab.gate.close();
                let h = lab.spawn(marked_request());
                lab.gate.entered().await;
                h.abort();
                let _ = h.await;
                lab.gate.open();
            }
            // waiting for upstream headers, abandoned
            _ => {
                lab.gate.open();
                let h = lab.spawn(marked_request());
                lab.up.received().await;
                upstream_cycles += 1;
                h.abort();
                let _ = h.await;
                lab.up.peer_closed().await;
            }
        }
        lab.settle().await;
    }
    assert_eq!(
        lab.up.requests(),
        upstream_cycles,
        "only the upstream-wait cycles sent"
    );
    assert_eq!(lab.metrics.upstream_attempts() as usize, upstream_cycles);
    // No accumulating tasks: the connection handlers and exchanges are gone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while alive_tasks() > tasks {
        assert!(
            tokio::time::Instant::now() < deadline,
            "leaked tasks {} > {}",
            alive_tasks(),
            tasks
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    lab.up
        .assert_never_saw(&Markers::empty().with("token", TOKEN));
}

// ====================================== stages: upstream permit wait / connect / send

#[tokio::test]
async fn pre_send_failures_across_the_lifecycle_deliver_zero_bytes_upstream() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    // Upstream class exhausted after inspection: overload, nothing sent.
    let held: Vec<_> = (0..lab.caps.upstream)
        .map(|_| lab.admission.try_upstream().unwrap())
        .collect();
    let out = collect(within(lab.spawn(marked_request())).await.unwrap()).await;
    assert_code(&out, 503, "overload");
    // Stream class exhausted for a stream request: overload, nothing sent.
    drop(held);
    let held: Vec<_> = (0..lab.caps.stream)
        .map(|_| lab.admission.try_stream().unwrap())
        .collect();
    let out = collect(within(lab.spawn(marked_stream_request())).await.unwrap()).await;
    assert_code(&out, 503, "overload");
    drop(held);
    // Validation failure and missing credential.
    let out = collect(within(lab.spawn(request("{", KEY, &[]))).await.unwrap()).await;
    assert_code(&out, 400, "malformed_input");
    let out = collect(
        within(lab.spawn(request(&chat_body("x"), "", &[])))
            .await
            .unwrap(),
    )
    .await;
    assert_code(&out, 401, "missing_credential");
    lab.settle().await;
    lab.assert_zero_forward();
    // Shutdown before the request starts: refused, nothing sent.
    lab.route.cancel_in_flight();
    let out = collect(within(lab.spawn(marked_request())).await.unwrap()).await;
    assert_code(&out, 503, "not_ready");
    lab.settle().await;
    lab.assert_zero_forward();
}

#[tokio::test]
async fn a_partial_upstream_send_is_never_replayed_and_never_falls_back() {
    for streaming in [false, true] {
        let lab = Lab::new(Script::ResetAfterPartialBody, RequestLimits::provisional()).await;
        let request = if streaming {
            marked_stream_request()
        } else {
            marked_request()
        };
        let out = collect(within(lab.spawn(request)).await.unwrap()).await;
        assert_eq!(
            out.0, 502,
            "an ordinary gateway error before any response commitment"
        );
        let body = String::from_utf8_lossy(&out.1).into_owned();
        assert!(body.contains("upstream_"), "{body}");
        markers().assert_clean("gateway error body", &out.1);
        lab.settle().await;
        // Exactly one connection, one (cut) request, one attempt: no retry, no raw fallback.
        assert_eq!(lab.up.connections(), 1);
        assert_eq!(lab.up.requests(), 1);
        assert_eq!(lab.metrics.upstream_attempts(), 1);
        // A negative window: nothing else arrives afterwards.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(lab.up.connections(), 1, "no replay");
        // What did reach the provider is the sanitized form, never the original secret.
        lab.up
            .assert_never_saw(&Markers::empty().with("token", TOKEN));
    }
}

// ================================================ stage: upstream headers wait

#[tokio::test]
async fn stuck_upstream_headers_end_in_a_504_once_and_close_the_exchange() {
    let limits = limits_with(|l| {
        l.upstream_header_ms = 300;
        l.upstream_total_ms = 600;
    });
    let lab = Lab::new(Script::HoldHeaders, limits).await;
    let out = collect(within(lab.spawn(marked_request())).await.unwrap()).await;
    assert_code(&out, 504, "upstream_timeout");
    lab.up.peer_closed().await; // the exchange was really closed, not left dangling
    lab.settle().await;
    assert_eq!(lab.up.requests(), 1);
    assert_eq!(
        lab.metrics.upstream_attempts(),
        1,
        "no retry after the timeout"
    );
}

#[tokio::test]
async fn disconnect_while_waiting_for_upstream_headers_cancels_the_exchange() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    let handler = lab.spawn(marked_request());
    lab.up.received().await;
    assert_eq!(lab.free()[2], 15, "the exchange owns an upstream permit");
    handler.abort();
    let _ = handler.await;
    lab.up.peer_closed().await;
    lab.settle().await;
    assert_eq!(lab.up.requests(), 1, "no retry after the disconnect");
}

#[tokio::test]
async fn shutdown_while_waiting_for_upstream_headers_answers_503_and_closes_the_exchange() {
    let lab = Lab::new(Script::HoldHeaders, RequestLimits::provisional()).await;
    let handler = lab.spawn(marked_request());
    lab.up.received().await;
    lab.route.cancel_in_flight();
    let out = collect(within(handler).await.unwrap()).await;
    assert_code(&out, 503, "not_ready");
    lab.up.peer_closed().await;
    lab.settle().await;
    assert_eq!(lab.up.requests(), 1);
    assert_eq!(lab.metrics.upstream_attempts(), 1);
}

#[tokio::test]
async fn simultaneous_header_timeout_and_disconnect_settle_exactly_once() {
    // The cancellation point moves across the header deadline; whichever wins, the same
    // invariants hold for every trial.
    let limits = limits_with(|l| {
        l.upstream_header_ms = 150;
        l.upstream_total_ms = 300;
    });
    let lab = Lab::new(Script::HoldHeaders, limits).await;
    let mut timed_out = 0;
    let mut aborted = 0;
    for trial in 0..24_u64 {
        let handler = lab.spawn(marked_request());
        lab.up.received().await;
        tokio::time::sleep(Duration::from_millis(110 + trial * 3)).await;
        handler.abort();
        match handler.await {
            Ok(response) => {
                assert_code(&collect(response).await, 504, "upstream_timeout");
                timed_out += 1;
            }
            Err(e) => {
                assert!(e.is_cancelled());
                aborted += 1;
            }
        }
        lab.up.peer_closed().await;
        lab.settle().await;
        let n = usize::try_from(trial).unwrap() + 1;
        assert_eq!(lab.up.requests(), n, "one request per trial, no retries");
        assert_eq!(lab.metrics.upstream_attempts() as usize, n);
    }
    assert_eq!(timed_out + aborted, 24);
}

// ================================================= stage: SSE relay

fn sse(first: &[u8], gap_ms: u64, count: usize, finish: bool) -> Script {
    Script::Sse {
        first: first.to_vec(),
        chunk: EVENT.to_vec(),
        gap: Duration::from_millis(gap_ms),
        count,
        finish,
    }
}

#[tokio::test]
async fn trickling_just_under_the_idle_deadline_survives_and_completes() {
    // Idle deadline 1 s; the provider sends every 600 ms (each gap at 60 % of the deadline),
    // for far longer than one idle period in total.
    let limits = limits_with(|l| {
        l.stream_idle_ms = 1000;
        l.stream_lifetime_ms = 60_000;
    });
    let lab = Lab::new(sse(EVENT, 600, 4, true), limits).await;
    let served = Served::start(&lab, Duration::from_secs(5)).await;
    let mut client = served.connect().await;
    client
        .write_all(&raw_post(&marked_body_stream(), KEY))
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let (bytes, closed) = read_close(&mut client, Duration::from_secs(20)).await;
    assert!(closed);
    assert!(
        started.elapsed() >= Duration::from_millis(2300),
        "the stream outlived one idle period"
    );
    assert_eq!(count(&bytes, EVENT), 5, "every provider event arrived");
    assert!(
        bytes.ends_with(b"0\r\n\r\n"),
        "a clean provider end is a clean end"
    );
    assert_eq!(lab.metrics.streams_ended(StreamEnd::Completed), 1);
    lab.settle().await;
}

#[tokio::test]
async fn silence_after_trickle_is_cut_at_the_idle_deadline_without_a_marker_or_second_status() {
    let limits = limits_with(|l| {
        l.stream_idle_ms = 500;
        l.stream_lifetime_ms = 60_000;
    });
    let lab = Lab::new(sse(EVENT, 200, 2, false), limits).await;
    let served = Served::start(&lab, Duration::from_secs(5)).await;
    let mut client = served.connect().await;
    client
        .write_all(&raw_post(&marked_body_stream(), KEY))
        .await
        .unwrap();
    let (bytes, closed) = read_close(&mut client, Duration::from_secs(20)).await;
    assert!(closed, "the cut closes the connection");
    assert_eq!(count(&bytes, b"HTTP/1.1"), 1, "no second HTTP status");
    assert!(bytes.starts_with(b"HTTP/1.1 200"));
    assert_eq!(count(&bytes, EVENT), 3, "provider bytes preserved");
    assert!(
        !bytes.ends_with(b"0\r\n\r\n"),
        "no terminating chunk: truncated"
    );
    for forbidden in [&b"[DONE]"[..], b"error", b"not_ready", b"upstream_timeout"] {
        assert_eq!(count(&bytes, forbidden), 0, "no fabricated marker");
    }
    markers().assert_clean("truncated stream", &bytes);
    lab.up.peer_closed().await;
    assert_eq!(lab.metrics.streams_ended(StreamEnd::IdleTimeout), 1);
    lab.settle().await;
}

fn marked_body_stream() -> String {
    format!(
        r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{BODY_MARKER}"}}],"stream":true}}"#
    )
}

#[tokio::test]
async fn simultaneous_idle_timeout_and_disconnect_end_every_stream_exactly_once() {
    let limits = limits_with(|l| {
        l.stream_idle_ms = 150;
        l.stream_lifetime_ms = 60_000;
    });
    let lab = Lab::new(sse(EVENT, 0, 0, false), limits).await;
    let served = Served::start(&lab, Duration::from_secs(5)).await;
    // One lazily spawned client-pool task appears with the first exchange and then stays;
    // the baseline is taken after the first trial so only accumulation is measured.
    let mut tasks = usize::MAX;
    for trial in 0..20_u64 {
        let mut client = served.connect().await;
        client
            .write_all(&raw_post(&marked_body_stream(), KEY))
            .await
            .unwrap();
        let head = read_head(&mut client).await; // the headers are committed
        assert!(head.starts_with(b"HTTP/1.1 200"));
        tokio::time::sleep(Duration::from_millis(110 + trial * 3)).await;
        drop(client);
        let n = trial + 1;
        lab.until("stream ended", |l| l.ended() == n).await;
        lab.up.peer_closed().await;
        lab.settle().await;
        if trial == 0 {
            tasks = alive_tasks();
        }
        assert_eq!(lab.metrics.streams_started(), n, "one start per trial");
    }
    // Each stream ended once, as either the idle cut or the abandonment.
    let idle = lab.metrics.streams_ended(StreamEnd::IdleTimeout);
    let gone = lab.metrics.streams_ended(StreamEnd::Abandoned);
    assert_eq!(idle + gone, 20);
    assert_eq!(lab.up.requests(), 20);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while alive_tasks() > tasks {
        assert!(
            tokio::time::Instant::now() < deadline,
            "leaked tasks {} > {}",
            alive_tasks(),
            tasks
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn an_abandoned_sse_connection_that_is_never_read_is_cut_and_its_permits_return() {
    // The consumer reads the headers and then neither reads nor closes. The provider writes
    // as fast as it can; the write-stall deadline ends the connection.
    let limits = limits_with(|l| {
        l.stream_write_stall_ms = 400;
        l.stream_idle_ms = 20_000;
        l.stream_lifetime_ms = 60_000;
    });
    let big = vec![b'x'; 32 * 1024];
    let mut event = b"data: ".to_vec();
    event.extend_from_slice(&big);
    event.extend_from_slice(b"\n\n");
    let lab = Lab::new(
        Script::Sse {
            first: event.clone(),
            chunk: event,
            gap: Duration::ZERO,
            count: 100_000,
            finish: false,
        },
        limits,
    )
    .await;
    let served = Served::start(&lab, Duration::from_secs(5)).await;
    let mut client = served.connect().await;
    client
        .write_all(&raw_post(&marked_body_stream(), KEY))
        .await
        .unwrap();
    let _ = read_head(&mut client).await;
    // Never read again. The server cuts the connection after the stall deadline.
    lab.until("stream ended by the write stall", |l| l.ended() == 1)
        .await;
    assert_eq!(lab.metrics.streams_ended(StreamEnd::Abandoned), 1);
    lab.up.peer_closed().await;
    lab.settle().await;
    drop(client);
}

// ============================================================ stage: shutdown / drain

#[tokio::test]
async fn shutdown_with_many_open_connections_and_streams_ends_within_the_drain() {
    let limits = limits_with(|l| {
        l.max_connections = 128;
        l.upstream_header_ms = 120_000;
        l.stream_idle_ms = 120_000;
        l.body_deadline_ms = 60_000;
    });
    let caps = Caps4 {
        receipt: 64,
        inspection: 4,
        upstream: 16,
        stream: 8,
    };
    // First eight upstream connections stream, the next six hold their headers.
    let lab = Lab::with(
        |index| {
            if index < 8 {
                Script::Sse {
                    first: EVENT.to_vec(),
                    chunk: EVENT.to_vec(),
                    gap: Duration::from_millis(50),
                    count: 1_000_000,
                    finish: true,
                }
            } else {
                Script::HoldHeaders
            }
        },
        limits,
        caps,
        None,
    )
    .await;
    lab.gate.open();
    let mut served = Served::start(&lab, Duration::from_millis(400)).await;
    let tasks = alive_tasks();

    // Eight streams, committed.
    let mut streams = Vec::new();
    for _ in 0..8 {
        let mut c = served.connect().await;
        c.write_all(&raw_post(&marked_body_stream(), KEY))
            .await
            .unwrap();
        let head = read_head(&mut c).await;
        assert!(head.starts_with(b"HTTP/1.1 200"));
        streams.push(c);
    }
    // Six JSON requests stuck waiting for upstream headers.
    let mut stuck = Vec::new();
    for _ in 0..6 {
        let mut c = served.connect().await;
        c.write_all(&raw_post(&chat_body("hello"), KEY))
            .await
            .unwrap();
        lab.up.received().await;
        stuck.push(c);
    }
    // Ten callers stuck mid-body, twenty that never send a byte.
    let mut partial = Vec::new();
    for _ in 0..10 {
        let mut c = served.connect().await;
        c.write_all(
            format!("POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\nAuthorization: Bearer {KEY}\r\nContent-Length: 400\r\n\r\n{{\"model\":\"g").as_bytes(),
        )
        .await
        .unwrap();
        partial.push(c);
    }
    let mut silent = Vec::new();
    for _ in 0..20 {
        silent.push(served.connect().await);
    }
    lab.until("every owned resource is held", |l| {
        let free = l.free();
        free[3] == 0 && free[2] == 2 && free[0] <= 54
    })
    .await;

    let asked = tokio::time::Instant::now();
    served.stop();
    let result = within(&mut served.task).await.unwrap();
    assert!(result.is_ok());
    let took = asked.elapsed();
    assert!(
        took >= Duration::from_millis(350),
        "drain honoured: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "drain bounded: {took:?}");

    // Streams: truncated at the shutdown, never completed, no second status.
    for mut c in streams {
        let (bytes, closed) = read_close(&mut c, Duration::from_secs(10)).await;
        assert!(closed);
        assert!(
            !bytes.ends_with(b"0\r\n\r\n"),
            "stream truncated, not completed"
        );
        assert_eq!(count(&bytes, b"HTTP/1.1"), 0);
        assert_eq!(count(&bytes, b"[DONE]"), 0);
    }
    // Requests still before their headers: the documented 503 not_ready.
    for mut c in stuck.into_iter().chain(partial) {
        let (bytes, closed) = read_close(&mut c, Duration::from_secs(10)).await;
        assert!(closed);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 503"), "{text}");
        assert!(text.contains(r#"{"error":{"code":"not_ready"}}"#));
        markers().assert_clean("shutdown answer", &bytes);
    }
    // Idle connections are closed by the shutdown (no hang).
    for mut c in silent {
        let (bytes, closed) = read_close(&mut c, Duration::from_secs(10)).await;
        assert!(closed, "silent connection closed by the shutdown");
        assert!(bytes.is_empty());
    }
    assert_eq!(lab.metrics.streams_ended(StreamEnd::Shutdown), 8);
    lab.up.peer_closed().await;
    lab.settle().await;
    // No accumulating tasks after the drain.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while alive_tasks() > tasks.saturating_sub(0) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "leaked tasks {} > {}",
            alive_tasks(),
            tasks
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(lab.up.requests(), 14, "no retries during the drain");
}

// ===================================================== write stall at the connection layer

/// The server end of the pipe; tells the test when the HTTP server dropped (closed) it.
struct Tracked {
    inner: StallIo<DuplexStream>,
    closed: Option<oneshot::Sender<()>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(closed) = self.closed.take() {
            let _ = closed.send(());
        }
    }
}

impl tokio::io::AsyncRead for Tracked {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Tracked {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A listener that serves exactly one connection over an in-memory pipe, through the same
/// wrappers production uses (the head guard over the write-stall IO), then idles.
struct PipeListener {
    io: Option<Tracked>,
}

impl axum::serve::Listener for PipeListener {
    type Io = Tracked;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        if let Some(io) = self.io.take() {
            return (io, SocketAddr::from(([127, 0, 0, 1], 0)));
        }
        std::future::pending().await
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
    }
}

/// Serve one in-memory connection with a pipe of `pipe` bytes and the production wrappers.
/// The client end is returned; reading from it is the consumer.
fn serve_pipe(
    route: &Arc<ChatRoute>,
    pipe: usize,
    stall: Duration,
    budget: Option<Duration>,
) -> (DuplexStream, oneshot::Receiver<()>, JoinHandle<()>) {
    let (client, server) = duplex(pipe);
    let mut io = StallIo::new(server, stall);
    if let Some(budget) = budget {
        io = io.with_write_budget(budget);
    }
    let (closed, closed_rx) = oneshot::channel();
    let tracked = Tracked {
        inner: io,
        closed: Some(closed),
    };
    let listener =
        HeadGuardListener::new(PipeListener { io: Some(tracked) }, Duration::from_secs(10));
    let app = crate::server::guarded_app(chat_route::mount(axum::Router::new(), Arc::clone(route)));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (client, closed_rx, task)
}

#[tokio::test]
async fn a_refusal_to_a_consumer_that_never_reads_is_cut_by_the_write_stall() {
    let limits = limits_with(|l| l.stream_write_stall_ms = 300);
    let lab = Lab::new(Script::HoldHeaders, limits).await;
    let (mut client, closed, _server) =
        serve_pipe(&lab.route, 32, limits.stream_write_stall(), None);
    // A refusal (405) larger than the 32-byte pipe: the write cannot complete.
    client
        .write_all(b"GET /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\n\r\n")
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    // Do not read until the server has given up; only the server-side drop closes the pipe.
    within(closed).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "not cut before the stall"
    );
    let mut seen = Vec::new();
    let _ = within(client.read_to_end(&mut seen)).await;
    assert!(
        seen.len() <= 32,
        "only what fit in the pipe was written: {}",
        seen.len()
    );
    assert!(!seen.ends_with(b"}"), "the refusal was not fully delivered");
    lab.assert_zero_forward();
    lab.settle().await;
}

#[tokio::test]
async fn a_buffered_json_response_to_a_consumer_that_never_reads_is_cut_and_returns_its_permit() {
    let limits = limits_with(|l| l.stream_write_stall_ms = 400);
    let lab = Lab::new(Script::JsonBig(20_000), limits).await;
    let (mut client, closed, _server) =
        serve_pipe(&lab.route, 64, limits.stream_write_stall(), None);
    client
        .write_all(&raw_post(&chat_body("hello"), KEY))
        .await
        .unwrap();
    lab.up.received().await;
    // The buffered body is Gateway memory attributable to one upstream permit until it is
    // written or abandoned.
    lab.until("response buffered and held", |l| l.free()[2] == 15)
        .await;
    within(closed).await.unwrap(); // the connection is cut by the stall deadline
    lab.settle().await;
    let mut seen = Vec::new();
    let _ = within(client.read_to_end(&mut seen)).await;
    assert!(seen.len() < 1000, "truncated: {}", seen.len());
    assert_eq!(lab.up.requests(), 1);
    assert_eq!(lab.metrics.upstream_attempts(), 1);
}

#[tokio::test]
async fn a_consumer_that_reads_a_trickle_cannot_hold_a_json_response_past_the_write_budget() {
    // The stall deadline restarts on every byte of progress, so a consumer reading a few
    // bytes inside each stall window is never "stalled". The cumulative write budget (the
    // stream lifetime, since one connection carries one request) is what bounds it.
    let limits = limits_with(|l| {
        l.stream_write_stall_ms = 300;
        l.stream_lifetime_ms = 1500;
    });
    let lab = Lab::new(Script::JsonBig(200_000), limits).await;
    let (mut client, closed, _server) = serve_pipe(
        &lab.route,
        64,
        limits.stream_write_stall(),
        Some(limits.stream_lifetime()),
    );
    client
        .write_all(&raw_post(&chat_body("hello"), KEY))
        .await
        .unwrap();
    lab.up.received().await;
    let started = tokio::time::Instant::now();
    let mut read = 0_usize;
    let mut buf = [0_u8; 16];
    // Read 16 bytes every 100 ms (a third of the stall deadline): 200 KB would take hours.
    let ended = loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match tokio::time::timeout(Duration::from_millis(50), client.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => break true,
            Ok(Ok(n)) => read += n,
            Err(_) => {}
        }
        if started.elapsed() > Duration::from_secs(8) {
            break false;
        }
    };
    assert!(
        ended,
        "the trickle reader was still holding the response after 8 s ({read} bytes)"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1400),
        "cut by the budget, not earlier"
    );
    assert!(read < 20_000, "truncated: {read}");
    within(closed).await.unwrap();
    lab.settle().await;
}

#[tokio::test]
async fn the_production_listener_cuts_a_trickle_reader_of_a_real_connection() {
    // End to end over a real socket and the production listener stack: a 16 MB buffered JSON
    // answer (more than the kernel socket buffers hold, so the write blocks on the reader)
    // and a client with a tiny receive buffer that reads 256 bytes every 100 ms. The server
    // side must give the connection up at the stall deadline and return the upstream
    // permit, long before the client could ever finish (the data already in the kernel's
    // buffers stays readable for the client, which is why this checks the server side).
    let limits = limits_with(|l| {
        l.stream_write_stall_ms = 300;
        l.stream_lifetime_ms = 60_000;
        l.max_response_body_bytes = 32 * 1024 * 1024;
    });
    let lab = Lab::new(Script::JsonBig(16 * 1024 * 1024), limits).await;
    let served = Served::start(&lab, Duration::from_secs(5)).await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut client = socket.connect(served.addr).await.unwrap();
    client
        .write_all(&raw_post(&chat_body("hello"), KEY))
        .await
        .unwrap();
    lab.up.received().await;
    let mut read = 0_usize;
    let mut buf = [0_u8; 256];
    let started = tokio::time::Instant::now();
    while !lab.is_baseline() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the permit was still held after 15 s ({read} bytes read)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(20), client.read(&mut buf)).await
        {
            read += n;
        }
    }
    assert!(read < 1_000_000, "the client was nowhere near done: {read}");
    assert_eq!(lab.up.requests(), 1);
}

#[tokio::test]
async fn shutdown_during_a_blocked_json_write_returns_at_the_drain_plus_grace() {
    // A buffered JSON answer is past its headers once the first bytes are written: the
    // shutdown cancellation cannot change it (there is no second status to send), the
    // write stays bounded by the stall and budget deadlines, and the server's `serve`
    // still returns at the drain deadline plus the fixed grace. The process exit that
    // follows is the final termination of the connection (documented in the lifecycle
    // contract); here the client closing is what returns the permit.
    let limits = limits_with(|l| {
        l.stream_write_stall_ms = 120_000;
        l.stream_lifetime_ms = 600_000;
        l.max_response_body_bytes = 32 * 1024 * 1024;
    });
    let lab = Lab::new(Script::JsonBig(16 * 1024 * 1024), limits).await;
    let mut served = Served::start(&lab, Duration::from_millis(300)).await;
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut client = socket.connect(served.addr).await.unwrap();
    client
        .write_all(&raw_post(&chat_body("hello"), KEY))
        .await
        .unwrap();
    // Reading the first bytes is the event "the response write has begun".
    let head = read_head(&mut client).await;
    assert!(head.starts_with(b"HTTP/1.1 200"));
    assert_eq!(lab.free()[2], 15, "held while the write is in progress");
    let asked = tokio::time::Instant::now();
    served.stop();
    let result = within(&mut served.task).await.unwrap();
    assert!(result.is_ok());
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "serve returned at the drain plus the grace, not at the 120 s stall deadline"
    );
    assert_eq!(
        lab.free()[2],
        15,
        "the unfinished write still owns its permit"
    );
    drop(client);
    lab.settle().await;
    assert_eq!(lab.up.requests(), 1);
}

#[tokio::test]
async fn a_buffered_body_hands_the_server_bounded_frames_and_keeps_the_permit_until_dropped() {
    use http_body::Body as _;
    let admission = Admission::new(&plan(&ROOMY));
    let permit = admission.try_upstream().unwrap();
    let size = super::super::relay::FRAME_BYTES * 3 + 5;
    let response = super::super::UpstreamResponse::new(
        reqwest::StatusCode::OK,
        reqwest::header::HeaderMap::new(),
        vec![b'x'; size],
        permit,
    );
    let (_, _, mut body) = response.into_parts();
    assert_eq!(body.size_hint().exact(), Some(size as u64));
    let mut seen = 0_usize;
    let mut frames = 0_usize;
    while let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let data = frame.unwrap().into_data().unwrap();
        assert!(
            data.len() <= super::super::relay::FRAME_BYTES,
            "bounded frame"
        );
        seen += data.len();
        frames += 1;
        assert_eq!(body.size_hint().exact(), Some((size - seen) as u64));
    }
    assert_eq!((seen, frames), (size, 4));
    assert!(body.is_end_stream());
    // Still owned after the last frame was taken: only dropping the body returns it.
    let held: Vec<_> = (0..ROOMY.upstream)
        .filter_map(|_| admission.try_upstream().ok())
        .collect();
    assert_eq!(held.len(), ROOMY.upstream as usize - 1);
    drop(held);
    drop(body);
    let all: Vec<_> = (0..ROOMY.upstream)
        .filter_map(|_| admission.try_upstream().ok())
        .collect();
    assert_eq!(all.len(), ROOMY.upstream as usize);
}
