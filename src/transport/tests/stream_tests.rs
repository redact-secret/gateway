//! End-to-end SSE relay (#21; ADR 0018). Compiled only under `cfg(test)`.
//!
//! The whole chat route (admission, strict parse, real pinned-core inspection, upstream
//! and stream permits, central transport, incremental relay) runs against the loopback
//! fake provider through the test-only destination constructors of #23. There is no
//! production path to such a fake (tests/destination_policy.rs), so these tests live in
//! the crate. Real SDK clients against a Gateway wired to a fake provider are #22 (ADR
//! 0017, "Open item"). Every payload, key, and event text is synthetic; nothing leaves
//! loopback.

use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::Pin;

use axum::body::HttpBody;
use axum::extract::Request;
use axum::response::Response;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use super::fake_upstream::{SseFragment, SseFraming};
use super::forward_tests::{
    Caps, KEY, Rig, TOKEN, alive_tasks, assert_gateway_error, raw_post, request,
};
use super::leak::Markers;
use super::*;
use crate::chat_route::{self, ChatRoute};
use crate::telemetry::{Stage, StreamEnd};
use crate::write_stall::StallListener;

const PROVIDER_MARKER: &str = "SYNTH-PROVIDER-EVENT-MARKER-5K2";

/// Two provider events with Korean and emoji text, then the provider's own terminator.
const EVENTS: &str = "data: {\"id\":\"chatcmpl-SYNTH\",\"choices\":[{\"delta\":{\"content\":\"안녕하세요 😀 héllo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" 世界 🚀\"}}]}\n\ndata: [DONE]\n\n";

const EV1: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"하나 😀\"}}]}\n\n";
const EV2: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"둘 🚀\"}}]}\n\n";

const STREAMY: Caps = Caps {
    receipt: 8,
    inspection: 2,
    upstream: 4,
    stream: 2,
};

fn stream_body(content: &str) -> String {
    format!(
        r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{content}"}}],"stream":true}}"#
    )
}

fn stream_request() -> Request {
    request(&stream_body("hello"), KEY, &[])
}

fn limits_with(f: impl FnOnce(&mut RequestLimits)) -> RequestLimits {
    let mut limits = RequestLimits::provisional();
    f(&mut limits);
    limits
}

/// Split `bytes` into fragments of `n` bytes each, a millisecond apart. Splits fall inside
/// events and inside multibyte characters.
fn split_every(bytes: &[u8], n: usize) -> Vec<SseFragment> {
    bytes
        .chunks(n)
        .map(|c| SseFragment::after(Duration::from_millis(1), c.to_vec()))
        .collect()
}

fn sse(fragments: Vec<SseFragment>, finish: bool) -> Behavior {
    Behavior::Sse {
        fragments,
        framing: SseFraming::Chunked,
        finish,
    }
}

/// What a caller sees of a streamed response, frame by frame.
struct Streamed {
    status: u16,
    headers: reqwest::header::HeaderMap,
    bytes: Vec<u8>,
    end: Result<(), String>,
}

async fn drain(response: Response) -> Streamed {
    let (parts, mut body) = response.into_parts();
    let mut bytes = Vec::new();
    let end = loop {
        match poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            None => break Ok(()),
            Some(Err(e)) => break Err(e.to_string()),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    bytes.extend_from_slice(&data);
                }
            }
        }
    };
    Streamed {
        status: parts.status.as_u16(),
        headers: parts.headers,
        bytes,
        end,
    }
}

async fn serve_stalling(route: Arc<ChatRoute>, stall: Duration) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = chat_route::mount(axum::Router::new(), route);
    let task = tokio::spawn(async move {
        let _ = axum::serve(StallListener::new(listener, stall), app).await;
    });
    (addr, task)
}

/// Read until the end of the response head; returns the head text and any body bytes
/// already read.
async fn read_head(client: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), client.read(&mut tmp))
            .await
            .expect("response head in time")
            .unwrap();
        assert!(n > 0, "closed before the response head");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).into_owned();
            return (head, buf[i + 4..].to_vec());
        }
    }
}

/// Read until the peer closes (or errors) or `within` elapses. Returns the bytes and
/// whether the connection was seen to close.
async fn read_to_close(client: &mut TcpStream, within: Duration) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut tmp = [0_u8; 8192];
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, client.read(&mut tmp)).await {
            Err(_) => return (out, false),
            Ok(Ok(0) | Err(_)) => return (out, true),
            Ok(Ok(n)) => out.extend_from_slice(&tmp[..n]),
        }
    }
}

// ------------------------------------------------------------------ faithful relay

