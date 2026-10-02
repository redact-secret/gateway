//! Self-tests of the fake-upstream harness (issue #6). They prove the harness records
//! what it should and simulates each failure mode, so later negative tests can trust a
//! "zero upstream body" result. They do not test gateway behavior.

mod support;

use std::time::Duration;

use support::fake_upstream::{Behavior, FakeUpstream, SseFragment, SseFraming};
use support::leak::{BODY_MARKER, HEADER_MARKER, Markers};
use support::raw_http::{exchange, parse_response, post, post_chunked};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn records_method_path_headers_and_body_bytes() {
    let up = FakeUpstream::start(Behavior::ok_json()).await;
    let body = format!(r#"{{"content":"{BODY_MARKER}"}}"#);
    let req = post(
        "/v1/chat/completions",
        &[
            ("X-Synthetic", HEADER_MARKER),
            ("Content-Type", "application/json"),
        ],
        body.as_bytes(),
    );
    let (bytes, _) = exchange(up.addr(), &req, T).await.unwrap();
    let resp = parse_response(&bytes).expect("http response");
    assert_eq!(resp.status, 200);
    assert!(!resp.truncated);

    let calls = up.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.path, "/v1/chat/completions");
    assert_eq!(call.header("x-synthetic"), Some(HEADER_MARKER));
    assert_eq!(call.body, body.as_bytes());
    assert!(call.body_complete);
    let t = up.tally();
    assert_eq!((t.connections, t.calls, t.body_bytes), (1, 1, body.len()));
}

#[tokio::test]
async fn records_chunked_request_bodies_exactly() {
    let up = FakeUpstream::start(Behavior::ok_json()).await;
    let req = post_chunked("/x", &[b"{\"a\":", b"1}"]);
    let _ = exchange(up.addr(), &req, T).await.unwrap();
    assert_eq!(up.calls()[0].body, b"{\"a\":1}");
}

#[tokio::test]
async fn aborted_request_is_recorded_as_partial_body() {
    let up = FakeUpstream::start(Behavior::ok_json()).await;
    let mut s = TcpStream::connect(up.addr()).await.unwrap();
    s.write_all(b"POST /x HTTP/1.1\r\nContent-Length: 100\r\n\r\npartial-synthetic")
        .await
        .unwrap();
    drop(s);
    assert!(up.wait_for_calls(1, T).await);
    // Give the server a moment to observe EOF.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let call = &up.calls()[0];
    assert!(!call.body_complete);
    assert_eq!(call.body, b"partial-synthetic");
    assert!(
        up.check_nothing_sent().is_err(),
        "partial bytes count as sent"
    );
}

