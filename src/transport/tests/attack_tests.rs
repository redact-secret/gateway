//! Adversarial qualification suite: routing, credential, and framing boundaries (#25).
//! Compiled only under `cfg(test)`.
//!
//! These tests drive the **production connection handling** (`server::guarded_listener`
//! and `server::guarded_app`: write-stall deadline, request-head guard, `Connection:
//! close`) with raw sockets, in front of the real chat route, the real pinned-core
//! inspection, and the central transport pointed at loopback fake providers through the
//! test-only destination constructors of #23. There is deliberately no production path to
//! such a fake (tests/destination_policy.rs), so this suite lives in the crate; the
//! black-box counterparts that need no fake are in `tests/attack_surface.rs`.
//!
//! Every value is synthetic: revoked-looking keys and invented markers, nothing leaves
//! loopback, nothing needs a provider key. Rejection proof here means "the fake provider
//! saw zero connections or zero request bytes"; it is never a claim that a detector finds
//! every secret (the body fuzz asserts only what must hold for the exact synthetic token).
//!
//! Control map: `docs/qualification/alpha1-threat-control-map.md`.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::fake_upstream::{SseFragment, SseFraming};
use super::forward_tests::{Caps, GOOD, KEY, Rig, TOKEN, alive_tasks, chat_body};
use super::leak::Markers;
use super::*;
use crate::chat_route;
use crate::head_guard::{
    AMBIGUOUS_FRAMING_RESPONSE, HEAD_TOO_LARGE_RESPONSE, HeadVerdict, MAX_HEAD_BYTES, inspect_head,
};
use crate::transport::headers::WIRE_HEADER_NAMES;

const HOSTILE: &str = "SYNTH-HOSTILE-ROUTE-6F3A";
const EVIL_HOST: &str = "evil.test";

// ------------------------------------------------------------------------------ harness

/// Serve the route through the production connection handling.
async fn serve(rig: &Rig) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::server::guarded_app(chat_route::mount(
        axum::Router::new(),
        Arc::clone(&rig.route),
    ));
    let listener = crate::server::guarded_listener(listener, &rig.limits);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, task)
}

/// Everything the gateway wrote on one connection.
struct Wire {
    bytes: Vec<u8>,
    /// The server closed the connection (EOF or reset) before the read bound.
    closed: bool,
}

impl Wire {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    fn status(&self) -> Option<u16> {
        let text = self.text();
        let rest = text.strip_prefix("HTTP/1.1 ")?;
        rest.get(..3)?.parse().ok()
    }

    /// Case-insensitive header presence in the response head.
    fn has_header(&self, name: &str) -> bool {
        let text = self.text().to_ascii_lowercase();
        let head = text.split("\r\n\r\n").next().unwrap_or_default().to_owned();
        head.lines()
            .any(|l| l.starts_with(&format!("{}:", name.to_ascii_lowercase())))
    }
}

async fn read_all(stream: &mut TcpStream, within: Duration) -> Wire {
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 8192];
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Err(_) => {
                return Wire {
                    bytes,
                    closed: false,
                };
            }
            Ok(Ok(0) | Err(_)) => {
                return Wire {
                    bytes,
                    closed: true,
                };
            }
            Ok(Ok(n)) => bytes.extend_from_slice(&buf[..n]),
        }
    }
}

/// Write `request`, read until the server closes (bounded generously: closing is driven by
/// the server's own events, not by this bound).
async fn exchange(addr: SocketAddr, request: &[u8]) -> Wire {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let _ = stream.write_all(request).await;
    read_all(&mut stream, Duration::from_secs(15)).await
}

fn request_with(
    method: &str,
    target: &str,
    host: &str,
    extra: &str,
    framing: &str,
    body: &str,
) -> Vec<u8> {
    keyed_request(KEY, method, target, host, extra, framing, body)
}

fn keyed_request(
    key: &str,
    method: &str,
    target: &str,
    host: &str,
    extra: &str,
    framing: &str,
    body: &str,
) -> Vec<u8> {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Authorization: Bearer {key}\r\n{framing}Connection: close\r\n{extra}\r\n{body}"
    )
    .into_bytes()
}

/// A valid request with `Content-Length` framing plus `extra` header lines.
fn valid_with(extra: &str, body: &str) -> Vec<u8> {
    request_with(
        "POST",
        PATH,
        "gw.test",
        extra,
        &format!("Content-Length: {}\r\n", body.len()),
        body,
    )
}

fn chunked(body: &str) -> String {
    format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len())
}

fn json_eq_good(bytes: &[u8]) -> bool {
    let a: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let b: serde_json::Value = serde_json::from_str(GOOD).unwrap();
    a == b
}

/// Every header the fake saw is one the gateway builds, `Host` is the reviewed destination
/// (the fake's own address), and no hostile value or the evil host reached the wire.
fn assert_wire_is_gateway_built(rig: &Rig, call: &super::fake_upstream::RecordedCall) {
    assert_eq!(call.method, "POST");
    assert_eq!(call.path, PATH);
    let allowed: BTreeSet<&str> = WIRE_HEADER_NAMES.iter().copied().chain(["host"]).collect();
    for name in call.header_names() {
        assert!(
            allowed.contains(name.to_ascii_lowercase().as_str()),
            "unexpected upstream header {name}"
        );
    }
    assert_eq!(call.header("host"), Some(&*rig.fake.addr().to_string()));
    let markers = Markers::empty()
        .with("hostile", HOSTILE)
        .with("evil-host", EVIL_HOST);
    let mut seen = String::new();
    for (name, value) in &call.headers {
        if !name.eq_ignore_ascii_case("authorization") {
            seen.push_str(name);
            seen.push_str(value);
            seen.push('\n');
        }
    }
    markers.assert_clean("upstream headers", seen.as_bytes());
    markers.assert_clean("upstream body", &call.body);
}

fn sse_ok() -> Behavior {
    Behavior::Sse {
        fragments: vec![SseFragment::now(
            &b"data: {\"synthetic\":1}\n\ndata: [DONE]\n\n"[..],
        )],
        framing: SseFraming::Chunked,
        finish: true,
    }
}