#[tokio::test]
async fn fragmented_sse_with_multibyte_text_is_relayed_byte_exact() {
    let leaks = Markers::empty().with("key", KEY).with("token", TOKEN);
    for n in [1_usize, 2, 3, 7, 64] {
        let rig = Rig::with(
            sse(split_every(EVENTS.as_bytes(), n), true),
            RequestLimits::provisional(),
            STREAMY,
        )
        .await;
        let out = drain(
            rig.route
                .handle(request(
                    &stream_body(&format!("my token is {TOKEN} thanks")),
                    KEY,
                    &[],
                ))
                .await,
        )
        .await;
        assert_eq!(out.status, 200, "fragment size {n}");
        assert!(out.end.is_ok(), "a clean provider end is a clean end ({n})");
        assert_eq!(out.bytes, EVENTS.as_bytes(), "byte-exact at size {n}");
        assert_eq!(
            out.headers.get("content-type").map(|v| v.as_bytes()),
            Some(&b"text/event-stream"[..])
        );
        assert!(
            !out.headers.contains_key("content-length"),
            "an incremental body has no length"
        );

        // The one upstream request was the fully inspected, sanitized body.
        let calls = rig.fake.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].body_complete);
        let sent = String::from_utf8(calls[0].body.clone()).unwrap();
        assert!(sent.contains(r#""stream":true"#));
        assert!(!sent.contains(TOKEN), "planted secret reached upstream");
        assert!(sent.contains("my token is <SECRET_1> thanks"));
        leaks.assert_clean("relayed events", &out.bytes);
        rig.settle().await;
    }
}

#[tokio::test]
async fn several_events_in_one_transport_chunk_and_events_split_across_chunks() {
    // One chunk with two whole events and the first half of a third (cut inside a
    // multibyte character), then the rest, then the provider's terminator.
    let third = "data: {\"choices\":[{\"delta\":{\"content\":\"셋 😀\"}}]}\n\n".as_bytes();
    let cut = third.windows(4).position(|w| w == "😀".as_bytes()).unwrap() + 2;
    let mut first = format!("{EV1}{EV2}").into_bytes();
    first.extend_from_slice(&third[..cut]);
    let mut rest = third[cut..].to_vec();
    rest.extend_from_slice(b"data: [DONE]\n\n");
    let rig = Rig::with(
        sse(
            vec![
                SseFragment::now(first.clone()),
                SseFragment::after(Duration::from_millis(20), rest.clone()),
            ],
            true,
        ),
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let out = drain(rig.route.handle(stream_request()).await).await;
    assert!(out.end.is_ok());
    let mut expected = first.clone();
    expected.extend_from_slice(&rest);
    assert_eq!(out.bytes, expected);
    // Events are neither re-framed nor completed by the gateway.
    assert_eq!(
        String::from_utf8(out.bytes)
            .unwrap()
            .matches("\n\n")
            .count(),
        4
    );
    rig.settle().await;
}

#[tokio::test]
async fn a_clean_provider_end_is_relayed_without_adding_a_completion_event() {
    // The provider's stream has no terminator of its own: the gateway adds none.
    let body = format!("{EV1}{EV2}");
    let rig = Rig::with(
        sse(split_every(body.as_bytes(), 5), true),
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let out = drain(rig.route.handle(stream_request()).await).await;
    assert!(out.end.is_ok());
    assert_eq!(out.bytes, body.as_bytes());
    assert!(!String::from_utf8(out.bytes).unwrap().contains("[DONE]"));
    rig.assert_single_attempt().await;
    assert_eq!(rig.metrics.streams_ended(StreamEnd::Completed), 1);
    rig.settle().await;
}

#[tokio::test]
async fn a_clean_end_on_the_wire_has_the_terminating_chunk_and_a_cut_does_not() {
    // Positive control: a finished stream ends with the zero-length chunk.
    let rig = Rig::with(
        sse(vec![SseFragment::now(EV1.as_bytes().to_vec())], true),
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let (addr, server) = serve_stalling(Arc::clone(&rig.route), Duration::from_secs(5)).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&raw_post(&stream_body("hello"), KEY))
        .await
        .unwrap();
    let (head, mut rest) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
    );
    let (more, _) = read_to_close(&mut client, Duration::from_secs(5)).await;
    rest.extend_from_slice(&more);
    assert!(rest.ends_with(b"0\r\n\r\n"), "normal end is the last chunk");
    drop(client);
    server.abort();
    rig.settle().await;

    // The same stream cut by the idle deadline: partial data, then the connection closes
    // with no terminating chunk and no completion event.
    let limits = limits_with(|l| l.stream_idle_ms = 300);
    let rig = Rig::with(
        sse(
            vec![
                SseFragment::now(EV1.as_bytes().to_vec()),
                SseFragment::after(Duration::from_secs(4), EV2.as_bytes().to_vec()),
            ],
            true,
        ),
        limits,
        STREAMY,
    )
    .await;
    let (addr, server) = serve_stalling(Arc::clone(&rig.route), Duration::from_secs(5)).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&raw_post(&stream_body("hello"), KEY))
        .await
        .unwrap();
    let (head, mut rest) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let (more, closed) = read_to_close(&mut client, Duration::from_secs(3)).await;
    rest.extend_from_slice(&more);
    assert!(closed, "the connection is closed, not left hanging");
    let text = String::from_utf8_lossy(&rest).into_owned();
    assert!(text.contains("하나"), "bytes already sent stay sent");
    assert!(!text.contains("[DONE]"), "no fabricated completion");
    assert!(!text.contains("둘"), "nothing past the cut");
    assert!(
        !rest.ends_with(b"0\r\n\r\n"),
        "a cut stream must not look like a finished one"
    );
    server.abort();
    rig.settle().await;
}

