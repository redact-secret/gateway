//! Boundary tests for the request-header size classes (#41; ADR 0023).
//! Compiled only under `cfg(test)`.
//!
//! Every limit is tested on both sides, through the production connection handling (the
//! head guard, then the HTTP server, then the chat route) in front of a loopback fake
//! provider: the value at the limit is admitted and forwarded once, one more byte (or one
//! more field) is refused with the documented status and code, and every refusal leaves the
//! fake provider with zero connections and the wire without any request-derived byte.
//!
//! The size classes and who answers each are the table in
//! `docs/contracts/errors-and-telemetry.md` ("Request-head size classes"):
//!
//! | Class | Limit | Answered by |
//! | --- | --- | --- |
//! | header names plus values | 16 KiB total | route, `431 limit_exceeded` |
//! | one header value | 8 KiB | route, `431 limit_exceeded` |
//! | header fields | 100 | head guard, fixed `431 limit_exceeded` |
//! | head (line, fields, blank line) | 64 KiB | head guard, fixed `431 limit_exceeded` |
//! | head not finished in time | `body_deadline_ms` | closed, no response |

use super::attack_tests::{exchange, limits_with, serve, valid_with};
use super::fake_upstream::Tally;
use super::forward_tests::{Caps, GOOD, KEY, Rig};
use super::leak::Markers;
use super::*;
use crate::head_guard::{HEAD_TOO_LARGE_RESPONSE, MAX_HEAD_BYTES, MAX_HEAD_FIELDS};
use crate::transport::headers::{MAX_HEADER_BYTES, MAX_HEADER_VALUE_BYTES};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// The body of every route-level or guard-level header refusal.
const LIMIT_BODY: &str = r#"{"error":{"code":"limit_exceeded"}}"#;
const PAD_MARK: &str = "PADMARK0";
/// Longest value one padding header carries, well under the single-value cap.
const PAD_VALUE: usize = 4000;

fn pad_value(n: usize) -> String {
    PAD_MARK.repeat(n / PAD_MARK.len() + 1)[..n].to_owned()
}

/// Header text of the request, without the request line, up to the blank line.
fn head_of(request: &[u8]) -> String {
    let text = String::from_utf8_lossy(request).into_owned();
    let end = text.find("\r\n\r\n").unwrap();
    text[..end].to_owned()
}

/// What the route's total cap counts: the sum of every field's name and value lengths.
fn name_value_bytes(request: &[u8]) -> usize {
    head_of(request)
        .lines()
        .skip(1)
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            name.len() + value.trim_matches([' ', '\t']).len()
        })
        .sum()
}

/// What the guard's bound counts: request line through the blank line.
fn head_bytes(request: &[u8]) -> usize {
    head_of(request).len() + 4
}

/// Padding header lines that add exactly `needed` to a measure in which a line counts
/// `name + value + overhead` bytes (0 for the route's total, 4 for the head: `: ` and CRLF).
fn pad_lines(mut needed: usize, overhead: usize) -> String {
    let mut out = String::new();
    let mut i = 0;
    while needed > 0 {
        let name = format!("X-Pad-{i}");
        let fixed = name.len() + overhead;
        let value = if needed > PAD_VALUE + fixed + 10 {
            PAD_VALUE
        } else {
            needed - fixed
        };
        out.push_str(&format!("{name}: {}\r\n", pad_value(value)));
        needed -= fixed + value;
        i += 1;
    }
    out
}

/// A valid request whose `measure` is exactly `target`.
fn sized(target: usize, measure: fn(&[u8]) -> usize, overhead: usize) -> Vec<u8> {
    let base = measure(&valid_with("", GOOD));
    let request = valid_with(&pad_lines(target - base, overhead), GOOD);
    assert_eq!(measure(&request), target);
    request
}

fn fields(request: &[u8]) -> usize {
    head_of(request).lines().count() - 1
}

fn body_of(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    text.split_once("\r\n\r\n").unwrap().1.to_owned()
}

fn markers() -> Markers {
    Markers::empty().with("key", KEY).with("pad", PAD_MARK)
}

struct Fixture {
    rig: Rig,
    addr: SocketAddr,
    _server: JoinHandle<()>,
    /// What the provider has seen from admitted requests so far; refusals add nothing.
    seen: std::cell::Cell<Tally>,
}

impl Fixture {
    async fn start() -> Self {
        let rig = Rig::with(Behavior::ok_json(), limits_with(|_| {}), Caps::ROOMY).await;
        let (addr, server) = serve(&rig).await;
        Self {
            rig,
            addr,
            _server: server,
            seen: std::cell::Cell::new(Tally {
                connections: 0,
                calls: 0,
                body_bytes: 0,
            }),
        }
    }

    /// Admitted: forwarded exactly once, answered `200`, and nothing the caller padded the
    /// head with reaches the provider.
    async fn assert_admitted(&self, label: &str, request: &[u8]) {
        let before = self.rig.fake.calls().len();
        let wire = exchange(self.addr, request).await;
        assert_eq!(wire.status(), Some(200), "{label}");
        let calls = self.rig.fake.calls();
        assert_eq!(calls.len(), before + 1, "{label}: one upstream call");
        super::attack_tests::assert_wire_is_gateway_built(&self.rig, &calls[before]);
        let sent: String = calls[before]
            .headers
            .iter()
            .map(|(n, v)| format!("{n}{v}"))
            .collect();
        assert!(
            !sent.contains(PAD_MARK),
            "{label}: padding reached upstream"
        );
        self.rig.settle().await;
        self.seen.set(self.rig.fake.tally());
    }

