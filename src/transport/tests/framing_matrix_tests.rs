//! Framing, coding and hop-by-hop qualification at the HTTP level (#60). Compiled only
//! under `cfg(test)`.
//!
//! The whole chat route runs against the loopback fake provider (test-only destination
//! constructors of #23; there is no production path to a fake). Every value is synthetic.
//! Three questions are answered here, for each case:
//!
//! 1. Does anything *rejected* reach the provider? (Request framing: no, see
//!    `attack_tests.rs`, which owns the request-head refusals; the cases below add the
//!    response-direction and header-direction matrix.)
//! 2. What does the caller see when the provider's response framing or coding is
//!    inconsistent? A fixed `502 upstream_invalid_response` unless the HTTP client pinned
//!    by `Cargo.lock` itself resolves the ambiguity in a way that is safe to relay; each
//!    such case is pinned individually so a dependency upgrade that changes the outcome
//!    fails here and the table in `docs/contracts/errors-and-telemetry.md` is reconciled.
//! 3. Can a provider byte, a request credential, or payload text appear in an error?
//!    No: every case is leak-scanned.
//!
//! The provider is attempted exactly once in every case (the gateway never retries).

use super::forward_tests::{GOOD, KEY, Rig, TOKEN, assert_gateway_error, chat_body, request};
use super::leak::Markers;
use super::*;

const PROVIDER_MARKER: &str = "SYNTH-PROVIDER-FRAMING-MARKER-91B";