// ------------------------------------------------------------- termination contract

#[tokio::test]
async fn failures_after_the_headers_end_the_stream_abruptly_without_a_completion_event() {
    let malformed = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\ndata:\r\nZZ-not-a-chunk-size\r\n";
    let ev1 = EV1.as_bytes().to_vec();
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Behavior, RequestLimits, Vec<u8>, &str, StreamEnd)> = vec![
        (
            "provider cut after two events, no terminator",
            sse(
                vec![
                    SseFragment::now(ev1.clone()),
                    SseFragment::now(EV2.as_bytes().to_vec()),
                ],
                false,
            ),
            RequestLimits::provisional(),
            format!("{EV1}{EV2}").into_bytes(),
            "upstream_invalid_response",
            StreamEnd::UpstreamError,
        ),
        (
            "malformed chunk framing mid-stream",
            Behavior::Malformed(malformed.as_bytes().to_vec()),
            RequestLimits::provisional(),
            b"data:".to_vec(),
            "upstream_invalid_response",
            StreamEnd::UpstreamError,
        ),
        (
            "stalled provider (idle deadline)",
            sse(
                vec![
                    SseFragment::now(ev1.clone()),
                    SseFragment::after(Duration::from_secs(5), EV2.as_bytes().to_vec()),
                ],
                true,
            ),
            limits_with(|l| l.stream_idle_ms = 300),
            ev1.clone(),
            "upstream_timeout",
            StreamEnd::IdleTimeout,
        ),
        (
            "chatty provider that outlives the lifetime deadline",
            Behavior::SseEndless {
                chunk: ev1.clone(),
                interval: Duration::from_millis(20),
            },
            limits_with(|l| {
                l.stream_lifetime_ms = 500;
                l.stream_idle_ms = 400;
            }),
            Vec::new(),
            "upstream_timeout",
            StreamEnd::LifetimeExceeded,
        ),
        (
            "one provider chunk over the relay buffer bound",
            sse(
                vec![
                    SseFragment::now(ev1.clone()),
                    SseFragment::after(Duration::from_millis(150), vec![b'x'; 4096]),
                    SseFragment::after(Duration::from_secs(3), "late"),
                ],
                true,
            ),
            limits_with(|l| l.stream_buffer_bytes = 1024),
            ev1.clone(),
            "upstream_response_too_large",
            StreamEnd::BufferExceeded,
        ),
    ];
    for (name, behavior, limits, expect_prefix, code, ended) in cases {
        let rig = Rig::with(behavior, limits, STREAMY).await;
        let out = drain(rig.route.handle(stream_request()).await).await;
        // Headers were committed before the failure: the status cannot change.
        assert_eq!(out.status, 200, "{name}");
        assert_eq!(
            out.end.as_ref().unwrap_err().as_str(),
            code,
            "{name}: the body ends with an error, not a normal end"
        );
        let text = String::from_utf8_lossy(&out.bytes).into_owned();
        assert!(!text.contains("[DONE]"), "{name}: fabricated completion");
        assert!(
            out.bytes.starts_with(&expect_prefix) || expect_prefix.is_empty(),
            "{name}: bytes already relayed are the provider's"
        );
        assert!(
            !text.contains("upstream_") && !text.contains("error"),
            "{name}: no gateway text is injected into the stream"
        );
        rig.assert_single_attempt().await;
        assert_eq!(rig.metrics.streams_ended(ended), 1, "{name}");
        assert_eq!(rig.metrics.streams_ended(StreamEnd::Completed), 0, "{name}");
        if matches!(
            ended,
            StreamEnd::IdleTimeout | StreamEnd::LifetimeExceeded | StreamEnd::BufferExceeded
        ) {
            assert!(
                rig.fake
                    .wait_for_peer_closed(1, Duration::from_secs(5))
                    .await,
                "{name}: the provider connection was closed"
            );
        }
        rig.settle().await;
    }
}

