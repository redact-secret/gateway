//! Test-only local fake upstream (issue #6).
//!
//! A loopback-only HTTP/1.1 server that stands in for a provider. It is test support:
//! it lives under `tests/`, is never compiled into the binary, needs no credentials, and
//! never leaves the loopback interface.
//!
//! It records every connection, request line, header, and body byte it receives
//! (including partial bodies from aborted requests) so tests can assert exactly what
//! reached "upstream". Its `Debug` output and assertion failures print only counts and
//! header *names*, never header values or body bytes.
//!
//! Failure modes ([`Behavior`]): ordinary JSON, slow responses, disconnect before any
//! response, disconnect mid-body, malformed replies, and SSE delivered in arbitrary
//! fragments (chunked or close-delimited) with per-fragment delays, with or without a
//! clean finish, and an endless chunked SSE stream for backpressure tests. Behaviors can be
//! queued per call. For SSE the fake also records how many body bytes it managed to write
//! and how many streams it saw the peer close (read EOF or a failed write), so tests can
//! assert that the gateway cancelled the upstream exchange.
//!
//! Scope: this models the *upstream*. It does not implement any gateway behavior, and
//! it does not decide what is correct for the gateway; tests do.

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// Largest request body the fake accepts; larger bodies are recorded as incomplete.
const MAX_BODY: usize = 16 * 1024 * 1024;
/// Largest header block the fake accepts.
const MAX_HEADERS: usize = 64 * 1024;

/// How an SSE response is framed on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SseFraming {
    /// `Transfer-Encoding: chunked`; each fragment is one HTTP chunk.
    Chunked,
    /// No length, no chunking; the body ends when the connection closes.
    CloseDelimited,
}

/// One piece of an SSE stream and how long to wait before sending it. Fragments may split
/// events or even UTF-8 sequences at arbitrary byte boundaries.
#[derive(Clone, Debug)]
pub struct SseFragment {
    pub bytes: Vec<u8>,
    pub delay_before: Duration,
}

impl SseFragment {
    #[must_use]
    pub fn now(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            delay_before: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn after(delay: Duration, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            delay_before: delay,
        }
    }
}

/// What the fake does after it has read a request.
#[derive(Clone)]
pub enum Behavior {
    /// Complete `Content-Length` response.
    Json { status: u16, body: Vec<u8> },
    /// Wait, then behave as `then`.
    Slow {
        delay: Duration,
        then: Box<Behavior>,
    },
    /// Close the connection without writing a single response byte.
    DisconnectBeforeResponse,
    /// Declare `declared_len` body bytes, send `sent` (shorter), then close.
    DisconnectMidBody { declared_len: usize, sent: Vec<u8> },
    /// Write exactly these bytes (not necessarily valid HTTP), then close.
    Malformed(Vec<u8>),
    /// `200 text/event-stream` delivered in fragments. When `finish` is false the
    /// connection is dropped abruptly after the last fragment (no terminator).
    Sse {
        fragments: Vec<SseFragment>,
        framing: SseFraming,
        finish: bool,
    },
    /// `200 text/event-stream` (chunked) that writes `chunk` every `interval` until the
    /// peer closes. Never finishes by itself. A zero interval writes as fast as the
    /// socket accepts, so it measures how much backpressure lets through.
    SseEndless { chunk: Vec<u8>, interval: Duration },
}

impl fmt::Debug for Behavior {
    /// Kind only; never bodies.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Json { .. } => "Behavior::Json",
            Self::Slow { .. } => "Behavior::Slow",
            Self::DisconnectBeforeResponse => "Behavior::DisconnectBeforeResponse",
            Self::DisconnectMidBody { .. } => "Behavior::DisconnectMidBody",
            Self::Malformed(_) => "Behavior::Malformed",
            Self::Sse { .. } => "Behavior::Sse",
            Self::SseEndless { .. } => "Behavior::SseEndless",
        })
    }
}

impl Behavior {
    /// `200` with a small synthetic JSON body.
    #[must_use]
    pub fn ok_json() -> Self {
        Self::Json {
            status: 200,
            body: br#"{"synthetic":"fake-upstream-response"}"#.to_vec(),
        }
    }
}

/// One request the fake received.
#[derive(Clone)]
pub struct RecordedCall {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// False when the body was cut short (client aborted) or exceeded the fake's cap.
    pub body_complete: bool,
}

impl RecordedCall {
    /// First header value with this name (ASCII case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[must_use]
    pub fn header_names(&self) -> Vec<&str> {
        self.headers.iter().map(|(n, _)| n.as_str()).collect()
    }
}

impl fmt::Debug for RecordedCall {
    /// Method, path length, header names, and body length only. Never values or bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordedCall")
            .field("method", &self.method)
            .field("path_len", &self.path.len())
            .field("header_names", &self.header_names())
            .field("body_len", &self.body.len())
            .field("body_complete", &self.body_complete)
            .finish()
    }
}

