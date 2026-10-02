//! Accept-time connection bound (#40, ADR 0022) over real loopback sockets, through the
//! production `bind`/`serve` stack (connection bound, write-stall deadline, head guard).
//! All traffic is loopback and all data is synthetic.
//!
//! Observation: a connection that is over the bound is closed by the server without a
//! single response byte; one that is admitted answers the health endpoints. So a probe
//! (`GET /healthz`) says whether a slot is free. The kernel completes a TCP handshake
//! before the server accepts, and accepts in backlog order, so a probe opened after the
//! held connections is decided after them. Waiting for a slot to return polls with a
//! bound; nothing here depends on a fixed sleep being long enough.

#![allow(clippy::expect_used, clippy::arithmetic_side_effects)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redact_secret_gateway::config::{self, RuntimePlan};
use redact_secret_gateway::server::{self, Services, StartupError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

mod support;
use support::leak::{HEADER_MARKER, Markers};

const WAIT: Duration = Duration::from_secs(10);

fn config(max_connections: u32, head_ms: u32, drain_ms: u32) -> String {
    format!(
        r#"{{
    "schema_version": 1,
    "deployment": {{"listener": {{"address": "127.0.0.1:0"}}}},
    "content": {{"profile": "common"}},
    "resources": {{
        "capacity": {{"receipt": 2, "memory_units": 65536, "inspection": 1, "upstream": 1, "stream": 1}},
        "limits": {{"max_connections": {max_connections}, "body_deadline_ms": {head_ms}, "shutdown_drain_ms": {drain_ms}}}
    }}
}}"#
    )
}

struct Running {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

async fn start(max_connections: u32, head_ms: u32, drain_ms: u32) -> Running {
    let plan: Arc<RuntimePlan> = Arc::new(
        config::parse(config(max_connections, head_ms, drain_ms).as_bytes())
            .expect("valid synthetic config"),
    );
    let bound = server::bind(plan, Services::init).await.expect("bind");
    let addr = bound.local_addr().expect("addr");
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(bound.serve(async move {
        let _ = rx.await;
    }));
    Running { addr, stop, task }
}

/// One request on a fresh connection. `Some((status, raw bytes))` when the server answered;
/// `None` when it closed without a byte (what a connection over the bound sees).
async fn request(addr: SocketAddr, raw: &str) -> Option<(u16, Vec<u8>)> {
    let mut s = TcpStream::connect(addr).await.ok()?;
    let _ = s.write_all(raw.as_bytes()).await;
    let mut buf = Vec::new();
    // A reset after the refusal is a close too; bytes read before it still count.
    let read = tokio::time::timeout(WAIT, s.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "the server neither answered nor closed");
    let status = String::from_utf8_lossy(&buf)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())?;
    Some((status, buf))
}

async fn get(addr: SocketAddr, path: &str) -> Option<u16> {
    request(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
    )
    .await
    .map(|(status, _)| status)
}