#[tokio::test]
async fn failures_before_the_headers_are_ordinary_gateway_errors() {
    let cases: Vec<(&str, Behavior, RequestLimits, u16, &str)> = vec![
        (
            "disconnect before any response byte",
            Behavior::DisconnectBeforeResponse,
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "malformed response head",
            Behavior::Malformed(b"NOT HTTP\r\n\r\n".to_vec()),
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "slow response head",
            Behavior::Slow {
                delay: Duration::from_secs(5),
                then: Box::new(Behavior::ok_json()),
            },
            limits_with(|l| {
                l.upstream_header_ms = 250;
                l.upstream_total_ms = 600;
            }),
            504,
            "upstream_timeout",
        ),
        (
            "event stream with no explicit end (close-delimited)",
            Behavior::Sse {
                fragments: vec![SseFragment::now(EV1.as_bytes().to_vec())],
                framing: SseFraming::CloseDelimited,
                finish: true,
            },
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "content coding the gateway cannot relay",
            Behavior::Malformed(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n".to_vec(),
            ),
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "header block over the cap",
            Behavior::Malformed(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Big: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    "a".repeat(1000)
                )
                .into_bytes(),
            ),
            limits_with(|l| l.max_response_header_bytes = 256),
            502,
            "upstream_response_too_large",
        ),
    ];
    let leaks = Markers::empty().with("key", KEY).with("token", TOKEN);
    for (name, behavior, limits, status, code) in cases {
        let rig = Rig::with(behavior, limits, STREAMY).await;
        let out = rig
            .post(request(&stream_body(&format!("x {TOKEN}")), KEY, &[]))
            .await;
        assert_gateway_error(&out, status, code);
        leaks.assert_clean(name, &out.body);
        rig.assert_single_attempt().await;
        assert_eq!(rig.metrics.streams_started(), 0, "{name}");
        rig.settle().await;
    }
}

#[tokio::test]
async fn provider_error_and_non_stream_answers_to_a_stream_request_are_relayed_as_responses() {
    for (status, body) in [
        (
            429_u16,
            r#"{"error":{"message":"synthetic rate limit","code":"rate_limit_exceeded"}}"#,
        ),
        (401, r#"{"error":{"message":"synthetic bad key"}}"#),
        (500, r#"{"error":{"message":"synthetic provider failure"}}"#),
        (200, r#"{"synthetic":"json even though stream was true"}"#),
    ] {
        let rig = Rig::with(
            Behavior::Json {
                status,
                body: body.as_bytes().to_vec(),
            },
            RequestLimits::provisional(),
            STREAMY,
        )
        .await;
        let out = rig.post(stream_request()).await;
        assert_eq!(out.status, status);
        assert_eq!(out.body, body.as_bytes(), "provider body relayed unchanged");
        rig.assert_single_attempt().await;
        assert_eq!(rig.metrics.streams_started(), 0);
        rig.settle().await;
    }
}

// ----------------------------------------------------------------- zero-byte rejections

#[tokio::test]
async fn stream_true_never_bypasses_admission_or_inspection_and_rejections_send_nothing() {
    let limits = limits_with(|l| l.admission_wait_ms = 0);
    let caps = Caps {
        receipt: 1,
        inspection: 1,
        upstream: 1,
        stream: 1,
    };
    let rig = Rig::with(Behavior::ok_json(), limits, caps).await;
    let with_stream = |fields: &str| {
        format!(
            r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"hi"}}],"stream":true{fields}}}"#
        )
    };
    for (req, status, code) in [
        (
            request(&stream_body("hi"), "", &[]),
            401,
            "missing_credential",
        ),
        (
            request(r#"{"stream":true"#, KEY, &[]),
            400,
            "malformed_input",
        ),
        (
            request(&with_stream(r#","tools":[]"#), KEY, &[]),
            422,
            "unsupported_input",
        ),
        (
            request(
                r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}],"stream":"true"}"#,
                KEY,
                &[],
            ),
            422,
            "unsupported_input",
        ),
        (
            // Inspection rejects (a `Warn` finding under the default policy) even though
            // the request asks to stream: nothing is forwarded.
            request(&stream_body("password=hunter2xyz"), KEY, &[]),
            422,
            "unsupported_input",
        ),
        (
            // HTTP/1.0 callers cannot be given a stream that signals truncation.
            {
                let body = stream_body("hi");
                axum::extract::Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .version(axum::http::Version::HTTP_10)
                    .header("content-type", "application/json")
                    .header("content-length", body.len().to_string())
                    .header("authorization", format!("Bearer {KEY}"))
                    .body(axum::body::Body::from(body))
                    .unwrap()
            },
            422,
            "unsupported_input",
        ),
    ] {
        let out = rig.post(req).await;
        assert_gateway_error(&out, status, code);
        rig.fake.assert_nothing_sent();
    }

    // Receipt, inspection, upstream, and stream capacity each held elsewhere: overload,
    // zero upstream bytes.
    let held = rig.admission.try_receipt().unwrap();
    assert_gateway_error(&rig.post(stream_request()).await, 503, "overload");
    rig.fake.assert_nothing_sent();
    drop(held);
    let held = rig.admission.try_inspection().unwrap();
    assert_gateway_error(&rig.post(stream_request()).await, 503, "overload");
    rig.fake.assert_nothing_sent();
    drop(held);
    let held = rig.admission.try_upstream().unwrap();
    assert_gateway_error(&rig.post(stream_request()).await, 503, "overload");
    rig.fake.assert_nothing_sent();
    drop(held);
    let held = rig.admission.try_stream().unwrap();
    let out = rig.post(stream_request()).await;
    assert_gateway_error(&out, 503, "overload");
    assert_eq!(out.headers.get("retry-after").unwrap(), "1");
    rig.fake.assert_nothing_sent();
    drop(held);

    rig.settle().await;
    // Control: with everything free the same request streams (and, since this provider
    // answers JSON, is relayed as that JSON).
    assert_eq!(rig.post(stream_request()).await.status, 200);
    assert_eq!(rig.fake.tally().calls, 1);
    rig.settle().await;
}

