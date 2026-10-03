//! Responses JSON and SSE relay lifecycle (#87; ADR 0018, ADR 0031). Compiled only under
//! `cfg(test)`.
//!
//! `POST /v1/responses` is the shared `EndpointRoute` over the shared admission, inspection,
//! permits, and central transport, so the Chat relay bounds are the Responses relay bounds.
//! These tests drive a Responses endpoint against the loopback fake provider with synthetic
//! Responses event streams and prove: bytes and events are relayed unchanged, the gateway
//! never fabricates `response.completed`, an `error` event, a status, or a retry, a
//! provider-declared `response.failed` / `response.incomplete` is just provider bytes
//! followed by a normal end (transport EOF is the only thing the gateway reports as an
//! abrupt end), and every lifecycle limit (idle, lifetime, buffer, write stall, abort,
//! shutdown) returns permits only after the real resources are gone. Every payload, key,
//! and event text is synthetic; nothing leaves loopback.

use axum::extract::Request;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::fake_upstream::SseFragment;
use super::forward_tests::{Caps, KEY, Rig, TOKEN, alive_tasks};
use super::leak::Markers;
use super::stream_tests::{
    STREAMY, drain, limits_with, read_head, read_to_close, serve_stalling, split_every, sse,
};
use super::*;
use crate::chat_route::EndpointRoute;
use crate::telemetry::StreamEnd;

const CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_SYNTH\",\"status\":\"in_progress\"}}\n\n";
const TEXT_DELTA_1: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"안녕하세요 😀 héllo\"}\n\n";
const TEXT_DELTA_2: &str = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\" 世界 🚀\"}\n\n";
const ARGS_DELTA_1: &str = "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"city\\\":\"}\n\n";
const ARGS_DELTA_2: &str = "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"\\\"서울\\\"}\"}\n\n";
const ARGS_DONE: &str = "event: response.function_call_arguments.done\ndata: {\"type\":\"response.function_call_arguments.done\",\"arguments\":\"{\\\"city\\\":\\\"서울\\\"}\"}\n\n";
const COMPLETED: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_SYNTH\",\"status\":\"completed\"}}\n\n";
const FAILED: &str = "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_SYNTH\",\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"synthetic provider failure\"}}}\n\n";
const INCOMPLETE: &str = "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_SYNTH\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n";
const STREAM_ERROR: &str = "event: error\ndata: {\"type\":\"error\",\"code\":\"synthetic_error\",\"message\":\"synthetic provider stream error\"}\n\n";

fn complete_stream() -> String {
    [
        CREATED,
        TEXT_DELTA_1,
        TEXT_DELTA_2,
        ARGS_DELTA_1,
        ARGS_DELTA_2,
        ARGS_DONE,
        COMPLETED,
    ]
    .concat()
}

fn body(content: &str) -> String {
    format!(r#"{{"model":"gpt-4o-mini","input":"{content}","store":false,"stream":true}}"#)
}

fn post(body: &str) -> Request {
    at("/v1/responses", body, KEY)
}

fn at(path: &str, body: &str, key: &str) -> Request {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string())
        .header("authorization", format!("Bearer {key}"))
        .body(axum::body::Body::from(body.to_owned()))
        .unwrap()
}