/// Poll until a probe is admitted (a slot is free), bounded by [`WAIT`].
async fn wait_until_admitted(addr: SocketAddr) {
    let end = Instant::now() + WAIT;
    while get(addr, "/healthz").await != Some(200) {
        assert!(Instant::now() < end, "no connection slot came back");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Open `n` connections that send nothing.
async fn silent(addr: SocketAddr, n: usize) -> Vec<TcpStream> {
    let mut held = Vec::new();
    for _ in 0..n {
        held.push(TcpStream::connect(addr).await.expect("connect"));
    }
    held
}

/// Wait for the server to close `s` (EOF or reset), bounded by [`WAIT`].
async fn wait_closed(s: &mut TcpStream) {
    let mut buf = [0_u8; 64];
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the server did not close the connection");
}

async fn shutdown(running: Running) {
    let _ = running.stop.send(());
    let result = tokio::time::timeout(WAIT, running.task)
        .await
        .expect("serve returns")
        .expect("serve task");
    assert_eq!(result, Ok(()));
}

/// The server has exactly `bound` slots: `bound - 1` held connections still leave room for
/// a probe (capacity is at least `bound`: no slot leaked) and the server refuses once the
/// last slot is taken (capacity is at most `bound`: none was invented). A slot is returned
/// by the server a moment after it closes a socket, so each phase retries within [`WAIT`];
/// the retries cannot hide a leak, which never recovers.
async fn assert_exactly_bound_slots(addr: SocketAddr, bound: usize) {
    let end = Instant::now() + WAIT;
    // At least `bound`.
    let mut held = loop {
        let held = silent(addr, bound.saturating_sub(1)).await;
        if get(addr, "/healthz").await == Some(200) {
            break held;
        }
        drop(held);
        assert!(Instant::now() < end, "fewer than {bound} slots");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    // At most `bound`: keep taking slots until a probe is refused.
    loop {
        held.extend(silent(addr, 1).await);
        if get(addr, "/healthz").await.is_none() {
            break;
        }
        assert!(Instant::now() < end, "more than {bound} slots");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(held);
    wait_until_admitted(addr).await;
}

#[tokio::test]
async fn outcomes_below_the_bound_match_the_default_bound() {
    let bounded = start(6, 60_000, 50).await;
    let default = start(256, 60_000, 50).await;
    // Three of the six slots are held; the requests below, one at a time, use the rest.
    let _held = silent(bounded.addr, 3).await;
    let _held_default = silent(default.addr, 3).await;
    let post = "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    for raw in [
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        "GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        post,
    ] {
        let a = request(bounded.addr, raw).await.expect("answered");
        let b = request(default.addr, raw).await.expect("answered");
        assert_eq!(a.0, b.0, "same status below the bound");
        // Same body (the part after the last blank line); headers such as date vary.
        let body = |bytes: &[u8]| bytes.rsplit(|b| *b == b'\n').next().map(<[u8]>::to_vec);
        assert_eq!(body(&a.1), body(&b.1), "same body below the bound");
    }
    shutdown(bounded).await;
    shutdown(default).await;
}

#[tokio::test]
async fn silent_connections_fill_the_bound_and_the_head_deadline_returns_it() {
    // The head deadline is the only thing that ends a silent connection here, so it is
    // long enough that the refusal check below always lands inside it.
    let running = start(4, 2_000, 50).await;
    let mut held = silent(running.addr, 4).await;
    assert_eq!(
        get(running.addr, "/healthz").await,
        None,
        "refused at the bound"
    );
    // Gate on the event: the server closes each silent connection at its head deadline.
    for s in &mut held {
        wait_closed(s).await;
    }
    wait_until_admitted(running.addr).await;
    assert_exactly_bound_slots(running.addr, 4).await;
    shutdown(running).await;
}

#[tokio::test]
async fn slow_head_connections_hold_slots_until_they_close_and_leak_nothing() {
    let running = start(4, 60_000, 50).await;
    let markers = Markers::empty().with("header", HEADER_MARKER);
    let mut held = Vec::new();
    for _ in 0..4 {
        let mut s = TcpStream::connect(running.addr).await.expect("connect");
        // A head that never finishes. The marker is synthetic.
        s.write_all(format!("GET /healthz HTTP/1.1\r\nX-Probe: {HEADER_MARKER}\r\nHo").as_bytes())
            .await
            .expect("write");
        held.push(s);
    }
    let refused = request(running.addr, "GET /healthz HTTP/1.1\r\n\r\n").await;
    assert!(refused.is_none(), "refused at the bound with no bytes");
    // The held connections are never answered while their head is unfinished.
    for s in &mut held {
        let mut buf = [0_u8; 16];
        let none = tokio::time::timeout(Duration::from_millis(50), s.read(&mut buf)).await;
        assert!(
            none.is_err(),
            "no byte on a connection waiting for its head"
        );
    }
    // Closing one returns a slot.
    drop(held.pop());
    wait_until_admitted(running.addr).await;
    drop(held);
    wait_until_admitted(running.addr).await;
    assert_exactly_bound_slots(running.addr, 4).await;
    shutdown(running).await;
    // What the server wrote to a refused connection: nothing at all.
    markers.assert_clean("refusal bytes", &[]);
}

#[tokio::test]
async fn abandoned_connections_never_leak_slots() {
    let running = start(4, 60_000, 50).await;
    for _round in 0..3 {
        // Far more connections than slots, each closed right after it sends a request:
        // some are admitted and answered into a dead socket, some are refused.
        for _ in 0..64 {
            if let Ok(mut s) = TcpStream::connect(running.addr).await {
                let _ = s
                    .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await;
            }
        }
        wait_until_admitted(running.addr).await;
        assert_exactly_bound_slots(running.addr, 4).await;
    }
    shutdown(running).await;
}

#[tokio::test]
async fn health_endpoints_are_not_exempt_from_the_bound_and_recover_with_it() {
    let running = start(2, 60_000, 50).await;
    let held = silent(running.addr, 2).await;
    for path in ["/healthz", "/readyz"] {
        assert_eq!(get(running.addr, path).await, None, "{path} at the bound");
    }
    drop(held);
    wait_until_admitted(running.addr).await;
    for path in ["/healthz", "/readyz"] {
        assert_eq!(
            get(running.addr, path).await,
            Some(200),
            "{path} after release"
        );
    }
    shutdown(running).await;
}

#[tokio::test]
async fn shutdown_with_the_bound_reached_and_exceeded_stays_bounded() {
    // The head deadline is long, so it is the drain and the fixed grace that end `serve`.
    let running = start(8, 120_000, 50).await;
    let held = silent(running.addr, 8).await;
    let over = silent(running.addr, 8).await;
    let started = Instant::now();
    shutdown(running).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "serve took {:?}",
        started.elapsed()
    );
    drop((held, over));
}