/// Aggregate of what reached the fake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tally {
    /// TCP connections accepted, including ones that sent nothing.
    pub connections: usize,
    /// Requests whose header block was parsed.
    pub calls: usize,
    /// Body bytes received across all calls (including partial bodies).
    pub body_bytes: usize,
}

/// The "nothing was sent upstream" assertion failed.
#[derive(Debug, PartialEq, Eq)]
pub struct ForwardViolation {
    pub tally: Tally,
}

impl fmt::Display for ForwardViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "upstream was reached: {} connection(s), {} call(s), {} body byte(s)",
            self.tally.connections, self.tally.calls, self.tally.body_bytes
        )
    }
}

impl std::error::Error for ForwardViolation {}

struct State {
    /// SSE connections on which the peer was seen to close (EOF or failed write).
    peer_closed: usize,
    /// SSE body bytes successfully written to sockets (before the framing).
    streamed_bytes: usize,
    connections: usize,
    calls: Vec<RecordedCall>,
    script: VecDeque<Behavior>,
    default: Behavior,
}

struct Shared {
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Running fake upstream. Dropping it stops the server and all connection tasks.
pub struct FakeUpstream {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

impl fmt::Debug for FakeUpstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeUpstream")
            .field("addr", &self.addr)
            .field("tally", &self.tally())
            .finish()
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl FakeUpstream {
    /// Start on an ephemeral loopback port answering every call with `default`.
    pub async fn start(default: Behavior) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                peer_closed: 0,
                streamed_bytes: 0,
                connections: 0,
                calls: Vec::new(),
                script: VecDeque::new(),
                default,
            }),
        });
        let accept_shared = Arc::clone(&shared);
        let accept = tokio::spawn(async move {
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        accept_shared.lock().connections += 1;
                        conns.spawn(handle(stream, Arc::clone(&accept_shared)));
                    }
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            shared,
            accept,
        }
    }

    /// `http://127.0.0.1:<port>` (loopback only, plain HTTP).
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Answer the next call with `behavior` (FIFO), before falling back to the default.
    pub fn enqueue(&self, behavior: Behavior) {
        self.shared.lock().script.push_back(behavior);
    }

    /// Replace the default behavior.
    pub fn set_default(&self, behavior: Behavior) {
        self.shared.lock().default = behavior;
    }

    /// SSE connections the fake saw the gateway close.
    #[must_use]
    pub fn peer_closed(&self) -> usize {
        self.shared.lock().peer_closed
    }

    /// SSE body bytes the fake successfully wrote (kernel buffers included).
    #[must_use]
    pub fn streamed_bytes(&self) -> usize {
        self.shared.lock().streamed_bytes
    }

    /// Poll until `peer_closed() >= n` or the timeout elapses.
    pub async fn wait_for_peer_closed(&self, n: usize, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.peer_closed() >= n {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[must_use]
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.shared.lock().calls.clone()
    }

    #[must_use]
    pub fn tally(&self) -> Tally {
        let state = self.shared.lock();
        Tally {
            connections: state.connections,
            calls: state.calls.len(),
            body_bytes: state.calls.iter().map(|c| c.body.len()).sum(),
        }
    }

    /// Zero upstream bytes of any kind: no connection, no call, no body byte.
    pub fn check_nothing_sent(&self) -> Result<(), ForwardViolation> {
        let tally = self.tally();
        if tally.connections == 0 && tally.calls == 0 && tally.body_bytes == 0 {
            Ok(())
        } else {
            Err(ForwardViolation { tally })
        }
    }

    /// Panic (value-free message) unless nothing reached the fake. Use after every
    /// rejected request: a rejected request must produce zero upstream body.
    pub fn assert_nothing_sent(&self) {
        if let Err(v) = self.check_nothing_sent() {
            panic!("{v}");
        }
    }

    /// Poll until `calls() >= n` or the timeout elapses. Returns whether it was reached.
    pub async fn wait_for_calls(&self, n: usize, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.tally().calls >= n {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

enum Chunked {
    Complete(Vec<u8>),
    Incomplete(Vec<u8>),
    Invalid,
}

/// Decode as much of a chunked body as `buf` holds.
fn decode_chunked(buf: &[u8]) -> Chunked {
    let mut body = Vec::new();
    let mut pos = 0;
    loop {
        let Some(line_end) = find(&buf[pos..], b"\r\n") else {
            return Chunked::Incomplete(body);
        };
        let Ok(size_line) = std::str::from_utf8(&buf[pos..pos + line_end]) else {
            return Chunked::Invalid;
        };
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            return Chunked::Invalid;
        };
        let data_start = pos + line_end + 2;
        if size == 0 {
            return Chunked::Complete(body);
        }
        let data_end = data_start + size;
        if buf.len() < data_end + 2 {
            let avail = buf.len().saturating_sub(data_start).min(size);
            body.extend_from_slice(&buf[data_start.min(buf.len())..data_start + avail]);
            return Chunked::Incomplete(body);
        }
        body.extend_from_slice(&buf[data_start..data_end]);
        pos = data_end + 2;
    }
}

async fn handle(mut stream: TcpStream, shared: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0_u8; 16 * 1024];

    // Header block.
    let header_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEADERS {
            return;
        }
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let content_length: Option<usize> = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok());
    let is_chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });

    let index = {
        let mut state = shared.lock();
        state.calls.push(RecordedCall {
            method,
            path,
            headers,
            body: Vec::new(),
            body_complete: false,
        });
        state.calls.len() - 1
    };
    let record = |body: Vec<u8>, complete: bool| {
        let mut state = shared.lock();
        state.calls[index].body = body;
        state.calls[index].body_complete = complete;
    };

    // Body.
    let body_start = header_end + 4;
    let mut complete = false;
    loop {
        let raw = &buf[body_start.min(buf.len())..];
        if is_chunked {
            match decode_chunked(raw) {
                Chunked::Complete(body) => {
                    record(body, true);
                    complete = true;
                    break;
                }
                Chunked::Incomplete(body) => record(body, false),
                Chunked::Invalid => return,
            }
        } else {
            let want = content_length.unwrap_or(0);
            if raw.len() >= want {
                record(raw[..want].to_vec(), true);
                complete = true;
                break;
            }
            record(raw.to_vec(), false);
        }
        if raw.len() > MAX_BODY {
            break;
        }
        match stream.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    if !complete {
        return;
    }

    let behavior = {
        let mut state = shared.lock();
        state
            .script
            .pop_front()
            .unwrap_or_else(|| state.default.clone())
    };
    respond(&mut stream, behavior, &shared).await;
}