#[tokio::test]
async fn no_configured_upstream_is_a_local_501_for_streams_too() {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let none = Upstream::new(None).unwrap();
    let rig = Rig::over(fake, none, RequestLimits::provisional(), STREAMY);
    assert_gateway_error(&rig.post(stream_request()).await, 501, "not_implemented");
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

#[tokio::test]
async fn a_cancelled_waiter_never_starts_an_upstream_request_or_a_stream() {
    let rig = Rig::with(
        sse(vec![SseFragment::now(EV1.as_bytes().to_vec())], true),
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    // Poll once so the request is suspended on the inspection worker, then drop it.
    let mut fut = Box::pin(rig.route.handle(stream_request()));
    let finished = poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(fut.as_mut(), cx).is_ready())
    })
    .await;
    assert!(!finished, "the request must still have been in flight");
    drop(fut);
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.fake.assert_nothing_sent();
    assert_eq!(rig.metrics.streams_started(), 0);
    // The worker job owned its permits for the actual work and returned them when it
    // really ended, even though the stream never started.
    rig.settle().await;
}

// ------------------------------------------------------------- capacity and bounds

#[tokio::test]
async fn open_streams_do_not_hold_inspection_memory_or_receipt_capacity() {
    let caps = Caps {
        receipt: 4,
        inspection: 1,
        upstream: 4,
        stream: 3,
    };
    let rig = Rig::with(
        Behavior::SseEndless {
            chunk: EV1.as_bytes().to_vec(),
            interval: Duration::from_millis(50),
        },
        RequestLimits::provisional(),
        caps,
    )
    .await;
    let first = rig.route.handle(stream_request()).await;
    assert_eq!(first.status().as_u16(), 200);
    // The one inspection slot, every receipt slot, and the whole memory budget are free
    // while the stream stays open: only the upstream and stream slots are occupied.
    drop(
        rig.admission
            .try_inspection()
            .expect("inspection slot is free"),
    );
    let receipts: Vec<_> = (0..caps.receipt)
        .map(|_| rig.admission.try_receipt())
        .collect();
    assert!(receipts.iter().all(Result::is_ok), "receipt slots are free");
    drop(receipts);
    drop(
        rig.admission
            .try_reserve_memory(8192)
            .expect("the whole memory budget is free"),
    );
    // A second stream completes its own inspection through the single inspection slot
    // while the first is still open.
    let second = rig.route.handle(stream_request()).await;
    assert_eq!(second.status().as_u16(), 200);
    assert_eq!(rig.fake.tally().calls, 2);
    drop(first);
    drop(second);
    rig.settle().await;
}

