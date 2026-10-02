//! In-process health/readiness lifecycle, local rejection, and static-state tests.
//! All traffic is loopback and all data is synthetic.

#![allow(clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use redact_secret_gateway::config::{self, RuntimePlan};
use redact_secret_gateway::health::{self, HealthState};
use redact_secret_gateway::server::{self, Services, StartupError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const MARKER: &str = "SYNTH-SECRET-MARKER-0000";

const CONFIG: &str = r#"{
    "schema_version": 1,
    "deployment": {"listener": {"address": "127.0.0.1:0"}},
    "content": {"profile": "common"},
    "resources": {"capacity": {
        "receipt": 1, "memory_units": 1, "inspection": 1, "upstream": 1, "stream": 1
    }}
}"#;

fn plan() -> Arc<RuntimePlan> {
    Arc::new(config::parse(CONFIG.as_bytes()).expect("valid synthetic config"))
}

/// Send one raw HTTP/1.1 request and return (status, headers+body text).
async fn raw(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.expect("connect");
    s.write_all(request.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf))
        .await
        .expect("no timeout")
        .expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("status line");
    (status, text)
}

async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    raw(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    )
    .await
}

struct Running {
    addr: SocketAddr,
    health: Arc<HealthState>,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

async fn start(init: impl FnOnce(&RuntimePlan) -> Result<Services, StartupError>) -> Running {
    let bound = server::bind(plan(), init).await.expect("bind");
    let addr = bound.local_addr().expect("addr");
    let health = bound.health();
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(bound.serve(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        health,
        stop,
        task,
    }
}

#[tokio::test]
async fn listener_is_loopback_and_lifecycle_is_not_ready_then_ready_then_stopped() {
    let bound = server::bind(plan(), Services::init).await.expect("bind");
    let health = bound.health();
    // Initialized and bound, but not yet accepting: not ready.
    assert!(!health.is_ready());
    let addr = bound.local_addr().expect("addr");
    assert!(addr.ip().is_loopback());

    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(bound.serve(async move {
        let _ = rx.await;
    }));
    let (status, body) = get(addr, "/healthz").await;
    assert_eq!(status, 200);
    assert!(body.contains(r#"{"status":"live"}"#));
    let (status, body) = get(addr, "/readyz").await;
    assert_eq!(status, 200);
    assert!(body.contains(r#"{"status":"ready"}"#));
    assert!(health.is_ready());

    stop.send(()).expect("signal shutdown");
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("graceful shutdown completes")
        .expect("join");
    assert_eq!(result, Ok(()));
    assert!(!health.is_ready());
    assert!(TcpStream::connect(addr).await.is_err(), "listener closed");
}

#[tokio::test]
async fn failed_initialization_refuses_to_start() {
    let err = server::bind(plan(), |_| Err(StartupError::Init))
        .await
        .expect_err("init failure must not bind");
    assert_eq!(err, StartupError::Init);
}

#[tokio::test]
async fn readiness_is_false_until_initialization_is_marked() {
    // A plan exists and the process is alive, but initialization was never marked.
    let state = Arc::new(HealthState::new(plan()));
    state.set_accepting(true);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, health::router(Arc::clone(&state))).await;
    });
    let (status, body) = get(addr, "/readyz").await;
    assert_eq!(status, 503);
    assert!(body.contains(r#""code":"not_ready""#));
    let (status, _) = get(addr, "/healthz").await;
    assert_eq!(status, 200, "liveness does not depend on readiness");
}

#[tokio::test]
async fn proxy_routes_are_rejected_locally_without_upstream_calls_or_echo() {
    // A canary listener stands in for any upstream the request might try to reach.
    let canary = std::net::TcpListener::bind("127.0.0.1:0").expect("canary");
    canary.set_nonblocking(true).expect("nonblocking");
    let canary_addr = canary.local_addr().expect("canary addr");

    let run = start(Services::init).await;
    let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{MARKER}"}}]}}"#);
    let requests = [
        format!(
            "POST /v1/chat/completions?k={MARKER} HTTP/1.1\r\nHost: {canary_addr}\r\n\
             Authorization: Bearer {MARKER}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        // Absolute-form target naming the canary.
        format!(
            "POST http://{canary_addr}/v1/chat/completions HTTP/1.1\r\nHost: {canary_addr}\r\n\
             X-Api-Key: {MARKER}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        format!("GET /v1/models/{MARKER} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        // Wrong method on a health path.
        format!(
            "POST /healthz HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        // Health paths are never routes to anything else.
        "GET /healthz/../v1/chat/completions HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
            .to_owned(),
    ];
    for req in &requests {
        let (status, text) = raw(run.addr, req).await;
        assert!(status == 404 || status == 400, "unexpected status {status}");
        if status == 404 {
            assert!(text.contains(r#""code":"unsupported_input""#));
        }
        assert!(!text.contains(MARKER), "response echoed request content");
    }
    assert!(
        matches!(
            canary.accept(),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "no upstream connection may be attempted"
    );
    // The server stays ready after rejecting traffic.
    assert!(run.health.is_ready());
    run.stop.send(()).expect("stop");
    run.task.await.expect("join").expect("clean");
}

#[tokio::test]
async fn repeated_requests_do_not_reinitialize_services() {
    let inits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&inits);
    let run = start(move |plan| {
        counter.fetch_add(1, Ordering::SeqCst);
        Services::init(plan)
    })
    .await;
    for _ in 0..25 {
        assert_eq!(get(run.addr, "/readyz").await.0, 200);
        assert_eq!(get(run.addr, "/nope").await.0, 404);
    }
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    run.stop.send(()).expect("stop");
    run.task.await.expect("join").expect("clean");
}