/// Wait `delay`; true if the peer closed the connection first (read EOF or error).
async fn sleep_or_closed(stream: &mut TcpStream, delay: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    let mut probe = [0_u8; 64];
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return false,
            read = stream.read(&mut probe) => {
                if matches!(read, Ok(0) | Err(_)) {
                    return true;
                }
            }
        }
    }
}

fn note_closed(shared: &Shared) {
    shared.lock().peer_closed += 1;
}

fn note_streamed(shared: &Shared, n: usize) {
    shared.lock().streamed_bytes += n;
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

async fn respond(stream: &mut TcpStream, behavior: Behavior, shared: &Shared) {
    let mut behavior = behavior;
    while let Behavior::Slow { delay, then } = behavior {
        tokio::time::sleep(delay).await;
        behavior = *then;
    }
    match behavior {
        Behavior::Slow { .. } | Behavior::DisconnectBeforeResponse => {}
        Behavior::Json { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reason(status),
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.shutdown().await;
        }
        Behavior::DisconnectMidBody { declared_len, sent } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&sent).await;
            let _ = stream.flush().await;
        }
        Behavior::Malformed(bytes) => {
            let _ = stream.write_all(&bytes).await;
            let _ = stream.flush().await;
        }
        Behavior::Sse {
            fragments,
            framing,
            finish,
        } => {
            let head = match framing {
                SseFraming::Chunked => {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                }
                SseFraming::CloseDelimited => {
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                }
            };
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for fragment in fragments {
                if !fragment.delay_before.is_zero()
                    && sleep_or_closed(stream, fragment.delay_before).await
                {
                    note_closed(shared);
                    return;
                }
                let ok = match framing {
                    SseFraming::Chunked => {
                        let mut framed = format!("{:x}\r\n", fragment.bytes.len()).into_bytes();
                        framed.extend_from_slice(&fragment.bytes);
                        framed.extend_from_slice(b"\r\n");
                        stream.write_all(&framed).await
                    }
                    SseFraming::CloseDelimited => stream.write_all(&fragment.bytes).await,
                };
                if ok.is_err() || stream.flush().await.is_err() {
                    note_closed(shared);
                    return;
                }
                note_streamed(shared, fragment.bytes.len());
            }
            if finish {
                if framing == SseFraming::Chunked {
                    let _ = stream.write_all(b"0\r\n\r\n").await;
                }
                let _ = stream.shutdown().await;
            }
        }
        Behavior::SseEndless { chunk, interval } => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err() {
                note_closed(shared);
                return;
            }
            let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
            framed.extend_from_slice(&chunk);
            framed.extend_from_slice(b"\r\n");
            loop {
                if stream.write_all(&framed).await.is_err() {
                    note_closed(shared);
                    return;
                }
                note_streamed(shared, chunk.len());
                if !interval.is_zero() && sleep_or_closed(stream, interval).await {
                    note_closed(shared);
                    return;
                }
            }
        }
    }
}