#[tokio::test]
async fn nothing_sent_check_passes_when_idle_and_fails_with_value_free_message() {
    let up = FakeUpstream::start(Behavior::ok_json()).await;
    up.assert_nothing_sent();

    let body = format!(r#"{{"k":"{BODY_MARKER}"}}"#);
    let _ = exchange(
        up.addr(),
        &post("/x", &[("X-Synthetic", HEADER_MARKER)], body.as_bytes()),
        T,
    )
    .await
    .unwrap();
    let violation = up.check_nothing_sent().unwrap_err();
    assert_eq!(violation.tally.body_bytes, body.len());

    // The failure text, Debug of recorded calls, and the fake itself carry no markers.
    let markers = Markers::standard();
    markers.assert_clean_fmt("ForwardViolation", &violation);
    markers.assert_clean_debug("calls", &up.calls());
    markers.assert_clean_debug("fake", &up);
}

#[tokio::test]
async fn slow_response_is_delayed() {
    let up = FakeUpstream::start(Behavior::Slow {
        delay: Duration::from_millis(300),
        then: Box::new(Behavior::ok_json()),
    })
    .await;
    let start = std::time::Instant::now();
    let (bytes, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert_eq!(parse_response(&bytes).unwrap().status, 200);
}

#[tokio::test]
async fn disconnect_before_response_sends_no_bytes() {
    let up = FakeUpstream::start(Behavior::DisconnectBeforeResponse).await;
    let (bytes, timed_out) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    assert!(bytes.is_empty());
    assert!(!timed_out);
    assert_eq!(up.tally().calls, 1);
}

#[tokio::test]
async fn disconnect_mid_body_is_truncated() {
    let up = FakeUpstream::start(Behavior::DisconnectMidBody {
        declared_len: 100,
        sent: b"{\"partial\":".to_vec(),
    })
    .await;
    let (bytes, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    let resp = parse_response(&bytes).unwrap();
    assert!(resp.truncated);
    assert_eq!(resp.body, b"{\"partial\":");
}

#[tokio::test]
async fn malformed_reply_is_not_http() {
    let up = FakeUpstream::start(Behavior::Malformed(b"\x00\x01 not http at all".to_vec())).await;
    let (bytes, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    assert!(parse_response(&bytes).is_none());
    assert!(!bytes.is_empty());
}

#[tokio::test]
async fn sse_fragments_split_events_and_arrive_in_order() {
    let frags = vec![
        SseFragment::now(&b"data: {\"a\""[..]),
        SseFragment::after(Duration::from_millis(50), &b":1}\n"[..]),
        SseFragment::now(&b"\ndata: [DONE]\n\n"[..]),
    ];
    let up = FakeUpstream::start(Behavior::Sse {
        fragments: frags,
        framing: SseFraming::Chunked,
        finish: true,
    })
    .await;
    let (bytes, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    let resp = parse_response(&bytes).unwrap();
    assert!(!resp.truncated);
    assert_eq!(resp.body, b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
}

#[tokio::test]
async fn sse_without_finish_is_cut_short_and_close_delimited_works() {
    let up = FakeUpstream::start(Behavior::Sse {
        fragments: vec![SseFragment::now(&b"data: x\n\n"[..])],
        framing: SseFraming::Chunked,
        finish: false,
    })
    .await;
    let (bytes, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    assert!(parse_response(&bytes).unwrap().truncated);

    up.set_default(Behavior::Sse {
        fragments: vec![SseFragment::now(&b"data: y\n\n"[..])],
        framing: SseFraming::CloseDelimited,
        finish: true,
    });
    let (bytes, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    assert_eq!(parse_response(&bytes).unwrap().body, b"data: y\n\n");
}

#[tokio::test]
async fn behaviors_can_be_queued_per_call() {
    let up = FakeUpstream::start(Behavior::ok_json()).await;
    up.enqueue(Behavior::Json {
        status: 429,
        body: b"{}".to_vec(),
    });
    let (a, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    let (b, _) = exchange(up.addr(), &post("/x", &[], b"{}"), T)
        .await
        .unwrap();
    assert_eq!(parse_response(&a).unwrap().status, 429);
    assert_eq!(parse_response(&b).unwrap().status, 200);
}

/// The failure modes as seen by the real transport client stack (reqwest with rustls,
/// the same builder settings `transport::Upstream` uses), over plain loopback HTTP.
#[tokio::test]
async fn transport_client_observes_the_failure_modes() {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();

    let up = FakeUpstream::start(Behavior::ok_json()).await;
    let ok = client.post(up.base_url()).body("{}").send().await.unwrap();
    assert_eq!(ok.status().as_u16(), 200);

    up.set_default(Behavior::DisconnectBeforeResponse);
    assert!(client.post(up.base_url()).body("{}").send().await.is_err());

    up.set_default(Behavior::Malformed(b"garbage".to_vec()));
    assert!(client.post(up.base_url()).body("{}").send().await.is_err());

    up.set_default(Behavior::Slow {
        delay: Duration::from_secs(3),
        then: Box::new(Behavior::ok_json()),
    });
    let err = client
        .post(up.base_url())
        .body("{}")
        .send()
        .await
        .unwrap_err();
    assert!(err.is_timeout());

    up.set_default(Behavior::DisconnectMidBody {
        declared_len: 50,
        sent: b"{".to_vec(),
    });
    let resp = client.post(up.base_url()).body("{}").send().await.unwrap();
    assert!(
        resp.bytes().await.is_err(),
        "truncated body must surface as an error"
    );
}