fn limits_with(f: impl FnOnce(&mut RequestLimits)) -> RequestLimits {
    let mut limits = RequestLimits::provisional();
    f(&mut limits);
    limits
}

/// Deterministic generator (xorshift64*) so fuzz-style cases are reproducible from the seed
/// committed in the test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

// ------------------------------------------------------------- destination overrides

#[tokio::test]
async fn hostile_request_targets_and_methods_never_reach_a_provider() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let evil = FakeUpstream::start(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    let abs = format!("http://{}{PATH}", evil.addr());
    let userinfo = format!("http://user:{HOSTILE}@{}{PATH}", evil.addr());

    let post_targets: Vec<String> = vec![
        abs,
        userinfo,
        format!("https://api.openai.com{PATH}"),
        format!("http://{EVIL_HOST}{PATH}"),
        format!("//{EVIL_HOST}{PATH}"),
        "//v1/chat/completions".into(),
        "///v1/chat/completions".into(),
        "/v1/chat/completions/".into(),
        "/v1/chat/completions//".into(),
        "/V1/chat/completions".into(),
        "/v1/Chat/Completions".into(),
        "/v1/chat/completions%00".into(),
        "/v1/%63hat/completions".into(),
        "/v1/chat/completions%2f".into(),
        "/v1/chat/%2e%2e/chat/completions".into(),
        "/v1/chat/../chat/completions".into(),
        "/v1/./chat/completions".into(),
        "/%2e/v1/chat/completions".into(),
        "/v1/chat/completions;x=1".into(),
        "/v1/chat/completions?".into(),
        "/v1/chat/completions?model=x".into(),
        format!("/v1/chat/completions?u=http://{EVIL_HOST}/"),
        "/v1/chat/completions\\".into(),
        "/v1/chat/completions%0d%0aHost:%20evil".into(),
        "*".into(),
        format!("{EVIL_HOST}:443"),
        String::new(),
    ];
    let mut cases: Vec<(String, String)> = post_targets
        .into_iter()
        .map(|t| ("POST".to_owned(), t))
        .collect();
    for method in [
        "GET", "HEAD", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE", "PROPFIND", "post",
    ] {
        cases.push((method.to_owned(), PATH.to_owned()));
    }
    for target in [
        format!("{EVIL_HOST}:443"),
        "api.openai.com:443".to_owned(),
        PATH.to_owned(),
        format!("http://{EVIL_HOST}{PATH}"),
    ] {
        cases.push(("CONNECT".to_owned(), target));
    }

    for (method, target) in cases {
        let body = GOOD;
        let wire = exchange(
            addr,
            &request_with(
                &method,
                &target,
                "gw.test",
                "",
                &format!("Content-Length: {}\r\n", body.len()),
                body,
            ),
        )
        .await;
        assert!(wire.closed, "{method} {target}: connection must close");
        if let Some(status) = wire.status() {
            assert!(
                (400..500).contains(&status),
                "{method} {target}: unexpected status {status}"
            );
        }
        // Rejected before the transport: nothing reached either destination, and the
        // credential and the marker host appear nowhere in what the gateway wrote.
        rig.fake.assert_nothing_sent();
        evil.assert_nothing_sent();
        Markers::empty()
            .with("key", KEY)
            .with("hostile", HOSTILE)
            .assert_clean("response", &wire.bytes);
    }
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn upgrade_websocket_and_h2c_attempts_never_switch_protocols_or_forward() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    let attempts: Vec<(&str, &str)> = vec![
        (
            "POST",
            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: c3ludGhldGljLW5vdC1hLWtleQ==\r\nSec-WebSocket-Version: 13\r\n",
        ),
        (
            "POST",
            "Connection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nHTTP2-Settings: AAMAAABkAAQAAP__\r\n",
        ),
        ("POST", "Upgrade: TLS/1.2\r\n"),
        ("POST", "Connection: keep-alive, Upgrade\r\n"),
        (
            "GET",
            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: c3ludGhldGljLW5vdC1hLWtleQ==\r\nSec-WebSocket-Version: 13\r\n",
        ),
    ];
    for (method, extra) in attempts {
        let wire = exchange(
            addr,
            &request_with(
                method,
                PATH,
                "gw.test",
                extra,
                &format!("Content-Length: {}\r\n", GOOD.len()),
                GOOD,
            ),
        )
        .await;
        assert!(wire.closed);
        let status = wire.status().expect("a gateway response");
        assert_ne!(status, 101, "protocol switch must never happen");
        assert!((400..500).contains(&status), "status {status}");
        assert!(!wire.has_header("upgrade"));
        rig.fake.assert_nothing_sent();
    }
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn hostile_routing_headers_cannot_change_what_the_provider_receives() {
    // JSON and SSE share one assertion: both reach the provider only as gateway-built
    // requests to the reviewed destination.
    for streaming in [false, true] {
        let behavior = if streaming {
            sse_ok()
        } else {
            Behavior::ok_json()
        };
        let rig = Rig::new(behavior).await;
        let evil = FakeUpstream::start(Behavior::ok_json()).await;
        let (addr, server) = serve(&rig).await;
        let extra = format!(
            "X-Forwarded-Host: {EVIL_HOST}\r\nX-Forwarded-For: 203.0.113.9\r\n\
             X-Forwarded-Proto: http\r\nX-Forwarded-Port: 8443\r\n\
             Forwarded: for=203.0.113.9;host={EVIL_HOST};proto=http\r\n\
             X-Original-URL: http://{}{PATH}\r\nX-Rewrite-URL: http://{EVIL_HOST}/\r\n\
             X-HTTP-Method-Override: DELETE\r\nX-Method-Override: PUT\r\n\
             Via: 1.1 {EVIL_HOST}\r\nProxy-Authorization: Basic {HOSTILE}\r\n\
             Proxy-Connection: keep-alive\r\nCookie: s={HOSTILE}\r\nReferer: http://{EVIL_HOST}/\r\n\
             Origin: http://{EVIL_HOST}\r\nX-Api-Key: {HOSTILE}\r\napi-key: {HOSTILE}\r\n\
             X-Real-IP: 203.0.113.9\r\nContent-Location: http://{EVIL_HOST}/\r\n\
             OpenAI-Beta: {HOSTILE}\r\nX-Stainless-Lang: {HOSTILE}\r\nTE: trailers\r\n\
             Trailer: X-{HOSTILE}\r\nKeep-Alive: timeout=99\r\nAccept: text/html\r\n\
             Accept-Encoding: gzip, br\r\nUser-Agent: {HOSTILE}\r\n\
             X-Gateway-Local-Token: {HOSTILE}\r\nX-Request-Id: {HOSTILE}\r\n",
            evil.addr()
        );
        let body = if streaming {
            r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}],"stream":true}"#
        } else {
            GOOD
        };
        // The real `Host` header is hostile too.
        let wire = exchange(
            addr,
            &request_with(
                "POST",
                PATH,
                &format!("{EVIL_HOST}:8443"),
                &extra,
                &format!("Content-Length: {}\r\n", body.len()),
                body,
            ),
        )
        .await;
        assert_eq!(wire.status(), Some(200), "streaming={streaming}");
        assert!(wire.closed);
        let calls = rig.fake.calls();
        assert_eq!(calls.len(), 1);
        assert_wire_is_gateway_built(&rig, &calls[0]);
        assert_eq!(
            calls[0].header("authorization"),
            Some(&*format!("Bearer {KEY}"))
        );
        evil.assert_nothing_sent();
        Markers::empty()
            .with("hostile", HOSTILE)
            .with("key", KEY)
            .assert_clean("response", &wire.bytes);
        rig.settle().await;
        server.abort();
    }
}