fn raw_post(body: &str) -> Vec<u8> {
    format!(
        "POST /v1/responses HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\nAuthorization: Bearer {KEY}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn fragmented_responses_events_are_relayed_byte_exact_to_the_responses_destination() {
    let events = complete_stream();
    let leaks = Markers::empty().with("key", KEY).with("token", TOKEN);
    for n in [1_usize, 3, 7, 64] {
        let rig = Rig::responses(
            sse(split_every(events.as_bytes(), n), true),
            RequestLimits::provisional(),
            STREAMY,
        )
        .await;
        let out = drain(
            rig.route
                .handle(post(&body(&format!("my token is {TOKEN} thanks"))))
                .await,
        )
        .await;
        assert_eq!(out.status, 200, "fragment size {n}");
        assert!(
            out.end.is_ok(),
            "the provider's own end is a clean end ({n})"
        );
        assert_eq!(out.bytes, events.as_bytes(), "byte-exact at size {n}");
        assert_eq!(
            out.headers.get("content-type").map(|v| v.as_bytes()),
            Some(&b"text/event-stream"[..])
        );
        assert!(!out.headers.contains_key("content-length"));
        let calls = rig.fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/v1/responses");
        assert!(calls[0].body_complete);
        let sent = String::from_utf8(calls[0].body.clone()).unwrap();
        assert!(sent.contains(r#""stream":true"#));
        assert!(!sent.contains(TOKEN), "planted secret reached upstream");
        assert!(sent.contains("my token is <SECRET_1> thanks"));
        leaks.assert_clean("relayed events", &out.bytes);
        assert_eq!(rig.metrics.streams_ended(StreamEnd::Completed), 1);
        rig.settle().await;
    }
}

#[tokio::test]
async fn provider_declared_failure_and_incomplete_are_relayed_bytes_with_a_normal_end() {
    // The provider says the response failed or is incomplete in its own event. To the
    // transport that is ordinary content followed by a clean EOF: the gateway neither adds
    // nor removes an event, and does not turn it into an abrupt end or a status.
    for (name, tail) in [
        ("response.failed", FAILED),
        ("response.incomplete", INCOMPLETE),
        ("error event", STREAM_ERROR),
    ] {
        let events = [CREATED, TEXT_DELTA_1, tail].concat();
        let rig = Rig::responses(
            sse(split_every(events.as_bytes(), 5), true),
            RequestLimits::provisional(),
            STREAMY,
        )
        .await;
        let out = drain(rig.route.handle(post(&body("hello"))).await).await;
        assert_eq!(out.status, 200, "{name}");
        assert!(out.end.is_ok(), "{name}: transport saw a normal end");
        assert_eq!(out.bytes, events.as_bytes(), "{name}: bytes unchanged");
        assert!(
            !text(&out.bytes).contains("response.completed"),
            "{name}: no fabricated completion"
        );
        rig.assert_single_attempt().await;
        assert_eq!(rig.metrics.streams_ended(StreamEnd::Completed), 1, "{name}");
        rig.settle().await;
    }
}

#[tokio::test]
async fn truncation_without_a_terminal_event_is_an_abrupt_end_and_never_a_synthesized_one() {
    let malformed = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\ndata:\r\nZZ-not-a-chunk-size\r\n";
    let head = [CREATED, TEXT_DELTA_1].concat();
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Behavior, RequestLimits, &str, StreamEnd)> = vec![
        (
            "provider cut after text and function-argument deltas",
            sse(
                vec![
                    SseFragment::now(head.clone()),
                    SseFragment::now(ARGS_DELTA_1),
                ],
                false,
            ),
            RequestLimits::provisional(),
            "upstream_invalid_response",
            StreamEnd::UpstreamError,
        ),
        (
            "malformed chunk framing mid-stream",
            Behavior::Malformed(malformed.as_bytes().to_vec()),
            RequestLimits::provisional(),
            "upstream_invalid_response",
            StreamEnd::UpstreamError,
        ),
        (
            "stalled provider (idle deadline)",
            sse(
                vec![
                    SseFragment::now(head.clone()),
                    SseFragment::after(Duration::from_secs(5), COMPLETED),
                ],
                true,
            ),
            limits_with(|l| l.stream_idle_ms = 300),
            "upstream_timeout",
            StreamEnd::IdleTimeout,
        ),
        (
            "chatty provider that outlives the lifetime deadline",
            Behavior::SseEndless {
                chunk: TEXT_DELTA_1.as_bytes().to_vec(),
                interval: Duration::from_millis(20),
            },
            limits_with(|l| {
                l.stream_lifetime_ms = 500;
                l.stream_idle_ms = 400;
            }),
            "upstream_timeout",
            StreamEnd::LifetimeExceeded,
        ),
        (
            "one provider chunk over the relay buffer bound",
            sse(
                vec![
                    SseFragment::now(head.clone()),
                    SseFragment::after(Duration::from_millis(150), vec![b'x'; 4096]),
                    SseFragment::after(Duration::from_secs(3), COMPLETED),
                ],
                true,
            ),
            limits_with(|l| l.stream_buffer_bytes = 1024),
            "upstream_response_too_large",
            StreamEnd::BufferExceeded,
        ),
    ];
    for (name, behavior, limits, code, ended) in cases {
        let rig = Rig::responses(behavior, limits, STREAMY).await;
        let out = drain(rig.route.handle(post(&body("hello"))).await).await;
        assert_eq!(out.status, 200, "{name}: committed status cannot change");
        assert_eq!(
            out.end.as_ref().unwrap_err().as_str(),
            code,
            "{name}: the body ends with an error, not a normal end"
        );
        let seen = text(&out.bytes);
        for fabricated in [
            "response.completed",
            "response.failed",
            "response.incomplete",
        ] {
            assert!(
                !seen.contains(fabricated),
                "{name}: fabricated {fabricated}"
            );
        }
        assert!(
            !seen.contains("upstream_") && !seen.contains("event: error"),
            "{name}: no gateway text is injected into the stream"
        );
        rig.assert_single_attempt().await;
        assert_eq!(rig.metrics.streams_ended(ended), 1, "{name}");
        assert_eq!(rig.metrics.streams_ended(StreamEnd::Completed), 0, "{name}");
        rig.settle().await;
    }
}

#[tokio::test]
async fn ordinary_json_and_provider_errors_are_relayed_unchanged_for_streaming_and_not() {
    let ok = r#"{"id":"resp_SYNTH","status":"completed","output":[]}"#;
    let failed = r#"{"id":"resp_SYNTH","status":"failed","error":{"code":"server_error","message":"synthetic"}}"#;
    for (status, payload) in [
        (200, ok),
        (200, failed),
        (400, r#"{"error":{"message":"synthetic bad request"}}"#),
        (429, r#"{"error":{"message":"synthetic rate limit"}}"#),
        (500, r#"{"error":{"message":"synthetic provider failure"}}"#),
    ] {
        for stream in [false, true] {
            let rig = Rig::responses(
                Behavior::Json {
                    status,
                    body: payload.as_bytes().to_vec(),
                },
                RequestLimits::provisional(),
                STREAMY,
            )
            .await;
            let request = if stream {
                post(&body("hello"))
            } else {
                post(r#"{"model":"gpt-4o-mini","input":"hello","store":false}"#)
            };
            let out = rig.post(request).await;
            assert_eq!(out.status, status);
            assert_eq!(out.body, payload.as_bytes(), "provider body unchanged");
            rig.assert_single_attempt().await;
            assert_eq!(rig.metrics.streams_started(), 0);
            rig.settle().await;
        }
    }
}

#[tokio::test]
async fn rejected_requests_send_nothing_upstream_even_with_stream_true() {
    for (name, payload) in [
        (
            "store missing",
            r#"{"model":"gpt-4o-mini","input":"hi","stream":true}"#.to_owned(),
        ),
        (
            "store true",
            r#"{"model":"gpt-4o-mini","input":"hi","store":true,"stream":true}"#.to_owned(),
        ),
        (
            "unknown field",
            r#"{"model":"gpt-4o-mini","input":"hi","store":false,"stream":true,"zzz":1}"#
                .to_owned(),
        ),
        (
            "truncated JSON",
            r#"{"model":"gpt-4o-mini","input":"hi","store":false,"stream":tr"#.to_owned(),
        ),
    ] {
        let rig = Rig::responses(sse(vec![], true), RequestLimits::provisional(), STREAMY).await;
        let out = rig.post(post(&payload)).await;
        assert!(
            out.status >= 400 && out.status < 500,
            "{name}: {}",
            out.status
        );
        rig.fake.assert_nothing_sent();
        assert_eq!(rig.metrics.streams_started(), 0, "{name}");
        rig.settle().await;
    }
}

#[tokio::test]
async fn abort_before_the_response_headers_cancels_the_provider_exchange_and_never_retries() {
    // The provider accepted the request and is slow to answer; the caller goes away first.
    let rig = Rig::responses(
        Behavior::Slow {
            delay: Duration::from_secs(30),
            then: Box::new(sse(vec![SseFragment::now(CREATED)], true)),
        },
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let route = Arc::clone(&rig.route);
    let request = post(&body("hello"));
    let caller = tokio::spawn(async move { route.handle(request).await.status() });
    assert!(rig.fake.wait_for_calls(1, Duration::from_secs(10)).await);
    // The request is on the wire and no response head exists yet: the caller goes away.
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    // The provider would hold the request for 30 s; every permit is back long before
    // that only because dropping the request future dropped the exchange.
    rig.settle().await;
    assert_eq!(
        rig.fake.tally().calls,
        1,
        "the sent request is not replayed"
    );
    assert_eq!(rig.metrics.streams_started(), 0);
}

#[tokio::test]
async fn abort_after_the_headers_returns_every_permit_once_the_provider_is_closed() {
    let rig = Rig::responses(
        Behavior::SseEndless {
            chunk: TEXT_DELTA_1.as_bytes().to_vec(),
            interval: Duration::from_millis(20),
        },
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let (addr, server) = serve_stalling(Arc::clone(&rig.route), Duration::from_secs(30)).await;
    let baseline = alive_tasks();
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&raw_post(&body("hello"))).await.unwrap();
    let (head, _) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    // While the stream is open it owns an upstream and a stream slot.
    let probe: Vec<_> = (0..STREAMY.stream)
        .map(|_| rig.admission.try_stream())
        .collect();
    assert!(probe.iter().any(Result::is_err), "stream permit held");
    drop(probe);
    drop(client);
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(5))
            .await
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
async fn a_slow_consumer_is_backpressured_then_cut_by_the_stall_deadline() {
    const BUFFER: u32 = 64 * 1024;
    let limits = limits_with(|l| {
        l.stream_write_stall_ms = 1500;
        l.stream_buffer_bytes = BUFFER;
    });
    let rig = Rig::responses(
        Behavior::SseEndless {
            chunk: vec![b'a'; 16 * 1024],
            interval: Duration::ZERO,
        },
        limits,
        STREAMY,
    )
    .await;
    let (addr, server) = serve_stalling(Arc::clone(&rig.route), limits.stream_write_stall()).await;
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&raw_post(&body("hello"))).await.unwrap();
    let (head, _) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let first = rig.fake.streamed_bytes();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(first > 0);
    assert_eq!(
        first,
        rig.fake.streamed_bytes(),
        "backpressure stops the provider"
    );
    assert!(rig.metrics.stream_buffered_peak() <= u64::from(BUFFER));
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(8))
            .await,
        "the stalled stream was cancelled upstream"
    );
    rig.settle().await;
    assert_eq!(rig.metrics.streams_ended(StreamEnd::Abandoned), 1);
    assert_eq!(rig.metrics.stream_buffered(), 0);
    let (_, closed) = read_to_close(&mut client, Duration::from_secs(8)).await;
    assert!(closed, "the consumer connection was closed");
    assert_eq!(rig.fake.tally().calls, 1, "no retry");
    server.abort();
}

#[tokio::test]
async fn shutdown_cancellation_ends_open_responses_streams_without_a_terminal_event() {
    let rig = Rig::responses(
        Behavior::SseEndless {
            chunk: TEXT_DELTA_1.as_bytes().to_vec(),
            interval: Duration::from_millis(20),
        },
        RequestLimits::provisional(),
        STREAMY,
    )
    .await;
    let route = Arc::clone(&rig.route);
    let response = rig.route.handle(post(&body("hello"))).await;
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
    assert!(!text(&out.bytes).contains("response.completed"));
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
async fn graceful_shutdown_with_an_open_responses_stream_is_bounded_by_the_drain_deadline() {
    let limits = limits_with(|l| l.shutdown_drain_ms = 200);
    let rig = Rig::responses(
        Behavior::SseEndless {
            chunk: TEXT_DELTA_1.as_bytes().to_vec(),
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
    let chat = Arc::new(EndpointRoute::new(
        Arc::clone(&rig.admission),
        limits,
        RouteId::new("test.chat"),
    ));
    let services = crate::server::Services {
        admission: Arc::clone(&rig.admission),
        chat,
        responses: Some(Arc::clone(&rig.route)),
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
    client.write_all(&raw_post(&body("hello"))).await.unwrap();
    let (head, mut seen) = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let asked = tokio::time::Instant::now();
    stop.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("shutdown must not wait for an endless stream")
        .unwrap();
    assert!(result.is_ok());
    assert!(
        asked.elapsed() >= Duration::from_millis(150),
        "drain honoured"
    );
    let (more, closed) = read_to_close(&mut client, Duration::from_secs(3)).await;
    seen.extend_from_slice(&more);
    assert!(closed);
    assert!(!seen.ends_with(b"0\r\n\r\n"), "no normal-looking end");
    assert!(!text(&seen).contains("response.completed"));
    assert!(
        rig.fake
            .wait_for_peer_closed(1, Duration::from_secs(5))
            .await
    );
    rig.settle().await;
    assert_eq!(rig.fake.tally().calls, 1, "no replay");
}

#[tokio::test]
async fn an_open_responses_stream_holds_the_stream_permit_and_the_next_one_is_overload() {
    const ONE: Caps = Caps {
        receipt: 8,
        inspection: 2,
        upstream: 4,
        stream: 1,
    };
    let rig = Rig::responses(
        Behavior::SseEndless {
            chunk: TEXT_DELTA_1.as_bytes().to_vec(),
            interval: Duration::from_millis(20),
        },
        RequestLimits::provisional(),
        ONE,
    )
    .await;
    let first = rig.route.handle(post(&body("one"))).await;
    assert_eq!(first.status().as_u16(), 200);
    // The one stream permit is held by the open body: the next stream is an immediate,
    // fixed `overload`, not a queue, and sends nothing upstream.
    let second = rig.post(post(&body("two"))).await;
    assert_eq!(second.status, 503);
    assert_eq!(rig.fake.tally().calls, 1);
    drop(first);
    rig.settle().await;
}