/// What the caller is expected to see.
enum Expect {
    /// A relayed `200` carrying exactly this body.
    Relayed(&'static [u8]),
    /// A Gateway `502` with the fixed `upstream_invalid_response` body.
    InvalidResponse,
}

/// A `200` JSON response head (without the blank line) followed by `rest`.
fn json_response(rest: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{rest}").into_bytes()
}

async fn run(name: &str, raw: Vec<u8>, expect: &Expect) {
    let rig = Rig::new(Behavior::Malformed(raw)).await;
    let out = rig
        .post(request(&chat_body(&format!("x {TOKEN}")), KEY, &[]))
        .await;
    let leaks = Markers::empty()
        .with("provider", PROVIDER_MARKER)
        .with("key", KEY)
        .with("token", TOKEN);
    match expect {
        Expect::Relayed(body) => {
            assert_eq!(out.status, 200, "{name}");
            assert_eq!(out.body, *body, "{name}: only the framed body is relayed");
        }
        Expect::InvalidResponse => {
            assert_gateway_error(&out, 502, "upstream_invalid_response");
            leaks.assert_clean(name, &out.body);
        }
    }
    for (n, v) in &out.headers {
        leaks.assert_clean(name, n.as_str().as_bytes());
        leaks.assert_clean(name, v.as_bytes());
        assert!(
            !matches!(
                n.as_str(),
                "transfer-encoding" | "content-encoding" | "upgrade" | "keep-alive" | "trailer"
            ),
            "{name}: {n} must not be relayed"
        );
    }
    rig.assert_single_attempt().await;
    let calls = rig.fake.calls();
    assert!(
        !String::from_utf8_lossy(&calls[0].body).contains(TOKEN),
        "{name}: sanitized body only"
    );
    rig.settle().await;
}

#[tokio::test]
async fn provider_response_length_and_framing_inconsistencies_never_relay_unframed_bytes() {
    let trailing = format!("Content-Length: 2\r\nConnection: close\r\n\r\n{{}}{PROVIDER_MARKER}");
    let cases: Vec<(&str, Vec<u8>, Expect)> = vec![
        (
            "declared length longer than the bytes sent",
            json_response("Content-Length: 50\r\nConnection: close\r\n\r\n{}"),
            Expect::InvalidResponse,
        ),
        (
            "declared length shorter: trailing provider bytes are not relayed",
            json_response(&trailing),
            Expect::Relayed(b"{}"),
        ),
        (
            "duplicate Content-Length with different values",
            json_response("Content-Length: 2\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{}x"),
            Expect::InvalidResponse,
        ),
        (
            "Content-Length that is not a number",
            json_response("Content-Length: abc\r\nConnection: close\r\n\r\n{}"),
            Expect::InvalidResponse,
        ),
        (
            "Content-Length with a sign",
            json_response("Content-Length: +2\r\nConnection: close\r\n\r\n{}"),
            Expect::InvalidResponse,
        ),
        (
            "Content-Length with an absurd value",
            json_response(
                "Content-Length: 99999999999999999999999\r\nConnection: close\r\n\r\n{}",
            ),
            Expect::InvalidResponse,
        ),
        (
            "chunk size not hexadecimal",
            json_response(
                "Transfer-Encoding: chunked\r\nConnection: close\r\n\r\nZZ\r\n{}\r\n0\r\n\r\n",
            ),
            Expect::InvalidResponse,
        ),
        (
            "chunked body cut before the terminating chunk",
            json_response("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n{}\r\n"),
            Expect::InvalidResponse,
        ),
        (
            "chunk declares more than it carries",
            json_response("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n40\r\n{}"),
            Expect::InvalidResponse,
        ),
        (
            "status line that is not HTTP",
            format!("ICY 200 OK\r\nContent-Length: 2\r\n\r\n{{}} {PROVIDER_MARKER}").into_bytes(),
            Expect::InvalidResponse,
        ),
        (
            "partial status line, then the provider closes",
            b"HTTP/1.1 20".to_vec(),
            Expect::InvalidResponse,
        ),
        (
            "headers never terminated, then the provider closes",
            json_response("X-Partial: yes\r\n"),
            Expect::InvalidResponse,
        ),
        (
            "header line without a colon",
            json_response("NoColonHere\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"),
            Expect::InvalidResponse,
        ),
        (
            "101 Switching Protocols is never a relayable answer",
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
                .to_vec(),
            Expect::InvalidResponse,
        ),
    ];
    for (name, raw, expect) in cases {
        run(name, raw, &expect).await;
    }
}

/// Ambiguity the pinned HTTP client resolves itself is recorded, not assumed: the assertion
/// pins what the pinned `hyper`/`reqwest` do today and what the caller then sees.
#[tokio::test]
async fn provider_response_ambiguities_the_client_resolves_are_pinned() {
    // Transfer-Encoding wins and Content-Length is discarded by the pinned client: the body
    // is the de-chunked content. Neither framing header is relayed to the caller.
    run(
        "response with both Content-Length and Transfer-Encoding",
        json_response(
            "Content-Length: 50\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
        ),
        &Expect::Relayed(b"{}"),
    )
    .await;
    // No length at all on a JSON answer: read to EOF, which for a close-delimited body is a
    // legitimate end (RFC 9112 section 6.3). Streams refuse this form (ADR 0018), JSON does not.
    run(
        "close-delimited JSON answer",
        json_response("Connection: close\r\n\r\n{}"),
        &Expect::Relayed(b"{}"),
    )
    .await;
}

#[tokio::test]
async fn provider_content_codings_other_than_identity_are_never_relayed() {
    for coding in [
        "gzip",
        "GZIP",
        "x-gzip",
        "deflate",
        "br",
        "zstd",
        "compress",
        "gzip, identity",
        "identity, gzip",
        "unknown-coding",
    ] {
        run(
            coding,
            json_response(&format!(
                "Content-Encoding: {coding}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            )),
            &Expect::InvalidResponse,
        )
        .await;
    }
    // A second Content-Encoding line cannot hide a coding behind an identity one.
    run(
        "identity line then gzip line",
        json_response(
            "Content-Encoding: identity\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ),
        &Expect::InvalidResponse,
    )
    .await;
    // Explicit identity is not a coding and is relayed (without the header).
    run(
        "identity",
        json_response(
            "Content-Encoding: identity\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ),
        &Expect::Relayed(b"{}"),
    )
    .await;
}

#[tokio::test]
async fn hop_by_hop_and_connection_nominated_response_headers_are_not_relayed() {
    let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
Connection: close, X-Request-Id, X-Provider-Private\r\n\
X-Request-Id: req_nominated\r\nX-Provider-Private: SYNTH-PRIVATE\r\n\
OpenAI-Processing-Ms: 12\r\nKeep-Alive: timeout=5\r\nProxy-Authenticate: Basic realm=x\r\n\
Proxy-Connection: keep-alive\r\nTrailer: X-Late\r\nTE: trailers\r\nUpgrade: h2c\r\n\
Set-Cookie: s=SYNTH-COOKIE\r\nContent-Length: 2\r\n\r\n{}";
    let rig = Rig::new(Behavior::Malformed(raw.as_bytes().to_vec())).await;
    let out = rig.post_good().await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body, b"{}");
    let mut names: Vec<&str> = out.headers.keys().map(|n| n.as_str()).collect();
    names.sort_unstable();
    // Only allowlisted, un-nominated names survive; `Connection` itself is regenerated by
    // the gateway's own server, never copied from the provider.
    for gone in [
        "x-request-id",
        "x-provider-private",
        "keep-alive",
        "proxy-authenticate",
        "proxy-connection",
        "trailer",
        "te",
        "upgrade",
        "set-cookie",
    ] {
        assert!(!names.contains(&gone), "{gone} was relayed: {names:?}");
    }
    assert!(names.contains(&"openai-processing-ms"), "{names:?}");
    assert!(names.contains(&"content-type"), "{names:?}");
    rig.settle().await;
}

#[tokio::test]
async fn hop_by_hop_and_connection_nominated_request_headers_never_reach_the_provider() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let out = rig
        .post(request(
            GOOD,
            KEY,
            &[
                ("connection", "keep-alive, x-nominated-private"),
                ("x-nominated-private", "SYNTH-NOMINATED"),
                ("keep-alive", "timeout=99"),
                ("proxy-authorization", "Basic U1lOVEg="),
                ("proxy-connection", "keep-alive"),
                ("te", "trailers"),
                ("trailer", "X-Late"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
        ))
        .await;
    assert_eq!(out.status, 200);
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 1);
    let names: Vec<String> = calls[0]
        .headers
        .iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    for gone in [
        "x-nominated-private",
        "keep-alive",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "x-forwarded-for",
    ] {
        assert!(
            !names.iter().any(|n| n == gone),
            "{gone} reached the provider: {names:?}"
        );
    }
    assert!(
        !calls[0]
            .headers
            .iter()
            .any(|(_, v)| v.contains("SYNTH-NOMINATED")),
        "a nominated value reached the provider"
    );
    rig.settle().await;
}