// ------------------------------------------------------------------------- redirects

#[tokio::test]
async fn provider_redirects_are_returned_not_followed_on_json_and_sse_paths() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let evil = FakeUpstream::start(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    let streaming =
        r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    let mut expected_calls = 0_usize;
    for status in [301_u16, 302, 303, 307, 308] {
        for location in [
            format!("http://{}{PATH}", evil.addr()),
            format!("http://{EVIL_HOST}/"),
            "/second".to_owned(),
            format!("http://{}{PATH}", rig.fake.addr()),
        ] {
            let raw = format!(
                "HTTP/1.1 {status} Moved\r\nLocation: {location}\r\nSet-Cookie: s={HOSTILE}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            rig.fake.set_default(Behavior::Malformed(raw.into_bytes()));
            for body in [GOOD, streaming] {
                let wire = exchange(addr, &valid_with("", body)).await;
                expected_calls += 1;
                assert_eq!(wire.status(), Some(status), "{status} {location}");
                // The provider's redirect is relayed as a response, minus the headers the
                // allowlist drops; nothing is followed and the credential goes nowhere new.
                assert!(!wire.has_header("location"));
                assert!(!wire.has_header("set-cookie"));
                assert_eq!(
                    rig.fake.calls().len(),
                    expected_calls,
                    "exactly one upstream request per client request"
                );
                evil.assert_nothing_sent();
                Markers::empty()
                    .with("hostile", HOSTILE)
                    .assert_clean("response", &wire.bytes);
            }
        }
    }
    rig.settle().await;
    server.abort();
}

// ----------------------------------------------------------------- framing ambiguity

/// A head/body pair the HTTP layer or the route must refuse, with its label.
fn rejected_framing_cases() -> Vec<(&'static str, Vec<u8>)> {
    let n = GOOD.len();
    let ch = chunked(GOOD);
    let with = |fields: &str, body: &str| request_with("POST", PATH, "gw.test", "", fields, body);
    let with_extra = |extra: &str, fields: &str, body: &str| {
        request_with("POST", PATH, "gw.test", extra, fields, body)
    };
    let mut cases: Vec<(&'static str, Vec<u8>)> = vec![
        (
            "CL then TE",
            with(
                &format!("Content-Length: {n}\r\nTransfer-Encoding: chunked\r\n"),
                &ch,
            ),
        ),
        (
            "TE then CL",
            with(
                &format!("Transfer-Encoding: chunked\r\nContent-Length: {n}\r\n"),
                &ch,
            ),
        ),
        (
            "TE then invalid CL",
            with(
                "Transfer-Encoding: chunked\r\nContent-Length: not-a-number\r\n",
                &ch,
            ),
        ),
        (
            "mixed-case names CL+TE",
            with(
                &format!("cOnTeNt-LeNgTh: {n}\r\ntRaNsFeR-eNcOdInG: chunked\r\n"),
                &ch,
            ),
        ),
        (
            "TE name with trailing space",
            with(
                &format!("Content-Length: {n}\r\nTransfer-Encoding : chunked\r\n"),
                &ch,
            ),
        ),
        (
            "TE injected through a bare LF in a value",
            with_extra(
                "X-A: b\nTransfer-Encoding: chunked\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "TE injected through a bare CR in a value",
            with_extra(
                "X-A: b\rTransfer-Encoding: chunked\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "obsolete folding onto TE",
            with_extra(
                "X-Fold: a\r\n Transfer-Encoding: chunked\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "conflicting CL values",
            with(
                &format!("Content-Length: {n}\r\nContent-Length: {}\r\n", n + 1),
                GOOD,
            ),
        ),
        (
            "CL list",
            with(&format!("Content-Length: {n}, {n}\r\n"), GOOD),
        ),
        ("CL signed", with(&format!("Content-Length: +{n}\r\n"), GOOD)),
        ("CL hex", with("Content-Length: 0x10\r\n", GOOD)),
        (
            "CL overflow",
            with("Content-Length: 99999999999999999999999\r\n", GOOD),
        ),
        ("CL empty", with("Content-Length: \r\n", GOOD)),
        (
            "CL name with trailing space",
            with(&format!("Content-Length : {n}\r\n"), GOOD),
        ),
        (
            "CL declared above what arrives",
            with(&format!("Content-Length: {}\r\n", n + 50), GOOD),
        ),
        ("CL zero", with("Content-Length: 0\r\n", "")),
        ("no framing at all", with("", "")),
        ("TE chunked twice in one value", with("Transfer-Encoding: chunked, chunked\r\n", &ch)),
        ("TE identity", with("Transfer-Encoding: identity\r\n", &ch)),
        ("TE x-prefixed", with("Transfer-Encoding: xchunked\r\n", &ch)),
        ("TE parameter", with("Transfer-Encoding: chunked;q=1\r\n", &ch)),
        ("TE gzip then chunked", with("Transfer-Encoding: gzip, chunked\r\n", &ch)),
        ("TE chunked then gzip", with("Transfer-Encoding: chunked, gzip\r\n", &ch)),
        ("TE empty", with("Transfer-Encoding: \r\n", &ch)),
        (
            "TE chunked in two fields",
            with("Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n", &ch),
        ),
        ("chunk size not hex", with("Transfer-Encoding: chunked\r\n", "ZZ\r\nabc\r\n0\r\n\r\n")),
        (
            "chunk size overflow",
            with("Transfer-Encoding: chunked\r\n", "FFFFFFFFFFFFFFFFFFFF\r\nabc\r\n0\r\n\r\n"),
        ),
        (
            "chunk size negative",
            with("Transfer-Encoding: chunked\r\n", "-1\r\nabc\r\n0\r\n\r\n"),
        ),
        (
            "chunk larger than the body bound",
            with("Transfer-Encoding: chunked\r\n", "FFFFFF\r\nabc\r\n0\r\n\r\n"),
        ),
        (
            "chunked body never terminated",
            with("Transfer-Encoding: chunked\r\n", &format!("{:x}\r\n{GOOD}\r\n", n)),
        ),
        ("content-encoding gzip", with_extra(
            "Content-Encoding: gzip\r\n",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        )),
        (
            "Expect other than 100-continue",
            with_extra("Expect: 200-ok\r\n", &format!("Content-Length: {n}\r\n"), GOOD),
        ),
        (
            "Expect list",
            with_extra(
                "Expect: 100-continue, x\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "Connection nominates Content-Length",
            with_extra(
                "Connection: Content-Length\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "Connection nominates Transfer-Encoding",
            with_extra(
                "Connection: Transfer-Encoding\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "NUL in a header value",
            with_extra(
                "X-A: a\0b\r\n",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "space inside a header name",
            with_extra("Bad Name: x\r\n", &format!("Content-Length: {n}\r\n"), GOOD),
        ),
        (
            "header line without a colon",
            with_extra("NoColonHere\r\n", &format!("Content-Length: {n}\r\n"), GOOD),
        ),
        (
            "oversized header value",
            with_extra(
                &format!("X-Big: {}\r\n", "a".repeat(20_000)),
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "header block far over every bound",
            with_extra(
                &format!("X-Big: {}\r\n", "a".repeat(120_000)),
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "very many headers",
            with_extra(
                &"X-N: 1\r\n".repeat(5_000),
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "oversized request target",
            format!(
                "POST /{} HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\n\
                 Authorization: Bearer {KEY}\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n{GOOD}",
                "a".repeat(100_000)
            )
            .into_bytes(),
        ),
        (
            "duplicate Authorization",
            with_extra(
                &format!("Authorization: Bearer {KEY}\r\n"),
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "unsupported HTTP version",
            format!(
                "POST {PATH} HTTP/1.2\r\nHost: gw.test\r\nContent-Type: application/json\r\n\
                 Authorization: Bearer {KEY}\r\nContent-Length: {n}\r\n\r\n{GOOD}"
            )
            .into_bytes(),
        ),
        (
            "HTTP/1.0 with transfer coding",
            format!(
                "POST {PATH} HTTP/1.0\r\nContent-Type: application/json\r\n\
                 Authorization: Bearer {KEY}\r\nTransfer-Encoding: chunked\r\n\r\n{ch}"
            )
            .into_bytes(),
        ),
        (
            "bare LF head end with CL and TE",
            format!(
                "POST {PATH} HTTP/1.1\nHost: gw.test\nContent-Type: application/json\n\
                 Authorization: Bearer {KEY}\nContent-Length: {n}\nTransfer-Encoding: chunked\n\n{ch}"
            )
            .into_bytes(),
        ),
    ];
    // A synthetic body that is itself an attack on the framing must still be inert.
    cases.push((
        "request line smuggled inside a header value",
        with_extra(
            &format!("X-Smuggle: a\r\n\r\nPOST {PATH} HTTP/1.1\r\n"),
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
    ));
    cases
}

#[tokio::test]
async fn ambiguous_and_malformed_framing_never_delivers_upstream_bytes() {
    // A short body deadline makes the "declared more than arrives" and "never terminated"
    // cases end quickly; it changes no outcome, only how soon the server gives up.
    let limits = limits_with(|l| l.body_deadline_ms = 1000);
    let rig = Rig::with(Behavior::ok_json(), limits, Caps::ROOMY).await;
    let (addr, server) = serve(&rig).await;
    let mut closed_without_response: Vec<&str> = Vec::new();
    let mut answered_by_guard = 0_usize;
    let mut too_large = 0_usize;
    let cases = rejected_framing_cases();
    assert!(
        cases.len() > 45,
        "the table must stay broad: {}",
        cases.len()
    );
    for (label, bytes) in cases {
        let wire = exchange(addr, &bytes).await;
        assert!(wire.closed, "{label}: connection must close");
        match wire.status() {
            None => {
                assert!(wire.bytes.is_empty(), "{label}: only a clean close");
                closed_without_response.push(label);
            }
            Some(status) => {
                assert!((400..500).contains(&status), "{label}: status {status}");
                assert!(wire.has_header("connection"), "{label}: Connection: close");
            }
        }
        // A head the guard judges ambiguous is answered with exactly the fixed local
        // refusal of the error contract (#43), never by the parser or the route.
        if inspect_head(&bytes) == HeadVerdict::Ambiguous {
            assert_eq!(wire.bytes, AMBIGUOUS_FRAMING_RESPONSE, "{label}");
            answered_by_guard += 1;
        }
        // A head longer than the guard's hold bound gets the fixed `431`, and only then.
        if wire.bytes == HEAD_TOO_LARGE_RESPONSE {
            assert!(bytes.len() > MAX_HEAD_BYTES, "{label}");
            too_large += 1;
        }
        // Zero connections and zero bytes at the provider, for every refused request.
        rig.fake.assert_nothing_sent();
        Markers::empty()
            .with("key", KEY)
            .assert_clean(label, &wire.bytes);
    }
    // The connection-level cases above are answered by the head guard, not left silent.
    assert!(answered_by_guard >= 5, "{answered_by_guard}");
    assert!(
        too_large >= 1,
        "the table must include a head over the hold bound"
    );
    assert!(
        closed_without_response.is_empty(),
        "closed without any answer: {closed_without_response:?}"
    );
    rig.settle().await;
    // Control: the server is still healthy and forwards a clean request exactly once.
    let wire = exchange(addr, &valid_with("", GOOD)).await;
    assert_eq!(wire.status(), Some(200));
    assert_eq!(rig.fake.calls().len(), 1);
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn framing_forms_the_gateway_admits_are_forwarded_exactly_once_and_inert() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    let n = GOOD.len();
    let smuggled = format!(
        "POST {PATH} HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\n\
         Authorization: Bearer {KEY}\r\nContent-Length: 12\r\n\r\n{{\"{HOSTILE}\":1}}"
    );
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("plain content length", valid_with("", GOOD)),
        (
            "identical duplicate content length",
            request_with(
                "POST",
                PATH,
                "gw.test",
                "",
                &format!("Content-Length: {n}\r\nContent-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "content length with optional whitespace",
            request_with(
                "POST",
                PATH,
                "gw.test",
                "",
                &format!("Content-Length:   {n}   \r\n"),
                GOOD,
            ),
        ),
        (
            "chunked, mixed-case coding",
            request_with(
                "POST",
                PATH,
                "gw.test",
                "",
                "Transfer-Encoding: Chunked\r\n",
                &chunked(GOOD),
            ),
        ),
        (
            "chunked with extension and trailer",
            request_with(
                "POST",
                PATH,
                "gw.test",
                "",
                &format!("Transfer-Encoding: chunked\r\nTrailer: X-{HOSTILE}\r\n"),
                &format!(
                    "{:x};ext={HOSTILE}\r\n{GOOD}\r\n0\r\nX-{HOSTILE}: v\r\n\r\n",
                    n
                ),
            ),
        ),
        (
            "bytes after the body (smuggled second message)",
            [valid_with("", GOOD), smuggled.clone().into_bytes()].concat(),
        ),
        (
            "pipelined second request",
            [
                valid_with("", GOOD),
                valid_with("", &format!("{{\"{HOSTILE}\":1}}")),
            ]
            .concat(),
        ),
        (
            // The URI parser drops a fragment, so the request is the exact route and the
            // fragment text goes nowhere (it never reaches the provider or the response).
            "fragment in the request target",
            request_with(
                "POST",
                &format!("{PATH}#{HOSTILE}"),
                "gw.test",
                "",
                &format!("Content-Length: {n}\r\n"),
                GOOD,
            ),
        ),
        (
            "upper-case header names",
            format!(
                "POST {PATH} HTTP/1.1\r\nHOST: gw.test\r\nCONTENT-TYPE: application/json\r\n\
                 AUTHORIZATION: Bearer {KEY}\r\nCONTENT-LENGTH: {n}\r\n\r\n{GOOD}"
            )
            .into_bytes(),
        ),
    ];
    for (label, bytes) in cases {
        let before = rig.fake.calls().len();
        let wire = exchange(addr, &bytes).await;
        assert_eq!(wire.status(), Some(200), "{label}");
        assert!(wire.has_header("connection"), "{label}");
        assert!(wire.closed, "{label}: one request per connection");
        let calls = rig.fake.calls();
        assert_eq!(calls.len(), before + 1, "{label}: exactly one forward");
        let call = calls.last().unwrap();
        assert!(call.body_complete, "{label}");
        assert!(
            json_eq_good(&call.body),
            "{label}: the body is the first message"
        );
        assert_wire_is_gateway_built(&rig, call);
        // Trailers and the smuggled message never reach the provider.
        assert!(
            call.header_names()
                .iter()
                .all(|name| !name.to_ascii_lowercase().starts_with("x-")),
            "{label}"
        );
    }
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn expect_continue_is_answered_locally_and_never_forwarded() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    let head = format!(
        "POST {PATH} HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\n\
         Authorization: Bearer {KEY}\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\
         Connection: close\r\n\r\n",
        GOOD.len()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    // The interim answer arrives before the body is sent.
    let mut got = Vec::new();
    let mut buf = [0_u8; 256];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&got).contains("100 Continue") {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
            .await
            .expect("interim response in time")
            .unwrap();
        assert!(n > 0, "closed before the interim response");
        got.extend_from_slice(&buf[..n]);
    }
    rig.fake.assert_nothing_sent();
    stream.write_all(GOOD.as_bytes()).await.unwrap();
    let rest = read_all(&mut stream, Duration::from_secs(15)).await;
    assert!(String::from_utf8_lossy(&rest.bytes).contains("200 OK"));
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].header("expect").is_none(),
        "Expect is never forwarded"
    );
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn one_request_per_connection_even_when_the_client_asks_for_keep_alive() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    // Two complete requests in one write, the first explicitly keep-alive.
    let second_body = chat_body(HOSTILE);
    let first = String::from_utf8(request_with(
        "POST",
        PATH,
        "gw.test",
        "",
        &format!("Content-Length: {}\r\n", GOOD.len()),
        GOOD,
    ))
    .unwrap()
    .replace("Connection: close", "Connection: keep-alive");
    let second = valid_with("", &second_body);
    let mut both = first.into_bytes();
    both.extend_from_slice(&second);
    let wire = exchange(addr, &both).await;
    assert_eq!(wire.status(), Some(200));
    assert!(wire.has_header("connection"));
    assert!(wire.closed);
    assert_eq!(
        wire.text().matches("HTTP/1.1 ").count(),
        1,
        "only one response on the connection"
    );
    assert_eq!(rig.fake.calls().len(), 1, "the second message never ran");
    let seen = rig.fake.calls();
    Markers::empty()
        .with("second", HOSTILE)
        .assert_clean("upstream body", &seen[0].body);
    rig.settle().await;
    server.abort();
}

// ------------------------------------------------------------------ slow head / slowloris

#[tokio::test]
async fn silent_partial_and_trickling_heads_are_cut_at_the_head_deadline() {
    let limits = limits_with(|l| l.body_deadline_ms = 1000);
    let rig = Rig::with(Behavior::ok_json(), limits, Caps::ROOMY).await;
    let (addr, server) = serve(&rig).await;

    // Connects and sends nothing.
    let mut silent = TcpStream::connect(addr).await.unwrap();
    // Sends part of a head, then nothing.
    let mut partial = TcpStream::connect(addr).await.unwrap();
    partial
        .write_all(format!("POST {PATH} HTTP/1.1\r\nHost: gw.test\r\n").as_bytes())
        .await
        .unwrap();
    // Sends a byte at a time, never finishing the head.
    let mut trickle = TcpStream::connect(addr).await.unwrap();
    let feeder = tokio::spawn(async move {
        for byte in format!("POST {PATH} HTTP/1.1\r\nX-Slow: ").bytes().cycle() {
            if trickle.write_all(&[byte]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    });

    // Each is closed by the server, gated on the close event (bounded generously).
    for (label, stream) in [("silent", &mut silent), ("partial", &mut partial)] {
        let started = Instant::now();
        let wire = read_all(stream, Duration::from_secs(20)).await;
        assert!(wire.closed, "{label}: connection must be cut");
        assert!(wire.bytes.is_empty(), "{label}: no response is produced");
        assert!(started.elapsed() < Duration::from_secs(20), "{label}");
    }
    feeder.await.unwrap();

    // No receipt or memory was ever reserved for them, and nothing reached the provider.
    rig.settle().await;
    rig.fake.assert_nothing_sent();
    // The server is still serving.
    let wire = exchange(addr, &valid_with("", GOOD)).await;
    assert_eq!(wire.status(), Some(200));
    server.abort();
}

// ------------------------------------------------------- credentials across requests

#[tokio::test]
async fn credentials_do_not_survive_into_the_next_request_on_the_shared_client() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let (addr, server) = serve(&rig).await;
    let key = |c: char| format!("sk-S{c}Q-SYNTHETIC-REVOKED-NOT-A-KEY");
    let with_key = |k: &str, extra: &str| {
        keyed_request(
            k,
            "POST",
            PATH,
            "gw.test",
            extra,
            &format!("Content-Length: {}\r\n", GOOD.len()),
            GOOD,
        )
    };
    // Request A with a key and an organization.
    let a = with_key(&key('A'), "OpenAI-Organization: org-SYNTH-A\r\n");
    assert_eq!(exchange(addr, &a).await.status(), Some(200));
    // Request B has no credential: rejected, nothing sent, and A's key is not reused.
    let no_key = String::from_utf8(valid_with("", GOOD))
        .unwrap()
        .replace(&format!("Authorization: Bearer {KEY}\r\n"), "");
    let wire = exchange(addr, no_key.as_bytes()).await;
    assert_eq!(wire.status(), Some(401));
    assert_eq!(rig.fake.calls().len(), 1, "no upstream request for the 401");
    // Request C uses another key and no organization.
    let c = with_key(&key('C'), "");
    assert_eq!(exchange(addr, &c).await.status(), Some(200));
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].header("authorization"),
        Some(&*format!("Bearer {}", key('A')))
    );
    assert_eq!(calls[0].header("openai-organization"), Some("org-SYNTH-A"));
    assert_eq!(
        calls[1].header("authorization"),
        Some(&*format!("Bearer {}", key('C')))
    );
    assert!(calls[1].header("openai-organization").is_none());
    Markers::empty()
        .with("key-a", &key('A'))
        .with("org-a", "org-SYNTH-A")
        .assert_clean("second upstream request", &{
            let mut all = calls[1].body.clone();
            for (n, v) in &calls[1].headers {
                all.extend_from_slice(n.as_bytes());
                all.extend_from_slice(v.as_bytes());
            }
            all
        });
    rig.settle().await;
    server.abort();
}

// ------------------------------------------- provider responses are explicitly unredacted

#[tokio::test]
async fn provider_responses_are_relayed_unredacted_on_every_path() {
    let json_body = format!(r#"{{"choices":[{{"message":{{"content":"echo {TOKEN}"}}}}]}}"#);
    let error_body = format!(r#"{{"error":{{"message":"bad key {TOKEN}"}}}}"#);
    let sse_body = format!("data: {{\"delta\":\"{TOKEN}\"}}\n\ndata: [DONE]\n\n");
    let rig = Rig::new(Behavior::Json {
        status: 200,
        body: json_body.clone().into_bytes(),
    })
    .await;
    let (addr, server) = serve(&rig).await;

    let wire = exchange(addr, &valid_with("", GOOD)).await;
    assert_eq!(wire.status(), Some(200));
    assert!(wire.text().contains(TOKEN), "JSON answers are not redacted");

    rig.fake.set_default(Behavior::Json {
        status: 401,
        body: error_body.into_bytes(),
    });
    let wire = exchange(addr, &valid_with("", GOOD)).await;
    assert_eq!(wire.status(), Some(401));
    assert!(
        wire.text().contains(TOKEN),
        "provider errors are not redacted"
    );

    rig.fake.set_default(Behavior::Sse {
        fragments: vec![SseFragment::now(sse_body.into_bytes())],
        framing: SseFraming::Chunked,
        finish: true,
    });
    let streaming =
        r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}],"stream":true}"#;
    let wire = exchange(addr, &valid_with("", streaming)).await;
    assert_eq!(wire.status(), Some(200));
    assert!(wire.text().contains(TOKEN), "SSE events are not redacted");

    // The gateway's own diagnostics never carry provider content.
    let markers = Markers::empty().with("provider-token", TOKEN);
    markers.assert_clean_debug("route", &rig.route);
    markers.assert_clean_debug("metrics", &rig.metrics);
    markers.assert_clean_debug("admission", rig.route.admission());
    rig.settle().await;
    server.abort();
}

// ------------------------------------------------------------------- cancellation flood

fn rss_kb() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

#[tokio::test]
async fn a_flood_of_abandoned_requests_returns_every_permit_and_task() {
    const COMPLETE: usize = 40;
    const HALF: usize = 10;
    let limits = limits_with(|l| {
        l.admission_wait_ms = 250;
        l.body_deadline_ms = 2_000;
    });
    let caps = Caps {
        receipt: 4,
        inspection: 2,
        upstream: 2,
        stream: 1,
    };
    let rig = Rig::with(
        Behavior::Slow {
            delay: Duration::from_millis(150),
            then: Box::new(Behavior::ok_json()),
        },
        limits,
        caps,
    )
    .await;
    let (addr, server) = serve(&rig).await;
    let baseline = alive_tasks();
    let rss_before = rss_kb();

    let mut set = tokio::task::JoinSet::new();
    for i in 0..COMPLETE {
        set.spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let _ = stream.write_all(&valid_with("", GOOD)).await;
            for _ in 0..(i % 4) {
                tokio::task::yield_now().await;
            }
            // Abandon the request: the response is never read.
            drop(stream);
        });
    }
    for _ in 0..HALF {
        set.spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let head = format!(
                "POST {PATH} HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\n\
                 Authorization: Bearer {KEY}\r\nContent-Length: 500\r\n\r\n{{\"model\":"
            );
            let _ = stream.write_all(head.as_bytes()).await;
            drop(stream);
        });
    }
    while let Some(result) = set.join_next().await {
        result.unwrap();
    }

    // Everything the flood held is returned: gated on the capacity events, not a sleep.
    rig.settle().await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive_tasks() > baseline {
        assert!(Instant::now() < deadline, "leaked tasks");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let attempts = rig.metrics.upstream_attempts();
    let calls = rig.fake.tally().calls;
    assert!(
        usize::try_from(attempts).unwrap() <= COMPLETE,
        "attempts {attempts}"
    );
    assert!(
        calls as u64 <= attempts,
        "calls {calls} attempts {attempts}"
    );
    // The service still works.
    let wire = exchange(addr, &valid_with("", GOOD)).await;
    assert_eq!(wire.status(), Some(200));
    rig.settle().await;
    // Counts and memory only: no payloads, no scoring internals (informative, not a limit).
    eprintln!(
        "flood-report: abandoned={} half_bodies={HALF} capacity=receipt{}/inspection{}/upstream{}/stream{} \
         upstream_attempts={attempts} provider_calls={calls} rss_kb_before={rss_before:?} rss_kb_after={:?}",
        COMPLETE,
        caps.receipt,
        caps.inspection,
        caps.upstream,
        caps.stream,
        rss_kb()
    );
    server.abort();
}

// ------------------------------------------------------------- deterministic mutation runs

/// Committed seed: failures reproduce exactly.
const SEED: u64 = 0x5EED_0025_A1FA_0001;

#[tokio::test]
async fn mutated_heads_never_break_the_forwarding_invariants() {
    let limits = limits_with(|l| l.body_deadline_ms = 500);
    let rig = Rig::with(Behavior::ok_json(), limits, Caps::ROOMY).await;
    let (addr, server) = serve(&rig).await;
    let mut rng = Rng(SEED);
    let n = GOOD.len();

    let targets = [
        PATH.to_owned(),
        format!("http://{EVIL_HOST}{PATH}"),
        "//v1/chat/completions".to_owned(),
        "/v1/chat/completions?x=1".to_owned(),
        "/v1/chat/completions/".to_owned(),
        "*".to_owned(),
    ];
    let methods = ["POST", "GET", "PUT", "CONNECT", "post", "TRACE"];
    let versions = ["HTTP/1.1", "HTTP/1.0", "HTTP/1.2", "HTTP/2.0", "http/1.1"];
    let cl_values = [
        n.to_string(),
        (n + 1).to_string(),
        (n - 1).to_string(),
        "0".to_owned(),
        "-1".to_owned(),
        format!("{n}, {n}"),
        "99999999999999999999".to_owned(),
        "abc".to_owned(),
        String::new(),
    ];
    let hostile_lines = [
        "Transfer-Encoding: chunked",
        "Transfer-Encoding: identity",
        "Transfer-Encoding: gzip, chunked",
        "Expect: 100-continue",
        "Expect: nonsense",
        "Upgrade: websocket",
        "Connection: Upgrade",
        "Connection: close, Authorization",
        "Content-Encoding: gzip",
        "Content-Type: text/plain",
        "X-Forwarded-Host: evil.test",
        "Forwarded: host=evil.test",
        "Proxy-Authorization: Basic SYNTH-HOSTILE-ROUTE-6F3A",
        "OpenAI-Organization: bad value",
        "Authorization: Basic SYNTH-HOSTILE-ROUTE-6F3A",
        "Host: evil.test",
        "Content-Length: 5",
        " folded",
        "NoColon",
    ];
    let junk: [&[u8]; 6] = [b"\0", b"\x0b", b"\r", b"\n", b"\x7f", b"\xff"];

    let mut forwarded = 0_usize;
    let cases = 80;
    for case in 0..cases {
        let mut lines: Vec<String> = vec![
            "Host: gw.test".into(),
            "Content-Type: application/json".into(),
            format!("Authorization: Bearer {KEY}"),
            format!("Content-Length: {n}"),
            "Connection: close".into(),
        ];
        let mut method = "POST".to_owned();
        let mut target = PATH.to_owned();
        let mut version = "HTTP/1.1".to_owned();
        let mut body = GOOD.to_owned();
        let mut raw_junk: Option<(usize, &[u8])> = None;
        let mut bare_lf = false;
        let mut truncate = false;
        for _ in 0..=rng.below(3) {
            match rng.below(11) {
                0 => {
                    let i = rng.below(lines.len());
                    lines.push(lines[i].clone());
                }
                1 if lines.len() > 1 => {
                    let i = rng.below(lines.len());
                    lines.remove(i);
                }
                2 => {
                    let v = rng.pick(&cl_values).clone();
                    if let Some(l) = lines.iter_mut().find(|l| l.starts_with("Content-Length")) {
                        *l = format!("Content-Length: {v}");
                    }
                }
                3 | 4 => lines.push((*rng.pick(&hostile_lines)).to_owned()),
                5 => target = rng.pick(&targets).clone(),
                6 => method = (*rng.pick(&methods)).to_owned(),
                7 => version = (*rng.pick(&versions)).to_owned(),
                8 => raw_junk = Some((rng.below(120), rng.pick(&junk))),
                9 => bare_lf = true,
                _ => truncate = true,
            }
        }
        if lines
            .iter()
            .any(|l| l.eq_ignore_ascii_case("Transfer-Encoding: chunked"))
        {
            body = chunked(GOOD);
        }
        let eol = if bare_lf { "\n" } else { "\r\n" };
        let mut head = format!("{method} {target} {version}{eol}");
        for line in &lines {
            head.push_str(line);
            head.push_str(eol);
        }
        head.push_str(eol);
        let mut bytes = head.into_bytes();
        if let Some((at, junk)) = raw_junk {
            let at = at.min(bytes.len());
            bytes.splice(at..at, junk.iter().copied());
        }
        bytes.extend_from_slice(body.as_bytes());
        if truncate {
            let keep = rng.below(bytes.len());
            bytes.truncate(keep);
        }

        let before = rig.fake.calls().len();
        let wire = exchange(addr, &bytes).await;
        assert!(wire.closed, "case {case}: connection must close");
        Markers::empty()
            .with("key", KEY)
            .with("hostile", HOSTILE)
            .assert_clean(&format!("case {case} response"), &wire.bytes);
        let calls = rig.fake.calls();
        let grew = calls.len() - before;
        assert!(grew <= 1, "case {case}: at most one forward per connection");
        if let Some(status) = wire.status()
            && (200..300).contains(&status)
        {
            assert_eq!(grew, 1, "case {case}: a 2xx means one forward");
        }
        if grew == 1 {
            forwarded += 1;
            let call = calls.last().unwrap();
            assert!(json_eq_good(&call.body), "case {case}");
            assert_wire_is_gateway_built(&rig, call);
        }
    }
    // The generator does produce both outcomes, so the checks above are not vacuous.
    assert!(forwarded > 0 && forwarded < cases, "forwarded {forwarded}");
    rig.settle().await;
    let wire = exchange(addr, &valid_with("", GOOD)).await;
    assert_eq!(wire.status(), Some(200), "the server survived every case");
    server.abort();
}

/// JSON-escape every `step`-th character of `text` as `\uXXXX`.
fn escape_some(text: &str, step: usize, offset: usize) -> String {
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if i % step == offset && c.is_ascii() {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

fn contains_token(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => s.contains(TOKEN),
        serde_json::Value::Array(a) => a.iter().any(contains_token),
        serde_json::Value::Object(o) => o
            .iter()
            .any(|(k, v)| k.contains(TOKEN) || contains_token(v)),
        _ => false,
    }
}

#[tokio::test]
async fn mutated_bodies_never_deliver_the_exact_synthetic_secret_or_ambiguous_structure() {
    // Rejection proof only: the exact synthetic token is a known detector input here, and
    // obfuscated forms (which would be a recall claim) are deliberately not generated.
    let rig = Rig::new(Behavior::ok_json()).await;
    let mut rng = Rng(SEED ^ 0xB0D1);
    let mut forwarded = 0_usize;
    let mut rejected = 0_usize;
    for case in 0..120 {
        let secret = escape_some(TOKEN, rng.below(5) + 2, rng.below(2));
        let mut must_reject = false;
        let text = format!("note {secret} end");
        let body = match rng.below(14) {
            0 => format!(
                r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{text}"}}]}}"#
            ),
            1 => format!(
                r#"{{"model":"gpt-4o-mini","messages":[{{"role":"system","content":"{text}"}},{{"role":"user","content":"hi"}}]}}"#
            ),
            2 => format!(
                r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}]}}"#
            ),
            3 => format!(
                r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi"}}],"stop":"{text}"}}"#
            ),
            4 => format!(
                r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi"}}],"stop":["a","{text}"]}}"#
            ),
            5 => format!(
                r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi"}}],"user":"{text}"}}"#
            ),
            6 => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","model":"gpt-4o","messages":[{{"role":"user","content":"{text}"}}]}}"#
                )
            }
            7 => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","model":"gpt-4o","messages":[{{"role":"user","content":"{text}"}}]}}"#
                )
            }
            8 => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi","content":"{text}"}}]}}"#
                )
            }
            9 => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi"}}],"x-unknown":"{text}"}}"#
                )
            }
            10 => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi","{secret}":"v"}}]}}"#
                )
            }
            11 => {
                must_reject = true;
                format!(r#"{{"model":"{secret}","messages":[{{"role":"user","content":"hi"}}]}}"#)
            }
            12 => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{text}"}}]}} trailing"#
                )
            }
            _ => {
                must_reject = true;
                format!(
                    r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{text}"}}],"stream":"true"}}"#
                )
            }
        };
        let before = rig.fake.calls().len();
        let out = rig
            .post(super::forward_tests::request(&body, KEY, &[]))
            .await;
        let calls = rig.fake.calls();
        Markers::empty()
            .with("token", TOKEN)
            .assert_clean(&format!("case {case} response"), &out.body);
        if must_reject {
            assert_ne!(out.status, 200, "case {case}");
            assert_eq!(calls.len(), before, "case {case}: nothing may be forwarded");
        }
        if calls.len() > before {
            forwarded += 1;
            let sent: serde_json::Value =
                serde_json::from_slice(&calls.last().unwrap().body).unwrap();
            assert!(
                !contains_token(&sent),
                "case {case}: the exact secret was delivered"
            );
            assert!(
                !String::from_utf8_lossy(&calls.last().unwrap().body).contains("ghp_SYNTH"),
                "case {case}"
            );
        } else {
            rejected += 1;
        }
    }
    assert!(forwarded > 0 && rejected > 0, "{forwarded}/{rejected}");
    rig.settle().await;
}