#[tokio::test]
async fn stream_capacity_bounds_streams_upstream_occupancy_and_tasks() {
    let rig = Rig::with(
        Behavior::SseEndless {
            chunk: EV1.as_bytes().to_vec(),
            interval: Duration::from_millis(50),
        },
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let baseline = alive_tasks();
    let first = rig.route.handle(stream_request()).await;
    let second = rig.route.handle(stream_request()).await;
    assert_eq!(first.status().as_u16(), 200);
    assert_eq!(second.status().as_u16(), 200);

    // The third is refused with nothing sent upstream.
    let third = rig.post(stream_request()).await;
    assert_gateway_error(&third, 503, "overload");
    assert_eq!(
        rig.fake.tally().calls,
        2,
        "no upstream request for the third"
    );
    // Upstream occupancy equals the open streams.
    let free: Vec<_> = (0..STREAMY.upstream)
        .map(|_| rig.admission.try_upstream())
        .collect();
    assert_eq!(free.iter().filter(|p| p.is_ok()).count(), 2);
    drop(free);

    // The gateway spawns nothing per stream: what is alive beyond the baseline is the
    // fake provider's connection task and the HTTP client's connection task, per stream.
    let growth = alive_tasks().saturating_sub(baseline);
    assert!(growth <= 2 * 2, "tasks grew by {growth} for two streams");

    drop(first);
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(5))
            .await
    );
    // Capacity returned by one stream admits another.
    let third = rig.route.handle(stream_request()).await;
    assert_eq!(third.status().as_u16(), 200);
    drop((second, third));
    rig.settle().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while alive_tasks() > baseline {
        assert!(tokio::time::Instant::now() < deadline, "leaked tasks");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn slow_consumer_is_backpressured_with_bounded_memory_then_cut_by_the_stall_deadline() {
    const BUFFER: u32 = 64 * 1024;
    let limits = limits_with(|l| {
        l.stream_write_stall_ms = 1500;
        l.stream_buffer_bytes = BUFFER;
    });
    let rig = Rig::with(
        Behavior::SseEndless {
            chunk: vec![b'a'; 16 * 1024],
            interval: Duration::ZERO,
        },
        limits,
        STREAMY,
    )
    .await;
    let (addr, server) = serve_stalling(Arc::clone(&rig.route), limits.stream_write_stall()).await;
    let baseline = alive_tasks();

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&raw_post(&stream_body("hello"), KEY))
        .await
        .unwrap();
    let (head, _) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // The consumer now stops reading while the provider writes as fast as it can.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let first = rig.fake.streamed_bytes();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let second = rig.fake.streamed_bytes();
    assert!(first > 0, "the provider did stream");
    assert_eq!(
        first, second,
        "backpressure: the provider's writes stopped, nothing is accumulated"
    );
    assert!(
        second < 128 * 1024 * 1024,
        "bytes in flight are bounded by socket buffers, got {second}"
    );
    // The relay holds at most one provider chunk, and never more than the bound.
    assert!(rig.metrics.stream_buffered_peak() > 0);
    assert!(
        rig.metrics.stream_buffered_peak() <= u64::from(BUFFER),
        "peak relay buffer {} over the bound",
        rig.metrics.stream_buffered_peak()
    );

    // The stall deadline then closes the connection and the provider connection with it.
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(8))
            .await,
        "the stalled stream was cancelled upstream"
    );
    rig.settle().await;
    assert_eq!(rig.metrics.streams_ended(StreamEnd::Abandoned), 1);
    assert_eq!(
        rig.metrics.stream_buffered(),
        0,
        "buffer gauge back to zero"
    );
    // The time was spent waiting on the consumer, not on the provider.
    let down = rig.metrics.stage(Stage::StreamDownstreamWait).total_micros;
    let up = rig.metrics.stage(Stage::StreamUpstreamWait).total_micros;
    assert!(down >= 1_000_000, "downstream wait {down}us");
    assert!(up < down / 2, "upstream wait {up}us vs downstream {down}us");
    let (_, closed) = read_to_close(&mut client, Duration::from_secs(8)).await;
    assert!(closed, "the consumer connection was closed");
    drop(client);
    assert_eq!(rig.fake.tally().calls, 1, "no retry");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while alive_tasks() > baseline {
        assert!(tokio::time::Instant::now() < deadline, "leaked tasks");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();
}