    /// Refused by the route: `431 limit_exceeded` with the gateway's own response (not the
    /// guard's constant), no upstream bytes, no request-derived byte on the wire.
    async fn assert_route_431(&self, label: &str, request: &[u8]) {
        let wire = exchange(self.addr, request).await;
        assert_eq!(wire.status(), Some(431), "{label}");
        assert_eq!(body_of(&wire.bytes), LIMIT_BODY, "{label}");
        assert_ne!(
            wire.bytes, HEAD_TOO_LARGE_RESPONSE,
            "{label}: route answers"
        );
        assert!(wire.closed, "{label}");
        self.assert_refused(label, &wire.bytes);
    }

    /// Refused by the head guard: exactly the fixed constant.
    async fn assert_guard_431(&self, label: &str, request: &[u8]) {
        let wire = exchange(self.addr, request).await;
        assert_eq!(wire.bytes, HEAD_TOO_LARGE_RESPONSE, "{label}");
        assert!(wire.closed, "{label}");
        self.assert_refused(label, &wire.bytes);
    }

    fn assert_refused(&self, label: &str, wire: &[u8]) {
        assert_eq!(
            self.rig.fake.tally(),
            self.seen.get(),
            "{label}: refusal reached the provider"
        );
        markers().assert_clean(label, wire);
    }
}

/// The route's byte-count limits sit under the guard's head bound with room for the
/// separators of the most fields it admits and a long request line, so a head the route
/// would accept is never refused by the guard for size (checked at compile time).
const _: () = {
    assert!(MAX_HEADER_VALUE_BYTES * 2 <= MAX_HEADER_BYTES);
    assert!(MAX_HEADER_BYTES + MAX_HEAD_FIELDS * 4 + 1024 < MAX_HEAD_BYTES);
};

#[tokio::test]
async fn total_header_block_cap_holds_on_both_sides() {
    let fx = Fixture::start().await;
    let at = sized(MAX_HEADER_BYTES, name_value_bytes, 0);
    assert!(fields(&at) <= MAX_HEAD_FIELDS);
    fx.assert_admitted("16384 bytes of names and values", &at)
        .await;
    let over = sized(MAX_HEADER_BYTES + 1, name_value_bytes, 0);
    fx.assert_route_431("16385 bytes of names and values", &over)
        .await;
    // The refusal leaves the server healthy.
    fx.assert_admitted("control", &valid_with("", GOOD)).await;
}

#[tokio::test]
async fn single_value_cap_holds_on_both_sides() {
    let fx = Fixture::start().await;
    let big = |n: usize| valid_with(&format!("X-Big: {}\r\n", pad_value(n)), GOOD);
    fx.assert_admitted("8192-byte value", &big(MAX_HEADER_VALUE_BYTES))
        .await;
    fx.assert_route_431("8193-byte value", &big(MAX_HEADER_VALUE_BYTES + 1))
        .await;
}

#[tokio::test]
async fn header_field_count_holds_on_both_sides() {
    let fx = Fixture::start().await;
    // The valid request carries five fields; pad with small ones to the exact count.
    let with_count = |n: usize| {
        let request = valid_with(
            &"X-N: 1\r\n".repeat(n - fields(&valid_with("", GOOD))),
            GOOD,
        );
        assert_eq!(fields(&request), n);
        request
    };
    fx.assert_admitted("100 fields", &with_count(MAX_HEAD_FIELDS))
        .await;
    fx.assert_guard_431("101 fields", &with_count(MAX_HEAD_FIELDS + 1))
        .await;
    // Far over: the same fixed answer, not a silent close.
    fx.assert_guard_431("1000 fields", &with_count(1000)).await;
}

#[tokio::test]
async fn head_bound_holds_on_both_sides() {
    let fx = Fixture::start().await;
    // A head of exactly 64 KiB is released to the HTTP server, which finds it fine, and
    // the route refuses it for the byte caps it exceeds: the route's answer, not the guard's.
    let at = sized(MAX_HEAD_BYTES, head_bytes, 4);
    assert!(fields(&at) <= MAX_HEAD_FIELDS);
    fx.assert_route_431("head of 65536 bytes", &at).await;
    // One byte more: the guard's fixed answer, however the bytes arrive.
    let over = sized(MAX_HEAD_BYTES + 1, head_bytes, 4);
    fx.assert_guard_431("head of 65537 bytes", &over).await;
    // Delivered in small pieces the outcome is the same.
    let mut stream = TcpStream::connect(fx.addr).await.unwrap();
    for piece in over.chunks(1021) {
        if stream.write_all(piece).await.is_err() {
            break;
        }
    }
    let wire = super::attack_tests::read_all(&mut stream, Duration::from_secs(15)).await;
    assert_eq!(wire.bytes, HEAD_TOO_LARGE_RESPONSE);
    fx.assert_refused("head of 65537 bytes in pieces", &wire.bytes);
    // A head that never ends gets the same answer once it passes the bound.
    let mut endless = b"POST /v1/chat/completions HTTP/1.1\r\n".to_vec();
    while endless.len() <= MAX_HEAD_BYTES + 8192 {
        endless.extend_from_slice(format!("X: {}\r\n", pad_value(1000)).as_bytes());
    }
    fx.assert_guard_431("endless head", &endless).await;
}

#[tokio::test]
async fn a_head_that_does_not_finish_in_time_is_closed_without_a_response() {
    let rig = Rig::with(
        Behavior::ok_json(),
        limits_with(|l| l.body_deadline_ms = 300),
        Caps::ROOMY,
    )
    .await;
    let (addr, _server) = serve(&rig).await;
    // Part of a head, then silence: closed by the server's own deadline, nothing written.
    let wire = exchange(
        addr,
        b"POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\n",
    )
    .await;
    assert!(wire.closed);
    assert!(wire.bytes.is_empty(), "late head is not answered");
    rig.fake.assert_nothing_sent();
}