#[tokio::test]
async fn upstream_wait_is_distinguished_from_relay_overhead_and_consumer_wait() {
    // A slow provider and a fast consumer: the time is upstream wait.
    let fragments = (0..5)
        .map(|i| SseFragment::after(Duration::from_millis(100), format!("data: {i}\n\n")))
        .collect();
    let rig = Rig::with(sse(fragments, true), RequestLimits::provisional(), STREAMY).await;
    let out = drain(rig.route.handle(stream_request()).await).await;
    assert!(out.end.is_ok());
    rig.settle().await;
    let up = rig.metrics.stage(Stage::StreamUpstreamWait).total_micros;
    let down = rig.metrics.stage(Stage::StreamDownstreamWait).total_micros;
    let total = rig.metrics.stage(Stage::StreamTotal).total_micros;
    assert!(up >= 400_000, "upstream wait {up}us");
    assert!(down < up / 2, "consumer wait {down}us vs upstream {up}us");
    assert!(total >= up, "total includes the waits");
    assert_eq!(rig.metrics.stage(Stage::StreamFirstByte).count, 1);
    assert_eq!(rig.metrics.stage(Stage::StreamTotal).count, 1);
    assert_eq!(rig.metrics.streams_started(), 1);
    assert_eq!(rig.metrics.stream_bytes(), 5 * "data: 0\n\n".len() as u64);
}

// -------------------------------------------------------- cancellation and shutdown

#[tokio::test]
async fn downstream_disconnect_cancels_the_upstream_stream_and_returns_everything() {
    let rig = Rig::with(
        Behavior::SseEndless {
            chunk: EV1.as_bytes().to_vec(),
            interval: Duration::from_millis(20),
        },
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let (addr, server) = serve_stalling(Arc::clone(&rig.route), Duration::from_secs(30)).await;
    let baseline = alive_tasks();
    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&raw_post(&stream_body("hello"), KEY))
        .await
        .unwrap();
    let (head, _) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // While streaming, the stream owns an upstream slot and a stream slot.
    let probe: Vec<_> = (0..STREAMY.stream)
        .map(|_| rig.admission.try_stream())
        .collect();
    assert!(probe.iter().any(Result::is_err), "stream permit held");
    drop(probe);

    drop(client); // downstream disconnect
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(5))
            .await,
        "the upstream exchange was cancelled promptly"
    );
    rig.settle().await;
    assert_eq!(rig.metrics.streams_ended(StreamEnd::Abandoned), 1);
    assert_eq!(rig.fake.tally().calls, 1, "no retry after the disconnect");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while alive_tasks() > baseline {
        assert!(tokio::time::Instant::now() < deadline, "leaked tasks");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.abort();
}

#[tokio::test]
async fn shutdown_cancellation_ends_open_streams_and_closes_upstream() {
    let rig = Rig::with(
        Behavior::SseEndless {
            chunk: EV1.as_bytes().to_vec(),
            interval: Duration::from_millis(20),
        },
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let route = Arc::clone(&rig.route);
    let response = rig.route.handle(stream_request()).await;
    assert_eq!(response.status().as_u16(), 200);
    let (out, ()) = tokio::join!(drain(response), async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        route.cancel_in_flight();
    });
    assert_eq!(out.end.as_ref().unwrap_err().as_str(), "not_ready");
    assert!(
        !out.bytes.is_empty(),
        "bytes already sent are not retracted"
    );
    assert!(
        !String::from_utf8_lossy(&out.bytes).contains("[DONE]"),
        "no fabricated completion"
    );
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(5))
            .await
    );
    assert_eq!(rig.metrics.streams_ended(StreamEnd::Shutdown), 1);
    rig.settle().await;
    assert_eq!(rig.fake.tally().calls, 1);
}

#[tokio::test]
async fn graceful_shutdown_with_an_open_stream_is_bounded_by_the_drain_deadline() {
    let limits = limits_with(|l| l.shutdown_drain_ms = 200);
    let rig = Rig::with(
        Behavior::SseEndless {
            chunk: EV1.as_bytes().to_vec(),
            interval: Duration::from_millis(20),
        },
        limits,
        STREAMY,
    )
    .await;
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
        admission: Arc::clone(&rig.admission),
        chat: Arc::clone(&rig.route),
        responses: None,
        drain: Duration::from_millis(200),
    };
    let bound = crate::server::bind(plan, move |_| Ok(services))
        .await
        .unwrap();
    let addr = bound.local_addr().unwrap();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(bound.serve(async move {
        let _ = rx.await;
    }));

    let mut client = TcpStream::connect(addr).await.unwrap();
    client
        .write_all(&raw_post(&stream_body("hello"), KEY))
        .await
        .unwrap();
    let (head, mut seen) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let asked = tokio::time::Instant::now();
    stop.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("shutdown must not wait for an endless stream")
        .unwrap();
    assert!(result.is_ok());
    let took = asked.elapsed();
    assert!(
        took >= Duration::from_millis(150),
        "drain honoured: {took:?}"
    );
    assert!(took < Duration::from_secs(4), "drain bounded: {took:?}");

    // The caller sees the stream stop without a normal end.
    let (more, closed) = read_to_close(&mut client, Duration::from_secs(3)).await;
    seen.extend_from_slice(&more);
    assert!(closed);
    assert!(!seen.ends_with(b"0\r\n\r\n"), "no normal-looking end");
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(5))
            .await
    );
    rig.settle().await;
    assert_eq!(rig.fake.tally().calls, 1, "no replay");
}

// -------------------------------------------------------------- direct transport API

#[tokio::test]
async fn forward_stream_hands_both_permits_to_the_body_and_hides_provider_content() {
    let rig = Rig::with(
        sse(
            vec![SseFragment::now(
                format!("data: {PROVIDER_MARKER}\n\n").into_bytes(),
            )],
            true,
        ),
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let adm = Admission::new(&CapacityPlan::new(
        NonZeroU32::new(1).unwrap(),
        NonZeroU32::new(1).unwrap(),
        NonZeroU32::new(1).unwrap(),
        NonZeroU32::new(1).unwrap(),
        NonZeroU32::new(1).unwrap(),
    ));
    let v = ValidatedRequest::for_test(
        adm.try_reserve_memory(1).unwrap(),
        adm.try_receipt().unwrap(),
    );
    let sealed = boundary::approve(
        v,
        CompleteInspection::for_test(br#"{"model":"m","messages":[],"stream":true}"#.to_vec()),
        chat_route(route()),
    )
    .unwrap();
    let up = http_upstream(rig.fake.addr());
    let forwarded = up
        .forward_stream(
            sealed,
            forward_vetted(KEY),
            adm.try_upstream().unwrap(),
            adm.try_stream().unwrap(),
        )
        .await
        .unwrap();
    // The sealed request's memory reservation ended with the response headers.
    assert!(adm.try_reserve_memory(1).is_ok());
    let Forwarded::Stream(response) = forwarded else {
        panic!("a 2xx event stream is relayed incrementally");
    };
    assert_eq!(response.status().as_u16(), 200);
    let shown = format!("{response:?}");
    assert!(!shown.contains(PROVIDER_MARKER));
    // Both permits belong to the response until it ends.
    assert!(adm.try_upstream().is_err());
    assert!(adm.try_stream().is_err());
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let (_, _, body) = response.into_parts(cancel_rx);
    assert!(!format!("{body:?}").contains(PROVIDER_MARKER));
    assert!(adm.try_upstream().is_err() && adm.try_stream().is_err());
    drop(body);
    assert!(adm.try_upstream().is_ok());
    assert!(adm.try_stream().is_ok());
}

fn forward_vetted(key: &str) -> headers::VettedHeaders {
    super::forward_tests::vetted_for_forward(key)
}

// ---------------------------------------------------------------- diagnostics

#[tokio::test]
async fn no_stream_content_or_keys_appear_in_diagnostics() {
    let markers = Markers::standard()
        .with("key", KEY)
        .with("token", TOKEN)
        .with("provider-event", PROVIDER_MARKER);
    let events = format!("data: {PROVIDER_MARKER}\n\n");
    for behavior in [
        // Completes.
        sse(vec![SseFragment::now(events.clone().into_bytes())], true),
        // Cut.
        sse(vec![SseFragment::now(events.clone().into_bytes())], false),
        // Idle.
        sse(
            vec![
                SseFragment::now(events.clone().into_bytes()),
                SseFragment::after(Duration::from_secs(3), "late"),
            ],
            true,
        ),
    ] {
        let limits = limits_with(|l| l.stream_idle_ms = 250);
        let rig = Rig::with(behavior, limits, STREAMY).await;
        let response = rig
            .route
            .handle(request(
                &stream_body(&format!("see {TOKEN} {}", super::leak::BODY_MARKER)),
                KEY,
                &[("openai-organization", "org-SYNTHETIC-ORG-0001")],
            ))
            .await;
        markers.assert_clean(
            "response headers",
            format!("{:?}", response.headers()).as_bytes(),
        );
        let out = drain(response).await;
        if let Err(text) = &out.end {
            // The error a caller's connection sees is a fixed safe code.
            markers.assert_clean("stream error", text.as_bytes());
            assert!(
                text.starts_with("upstream_") || text == "not_ready",
                "{text}"
            );
        }
        markers.assert_clean_debug("route", &rig.route);
        markers.assert_clean_debug("metrics", &rig.metrics);
        markers.assert_clean_debug("admission", rig.route.admission());
        rig.settle().await;
    }
    for error in [
        StreamError::IdleTimeout,
        StreamError::LifetimeExceeded,
        StreamError::Upstream,
        StreamError::BufferExceeded,
        StreamError::Shutdown,
    ] {
        markers.assert_clean_fmt("stream error", &error);
        markers.assert_clean_debug("stream error", &error);
        assert!(error.code().as_str().is_ascii());
    }
}
