//! Chat Completions admission and strict parsing, over real HTTP on loopback (issue #18).
//!
//! Every case runs against the served router. A fake upstream is running for each case
//! and must observe zero connections and zero body bytes (there is no forwarding yet, so
//! this is the standing no-forward evidence, and it applies unchanged once #20 wires an
//! upstream URL). Gateway responses are scanned for synthetic markers. All data is
//! synthetic.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod support;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use redact_secret_gateway::admission::{Admission, CapacityPlan, RequestLimits};
use redact_secret_gateway::chat_route::{Admitted, CHAT_COMPLETIONS_PATH, ChatRoute, Reject};
use redact_secret_gateway::config;
use redact_secret_gateway::config::RouteId;
use redact_secret_gateway::protocol::chat::TextSlot;
use redact_secret_gateway::server::{self, Services, StartupError};
use redact_secret_gateway::transport::destination::OPENAI_CHAT_COMPLETIONS_ROUTE;
use support::fake_upstream::{Behavior, FakeUpstream};
use support::leak::{BODY_MARKER, HEADER_MARKER, KEY_MARKER, Markers};
use support::raw_http::{Response, parse_response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const GOOD: &str = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}"#;

fn config_json(receipt: u32, memory: u32, limits: &str) -> String {
    format!(
        r#"{{"schema_version":1,
            "deployment":{{"listener":{{"address":"127.0.0.1:0"}}}},
            "content":{{"profile":"common"}},
            "resources":{{"capacity":{{"receipt":{receipt},"memory_units":{memory},
                "inspection":1,"upstream":1,"stream":1}},"limits":{limits}}}}}"#
    )
}

struct Gateway {
    addr: SocketAddr,
    chat: Arc<ChatRoute>,
    memory: u32,
    receipt: u32,
    upstream: FakeUpstream,
    markers: Markers,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

impl Gateway {
    /// Roomy capacity and provisional limits.
    async fn roomy() -> Self {
        Self::start(4, 8192, "{}").await
    }

    async fn start(receipt: u32, memory: u32, limits: &str) -> Self {
        let plan =
            Arc::new(config::parse(config_json(receipt, memory, limits).as_bytes()).unwrap());
        let bound = server::bind(plan, Services::init).await.unwrap();
        let addr = bound.local_addr().unwrap();
        let chat = bound.chat();
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(bound.serve(async move {
            let _ = rx.await;
        }));
        Self {
            addr,
            chat,
            memory,
            receipt,
            upstream: FakeUpstream::start(Behavior::ok_json()).await,
            markers: Markers::standard(),
            stop: Some(stop),
            task,
        }
    }

    /// Send raw bytes, read until close or timeout, return the parsed response.
    async fn send(&self, request: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let _ = stream.write_all(request).await;
        self.read(&mut stream).await
    }

    async fn read(&self, stream: &mut TcpStream) -> Response {
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Err(_) => panic!("timed out waiting for a response"),
            }
        }
        let response = parse_response(&out).expect("a complete HTTP response");
        // Gateway-generated text never echoes request content.
        self.markers.assert_clean("response", &out);
        response
    }

    async fn post(&self, headers: &[(&str, &str)], body: &[u8]) -> Response {
        self.send(&support::raw_http::post(
            CHAT_COMPLETIONS_PATH,
            headers,
            body,
        ))
        .await
    }

    async fn post_json(&self, body: &str) -> Response {
        self.post(&[("Content-Type", "application/json")], body.as_bytes())
            .await
    }

    /// Open a connection and write `head` without reading.
    async fn open(&self, head: &str) -> TcpStream {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        stream
    }

    /// Every capacity returned and nothing reached the fake upstream.
    async fn assert_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let adm = self.chat.admission();
            let memory = adm.try_reserve_memory(self.memory);
            let receipts: Vec<_> = (0..self.receipt).map(|_| adm.try_receipt()).collect();
            if memory.is_ok() && receipts.iter().all(Result::is_ok) {
                break;
            }
            assert!(Instant::now() < deadline, "capacity was not returned");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.upstream.assert_nothing_sent();
    }

    /// Wait until the receipt/memory reservation is held.
    async fn wait_until_memory_held(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self
            .chat
            .admission()
            .try_reserve_memory(self.memory)
            .is_ok()
        {
            assert!(Instant::now() < deadline, "reservation was never taken");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.task).await;
    }
}

fn body_text(r: &Response) -> String {
    String::from_utf8(r.body.clone()).unwrap()
}

fn expect(r: &Response, status: u16, code: &str) {
    assert_eq!(r.status, status, "{}", body_text(r));
    assert_eq!(body_text(r), format!(r#"{{"error":{{"code":"{code}"}}}}"#));
    assert!(
        r.headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close")),
        "rejections close the connection"
    );
}

// ---------------------------------------------------------------------------------------
// Route, method, content type, and framing admission.
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn supported_request_is_admitted_and_validated_then_rejected_locally() {
    let gw = Gateway::roomy().await;
    let r = gw.post_json(GOOD).await;
    expect(&r, 501, "not_implemented");
    // The served route is bound to the reviewed route id, never to anything in the request.
    assert_eq!(gw.chat.route().as_str(), OPENAI_CHAT_COMPLETIONS_ROUTE);
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn content_type_forms() {
    let gw = Gateway::roomy().await;
    for ct in [
        "application/json",
        "Application/JSON",
        "application/json; charset=utf-8",
        "application/json;charset=\"UTF-8\"",
    ] {
        let r = gw.post(&[("Content-Type", ct)], GOOD.as_bytes()).await;
        expect(&r, 501, "not_implemented");
    }
    for ct in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/jsonp",
        "application/json; charset=latin1",
        "application/json; version=1",
        "application/json;",
        "application/ld+json",
        "",
    ] {
        let r = gw.post(&[("Content-Type", ct)], GOOD.as_bytes()).await;
        expect(&r, 415, "unsupported_input");
    }
    // Missing and duplicated Content-Type.
    let r = gw.post(&[], GOOD.as_bytes()).await;
    expect(&r, 415, "unsupported_input");
    let r = gw
        .post(
            &[
                ("Content-Type", "application/json"),
                ("Content-Type", "application/json"),
            ],
            GOOD.as_bytes(),
        )
        .await;
    expect(&r, 415, "unsupported_input");
    // A marker in a rejected header is never echoed (checked inside `read`).
    let r = gw
        .post(&[("Content-Type", HEADER_MARKER)], GOOD.as_bytes())
        .await;
    expect(&r, 415, "unsupported_input");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn compression_and_transfer_codings_are_rejected() {
    let gw = Gateway::roomy().await;
    for encoding in ["gzip", "identity", "br", "deflate", "zstd", "gzip, br"] {
        let r = gw
            .post(
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Encoding", encoding),
                ],
                GOOD.as_bytes(),
            )
            .await;
        expect(&r, 415, "unsupported_input");
    }
    // A compressed-looking body without the header is just invalid JSON.
    let r = gw
        .post(
            &[("Content-Type", "application/json")],
            &[0x1f, 0x8b, 0x08, 0x00],
        )
        .await;
    expect(&r, 400, "malformed_input");
    // Transfer codings other than a lone `chunked`.
    for te in ["gzip, chunked", "gzip", "chunked, chunked"] {
        let req = format!(
            "POST {CHAT_COMPLETIONS_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\nTransfer-Encoding: {te}\r\n\r\n0\r\n\r\n"
        );
        let r = gw.send(req.as_bytes()).await;
        assert!(
            r.status == 415 || r.status == 400,
            "transfer coding {te}: {}",
            r.status
        );
    }
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn route_method_and_target_are_exact() {
    let gw = Gateway::roomy().await;
    let ct = "Content-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\n";
    for (method, status) in [("GET", 405), ("PUT", 405), ("DELETE", 405), ("PATCH", 405)] {
        let req = format!(
            "{method} {CHAT_COMPLETIONS_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{ct}\
             Content-Length: {}\r\n\r\n{GOOD}",
            GOOD.len()
        );
        let r = gw.send(req.as_bytes()).await;
        expect(&r, status, "unsupported_input");
        assert!(
            r.headers
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case("allow") && v == "POST")
        );
    }
    // Other paths never reach the handler.
    for path in [
        "/v1/chat/completions/",
        "/V1/chat/completions",
        "/v1/chat/completions%2f",
        "/v1/chat",
        "/v1/responses",
        "/v1/completions",
        "//v1/chat/completions",
        "/v1/chat/completions/../completions",
        "/",
    ] {
        let r = gw
            .send(&support::raw_http::post(
                path,
                &[("Content-Type", "application/json")],
                GOOD.as_bytes(),
            ))
            .await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(body_text(&r), r#"{"error":{"code":"unsupported_input"}}"#);
    }
    // Query strings (even empty) are rejected.
    for path in [
        "/v1/chat/completions?",
        "/v1/chat/completions?x=1",
        "/v1/chat/completions?api-version=2024-01-01",
        &format!("/v1/chat/completions?{BODY_MARKER}=1"),
    ] {
        let r = gw
            .send(&support::raw_http::post(
                path,
                &[("Content-Type", "application/json")],
                GOOD.as_bytes(),
            ))
            .await;
        expect(&r, 400, "unsupported_input");
    }
    // CONNECT never matches the route.
    let r = gw
        .send(b"CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\nConnection: close\r\n\r\n")
        .await;
    assert!((400..500).contains(&r.status), "{}", r.status);
    // Absolute-form targets never choose a destination.
    let r = gw
        .send(&support::raw_http::post(
            "http://evil.invalid/v1/chat/completions",
            &[("Content-Type", "application/json")],
            GOOD.as_bytes(),
        ))
        .await;
    expect(&r, 400, "unsupported_input");
    // Upgrade requests.
    let r = gw
        .post(
            &[
                ("Content-Type", "application/json"),
                ("Connection", "Upgrade"),
                ("Upgrade", "websocket"),
            ],
            GOOD.as_bytes(),
        )
        .await;
    expect(&r, 400, "unsupported_input");
    // Health stays local and untouched.
    let r = gw
        .send(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert_eq!(r.status, 200);
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn malformed_framing_is_rejected() {
    let gw = Gateway::roomy().await;
    let head = format!(
        "POST {CHAT_COMPLETIONS_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\n"
    );
    // Duplicate, mismatched, non-numeric, signed, spaced Content-Length values.
    for lengths in [
        "Content-Length: 5\r\nContent-Length: 6\r\n",
        "Content-Length: 5, 5\r\n",
        "Content-Length: abc\r\n",
        "Content-Length: -5\r\n",
        "Content-Length: +5\r\n",
        "Content-Length: 5.0\r\n",
        "Content-Length: 0x5\r\n",
    ] {
        let r = gw
            .send(format!("{head}{lengths}\r\n12345").as_bytes())
            .await;
        assert!(r.status == 400, "{lengths}: {}", r.status);
    }
    // Both Content-Length and Transfer-Encoding: the HTTP parser alone would resolve this in
    // favour of chunked framing (RFC 9112), invisibly to the route. The connection-level
    // head guard (#25, ADR 0019, #43) refuses it before the parser sees it with a local
    // 400 and closes the connection; nothing is admitted. (Detailed matrix:
    // src/transport/tests/attack_tests.rs.)
    let r = gw
        .send(
            format!(
                "{head}Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n12345\r\n0\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
    expect(&r, 400, "malformed_input");
    // No body and no framing at all: nothing to receive.
    let r = gw.send(format!("{head}\r\n").as_bytes()).await;
    expect(&r, 400, "malformed_input");
    let r = gw
        .send(format!("{head}Content-Length: 0\r\n\r\n").as_bytes())
        .await;
    expect(&r, 400, "malformed_input");
    // Malformed chunk framing.
    let r = gw
        .send(format!("{head}Transfer-Encoding: chunked\r\n\r\nZZ\r\nabc\r\n0\r\n\r\n").as_bytes())
        .await;
    assert_eq!(r.status, 400);
    gw.assert_idle().await;
    gw.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Endpoint matrix over HTTP: unsupported content is rejected, never forwarded.
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn unsupported_payloads_are_rejected_with_zero_upstream_bytes() {
    let gw = Gateway::roomy().await;
    let unknown_key = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"x"}}],"{KEY_MARKER}":"{BODY_MARKER}"}}"#
    );
    let unknown_nested = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"x","{KEY_MARKER}":"{BODY_MARKER}"}}]}}"#
    );
    let unknown_part = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":[{{"type":"text","text":"x","{KEY_MARKER}":1}}]}}]}}"#
    );
    let cases: Vec<(&str, String)> = vec![
        ("unknown top-level", unknown_key),
        ("unknown message field", unknown_nested),
        ("unknown part field", unknown_part),
        (
            "tools",
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"f","description":"d","parameters":{}}}]}"#.into(),
        ),
        (
            "tool_choice",
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"tool_choice":"auto"}"#.into(),
        ),
        (
            "tool message",
            r#"{"model":"m","messages":[{"role":"tool","tool_call_id":"c","content":"x"}]}"#.into(),
        ),
        (
            "functions",
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"functions":[]}"#.into(),
        ),
        (
            "image part",
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/a.png"}}]}]}"#.into(),
        ),
        (
            "audio part",
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"AAAA","format":"wav"}}]}]}"#.into(),
        ),
        (
            "file part",
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"f"}}]}]}"#.into(),
        ),
        (
            "null content",
            r#"{"model":"m","messages":[{"role":"assistant","content":null}]}"#.into(),
        ),
        (
            "opaque content object",
            r#"{"model":"m","messages":[{"role":"user","content":{"encrypted":"AAAA"}}]}"#.into(),
        ),
        (
            "n greater than one",
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"n":2}"#.into(),
        ),
        (
            "json_schema format",
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_schema","json_schema":{}}}"#.into(),
        ),
        ("not an object", r#"["x"]"#.into()),
        ("missing model", r#"{"messages":[{"role":"user","content":"x"}]}"#.into()),
        ("empty messages", r#"{"model":"m","messages":[]}"#.into()),
    ];
    for (name, body) in &cases {
        let r = gw.post_json(body).await;
        assert_eq!(r.status, 422, "{name}");
        expect(&r, 422, "unsupported_input");
        gw.upstream.assert_nothing_sent();
    }
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn malformed_json_is_rejected_with_zero_upstream_bytes() {
    let gw = Gateway::roomy().await;
    let nested_dup = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"{BODY_MARKER}","role":"system"}}]}}"#
    );
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "top-level duplicate",
            format!(r#"{{"model":"m","model":"{BODY_MARKER}","messages":[]}}"#).into_bytes(),
        ),
        ("nested duplicate", nested_dup.into_bytes()),
        (
            "escaped duplicate key",
            format!(r#"{{"model":"m","model":"{BODY_MARKER}","messages":[]}}"#).into_bytes(),
        ),
        (
            "escaped nested duplicate",
            r#"{"model":"m","messages":[{"role":"user","role":"system","content":"x"}]}"#
                .as_bytes()
                .to_vec(),
        ),
        (
            "marker duplicate key",
            format!(r#"{{"{KEY_MARKER}":1,"{KEY_MARKER}":2}}"#).into_bytes(),
        ),
        (
            "truncated",
            format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{BODY_MARKER}""#)
                .into_bytes(),
        ),
        (
            "trailing data",
            format!(r#"{GOOD} {BODY_MARKER}"#).into_bytes(),
        ),
        ("two documents", format!("{GOOD}{GOOD}").into_bytes()),
        ("not json", BODY_MARKER.as_bytes().to_vec()),
        ("bom", [&[0xEF, 0xBB, 0xBF][..], GOOD.as_bytes()].concat()),
        (
            "invalid utf8 in string",
            [
                br#"{"model":"m","messages":[{"role":"user","content":""#.as_slice(),
                &[0xff, 0xfe],
                br#""}]}"#,
            ]
            .concat(),
        ),
        (
            "overlong utf8",
            [
                br#"{"model":"m","messages":[{"role":"user","content":""#.as_slice(),
                &[0xc0, 0xaf],
                br#""}]}"#,
            ]
            .concat(),
        ),
        (
            "lone high surrogate",
            br#"{"model":"m","messages":[{"role":"user","content":"\ud800"}]}"#.to_vec(),
        ),
        (
            "lone low surrogate",
            br#"{"model":"m","messages":[{"role":"user","content":"\udc00"}]}"#.to_vec(),
        ),
        (
            "high surrogate then non-surrogate",
            br#"{"model":"m","messages":[{"role":"user","content":"\ud800A"}]}"#.to_vec(),
        ),
        (
            "raw control character in string",
            b"{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"a\nb\"}]}".to_vec(),
        ),
        (
            "bad escape",
            br#"{"model":"m","messages":[{"role":"user","content":"\x41"}]}"#.to_vec(),
        ),
        ("single quotes", b"{'model':'m'}".to_vec()),
        (
            "trailing comma",
            br#"{"model":"m","messages":[],}"#.to_vec(),
        ),
        (
            "nan",
            br#"{"model":"m","messages":[{"role":"user","content":"x"}],"temperature":NaN}"#
                .to_vec(),
        ),
        (
            "leading zero",
            br#"{"model":"m","messages":[{"role":"user","content":"x"}],"seed":01}"#.to_vec(),
        ),
        (
            "float overflow",
            br#"{"model":"m","messages":[{"role":"user","content":"x"}],"temperature":1e999}"#
                .to_vec(),
        ),
        ("comment", br#"{"model":"m",/*x*/"messages":[]}"#.to_vec()),
    ];
    for (name, body) in &cases {
        let r = gw.post(&[("Content-Type", "application/json")], body).await;
        assert_eq!(r.status, 400, "{name}");
        expect(&r, 400, "malformed_input");
        gw.upstream.assert_nothing_sent();
    }
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn huge_numbers_are_rejected_not_wrapped() {
    let gw = Gateway::roomy().await;
    for field in [
        r#""max_tokens":99999999999999999999999999999"#,
        r#""max_tokens":18446744073709551616"#,
        r#""max_tokens":9223372036854775808"#,
        r#""max_tokens":2147483648"#,
        r#""seed":18446744073709551615"#,
        r#""temperature":1e308"#,
        r#""temperature":-1e308"#,
        r#""n":99999999999999999999"#,
    ] {
        let body =
            format!(r#"{{"model":"m","messages":[{{"role":"user","content":"x"}}],{field}}}"#);
        let r = gw.post_json(&body).await;
        expect(&r, 422, "unsupported_input");
    }
    gw.assert_idle().await;
    gw.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Typed result of the real admission path: decoded text, Unicode, escaping.
// ---------------------------------------------------------------------------------------

fn direct_route() -> ChatRoute {
    let n = |v| NonZeroU32::new(v).unwrap();
    let admission = Arc::new(Admission::new(&CapacityPlan::new(
        n(4),
        n(8192),
        n(1),
        n(1),
        n(1),
    )));
    ChatRoute::new(
        admission,
        RequestLimits::provisional(),
        RouteId::new(OPENAI_CHAT_COMPLETIONS_ROUTE),
    )
}

fn request(body: &[u8]) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(CHAT_COMPLETIONS_PATH)
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY")
        .header("content-length", body.len().to_string())
        .body(Body::from(body.to_vec()))
        .unwrap()
}

async fn texts_of(body: &str) -> Vec<(TextSlot, String)> {
    let route = direct_route();
    let validated = route.admit(request(body.as_bytes())).await.unwrap();
    let mut out = Vec::new();
    validated
        .chat()
        .expect("chat request")
        .for_each_text(|slot, text| out.push((slot, text.to_owned())));
    out
}

#[tokio::test]
async fn supported_forms_parse_into_the_typed_contract() {
    let route = direct_route();
    let body = r#"{"model":"gpt-4o","stream":false,"temperature":0.2,"max_tokens":64,
        "messages":[
          {"role":"system","content":"be brief"},
          {"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
          {"role":"assistant","content":"ok"}],
        "stop":["END"],"user":"tester"}"#;
    let v = route.admit(request(body.as_bytes())).await.unwrap();
    let chat = v.chat().expect("chat request");
    assert_eq!(chat.model(), "gpt-4o");
    assert_eq!(chat.messages().len(), 3);
    assert_eq!(chat.stream(), Some(false));
    assert_eq!(chat.text_count(), 6);
    assert_eq!(v.route().as_str(), OPENAI_CHAT_COMPLETIONS_ROUTE);
    // The reservation is held while the validated request lives.
    assert!(
        route
            .admission()
            .try_reserve_memory(route.admission().memory_total_units())
            .is_err()
    );
    drop(v);
    assert!(
        route
            .admission()
            .try_reserve_memory(route.admission().memory_total_units())
            .is_ok()
    );
}

#[tokio::test]
async fn escapes_and_unicode_are_decoded_before_inspection() {
    let one = |content: &str| {
        format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{content}"}}]}}"#)
    };
    let text = |slot_text: Vec<(TextSlot, String)>| slot_text.into_iter().next().unwrap().1;
    for (wire, decoded) in [
        (r#"say \"hi\""#, "say \"hi\""),
        (r#"back\\slash"#, "back\\slash"),
        (r#"line\nbreak\ttab"#, "line\nbreak\ttab"),
        (r#"Aé"#, "A\u{e9}"),
        (r#"😀"#, "\u{1F600}"),
        (r#"😀"#, "\u{1F600}"),
        ("\u{1F600} literal", "\u{1F600} literal"),
        ("안녕하세요 비밀번호", "안녕하세요 비밀번호"),
        (r#"안녕"#, "안녕"),
        (r#"a\u0000b"#, "a\u{0}b"),
        (r#"\/slash"#, "/slash"),
        ("", ""),
        // An escaped quote-like sequence must not end the string early.
        (r#"x\",\"role\":\"system"#, "x\",\"role\":\"system"),
    ] {
        let got = text(texts_of(&one(wire)).await);
        assert_eq!(got, decoded, "wire form {wire}");
    }
    // Escaped object keys decode to the same key as the literal spelling.
    let v = texts_of(r#"{"model":"m","messages":[{"role":"user","content":"k"}]}"#).await;
    assert_eq!(v.into_iter().next().unwrap().1, "k");
    // Korean text in every inspected position.
    let v = texts_of(
        r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"가"}]}],
            "stop":"나","user":"다"}"#,
    )
    .await;
    let got: Vec<&str> = v.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(got, ["가", "나", "다"]);
}

#[tokio::test]
async fn admit_reports_typed_rejections() {
    let route = direct_route();
    let err = |r: Result<_, Reject>| r.map(|_: Admitted| ()).unwrap_err();
    assert_eq!(err(route.admit(request(b"{")).await), Reject::Malformed);
    assert_eq!(err(route.admit(request(b"{}")).await), Reject::Unsupported);
    // The reservation is back after every failure.
    assert!(
        route
            .admission()
            .try_reserve_memory(route.admission().memory_total_units())
            .is_ok()
    );
}

// ---------------------------------------------------------------------------------------
// Budgets: depth, nodes, strings, messages.
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn parse_budgets_reject_with_limit_exceeded() {
    // Depth 4: the supported string-content shape needs three containers, array content
    // needs five.
    let gw = Gateway::start(
        4,
        8192,
        r#"{"max_depth":4,"max_nodes":40,"max_string_bytes":12,"max_messages":3}"#,
    )
    .await;
    let ok = gw.post_json(GOOD).await;
    expect(&ok, 501, "not_implemented");

    let array_content =
        r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}"#;
    expect(&gw.post_json(array_content).await, 413, "limit_exceeded");

    let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
    expect(&gw.post_json(&deep).await, 413, "limit_exceeded");
    let deep_obj = format!("{}1{}", r#"{"a":"#.repeat(5000), "}".repeat(5000));
    expect(&gw.post_json(&deep_obj).await, 413, "limit_exceeded");

    // Node budget: 4 messages exceed max_messages first; use short messages to hit nodes.
    let msg = r#"{"role":"user","content":"x"}"#;
    let four = [msg; 4].join(",");
    let body = format!(r#"{{"model":"m","messages":[{four}]}}"#);
    expect(&gw.post_json(&body).await, 413, "limit_exceeded");
    let many_nodes = format!(
        r#"{{"model":"m","messages":[{msg}],"stop":["a","b","c","d"],"x":[{}]}}"#,
        vec!["1"; 60].join(",")
    );
    // Unknown field and node budget: the budget fires while parsing, before matrix checks.
    expect(&gw.post_json(&many_nodes).await, 413, "limit_exceeded");

    // String budget counts decoded bytes: 13 decoded bytes from 78 wire bytes.
    let escaped = r"A".repeat(13);
    let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{escaped}"}}]}}"#);
    expect(&gw.post_json(&body).await, 413, "limit_exceeded");
    let escaped = r"A".repeat(12);
    let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{escaped}"}}]}}"#);
    expect(&gw.post_json(&body).await, 501, "not_implemented");
    // A long key is a string too.
    let body = format!(r#"{{"model":"m","{}":1,"messages":[]}}"#, "k".repeat(13));
    expect(&gw.post_json(&body).await, 413, "limit_exceeded");

    gw.assert_idle().await;
    gw.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Resource bounds: oversized, unknown-length, slow input versus aggregate reservations.
// ---------------------------------------------------------------------------------------

fn limits_json(max_body: u32, wait_ms: u32, queue: u32, deadline_ms: u32) -> String {
    format!(
        r#"{{"max_body_bytes":{max_body},"admission_wait_ms":{wait_ms},"admission_queue":{queue},"body_deadline_ms":{deadline_ms}}}"#
    )
}

fn one_max_request_units(max_body: u32) -> u32 {
    let mut limits = RequestLimits::provisional();
    limits.max_body_bytes = max_body;
    limits.reservation_units(max_body as usize)
}

const CHUNKED_HEAD: &str = "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n\
    Connection: close\r\nContent-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\nTransfer-Encoding: chunked\r\n\r\n";

#[tokio::test]
async fn oversized_declared_length_is_rejected_before_any_body_is_read() {
    let gw = Gateway::start(
        4,
        one_max_request_units(2048),
        &limits_json(2048, 50, 4, 2000),
    )
    .await;
    for declared in ["2049", "1073741824", "99999999999999999999999999"] {
        let head = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\nContent-Length: {declared}\r\n\r\n"
        );
        // No body byte is ever sent; the answer must not wait for one.
        let started = Instant::now();
        let r = gw.send(head.as_bytes()).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        if declared.len() > 19 {
            // Beyond u64: the HTTP layer itself refuses the framing before our handler.
            assert!(r.status == 400 || r.status == 413, "{}", r.status);
        } else {
            expect(&r, 413, "limit_exceeded");
        }
    }
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn oversized_unknown_length_body_is_cut_off_at_the_reservation() {
    let gw = Gateway::start(
        4,
        one_max_request_units(1024),
        &limits_json(1024, 50, 4, 5000),
    )
    .await;
    let chunk = vec![b'x'; 512];
    let request = support::raw_http::post_chunked(
        CHAT_COMPLETIONS_PATH,
        &[chunk.as_slice(), chunk.as_slice(), chunk.as_slice()],
    );
    let r = gw.send(&request).await;
    // `post_chunked` has no content type: that is rejected first, with nothing reserved.
    expect(&r, 415, "unsupported_input");
    let mut with_type = CHUNKED_HEAD.as_bytes().to_vec();
    for _ in 0..3 {
        with_type.extend_from_slice(b"200\r\n");
        with_type.extend_from_slice(&chunk);
        with_type.extend_from_slice(b"\r\n");
    }
    with_type.extend_from_slice(b"0\r\n\r\n");
    let r = gw.send(&with_type).await;
    expect(&r, 413, "limit_exceeded");
    // Exactly at the limit is accepted for receipt (and then fails JSON validation).
    let mut at_limit = CHUNKED_HEAD.as_bytes().to_vec();
    at_limit.extend_from_slice(b"400\r\n");
    at_limit.extend_from_slice(&vec![b' '; 1024]);
    at_limit.extend_from_slice(b"\r\n0\r\n\r\n");
    let r = gw.send(&at_limit).await;
    expect(&r, 400, "malformed_input");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn reservation_is_taken_before_the_first_body_byte_and_released_on_deadline() {
    let units = one_max_request_units(2048);
    let gw = Gateway::start(4, units, &limits_json(2048, 50, 4, 400)).await;
    // Headers only: not one body byte has been sent.
    let mut slow = gw.open(CHUNKED_HEAD).await;
    gw.wait_until_memory_held().await;
    // The unknown-length request owns the whole budget: every other request overloads.
    let r = gw.post_json(GOOD).await;
    expect(&r, 503, "overload");
    assert!(
        r.headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("retry-after") && v == "1")
    );
    // The slow body hits the body deadline and the budget returns.
    let r = gw.read(&mut slow).await;
    expect(&r, 408, "limit_exceeded");
    gw.assert_idle().await;
    let r = gw.post_json(GOOD).await;
    expect(&r, 501, "not_implemented");
    gw.shutdown().await;
}

#[tokio::test]
async fn trickled_body_cannot_outlive_the_deadline() {
    let units = one_max_request_units(2048);
    let gw = Gateway::start(4, units, &limits_json(2048, 50, 4, 500)).await;
    let slow = gw.open(CHUNKED_HEAD).await;
    let started = Instant::now();
    // One byte every 100 ms keeps the connection "active" but cannot finish in time.
    let (mut rd, mut wr) = slow.into_split();
    let write_task = tokio::spawn(async move {
        for _ in 0..30 {
            if wr.write_all(b"1\r\n \r\n").await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let mut out = Vec::new();
    let mut buf = [0_u8; 1024];
    while let Ok(n) = rd.read(&mut buf).await {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    write_task.abort();
    let r = parse_response(&out).expect("response");
    expect(&r, 408, "limit_exceeded");
    assert!(started.elapsed() < Duration::from_secs(4));
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn declared_length_larger_than_what_arrives_times_out_and_releases() {
    let units = one_max_request_units(2048);
    let gw = Gateway::start(4, units, &limits_json(2048, 50, 4, 300)).await;
    let head = "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                Content-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\nContent-Length: 100\r\n\r\n{\"model\"";
    let mut slow = gw.open(head).await;
    let r = gw.read(&mut slow).await;
    expect(&r, 408, "limit_exceeded");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn aggregate_memory_bounds_concurrent_receipts() {
    // Memory for exactly one maximum-size request, receipt capacity for several.
    let units = one_max_request_units(2048);
    let gw = Gateway::start(8, units, &limits_json(2048, 50, 8, 600)).await;
    let mut held = gw.open(CHUNKED_HEAD).await;
    gw.wait_until_memory_held().await;
    // However many more arrive, none can reserve beyond the budget.
    let mut results = Vec::new();
    for _ in 0..5 {
        results.push(gw.post_json(GOOD).await);
    }
    for r in &results {
        expect(r, 503, "overload");
    }
    // Total reserved never exceeded the budget: nothing else was reservable meanwhile.
    assert!(gw.chat.admission().try_reserve_memory(1).is_err());
    let r = gw.read(&mut held).await;
    expect(&r, 408, "limit_exceeded");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn admission_queue_is_bounded_and_waiters_proceed_when_capacity_returns() {
    let units = one_max_request_units(2048);
    // One waiter may queue for up to 3 s; the holder gives up its reservation at 500 ms.
    let gw = Arc::new(Gateway::start(8, units, &limits_json(2048, 3000, 1, 500)).await);
    let mut holder = gw.open(CHUNKED_HEAD).await;
    gw.wait_until_memory_held().await;

    let waiter = {
        let gw = Arc::clone(&gw);
        tokio::spawn(async move { gw.post_json(GOOD).await })
    };
    // Give the waiter time to enter the queue.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The queue is full: this one fails immediately instead of waiting.
    let started = Instant::now();
    let r = gw.post_json(GOOD).await;
    assert!(started.elapsed() < Duration::from_millis(400));
    expect(&r, 503, "overload");

    // The holder's deadline releases the budget; the queued request is then served.
    let r = gw.read(&mut holder).await;
    expect(&r, 408, "limit_exceeded");
    let r = waiter.await.unwrap();
    expect(&r, 501, "not_implemented");
    gw.assert_idle().await;
    Arc::into_inner(gw).unwrap().shutdown().await;
}

#[tokio::test]
async fn zero_wait_means_immediate_overload() {
    let units = one_max_request_units(2048);
    let gw = Gateway::start(8, units, &limits_json(2048, 0, 0, 500)).await;
    let mut holder = gw.open(CHUNKED_HEAD).await;
    gw.wait_until_memory_held().await;
    let started = Instant::now();
    let r = gw.post_json(GOOD).await;
    assert!(started.elapsed() < Duration::from_millis(300));
    expect(&r, 503, "overload");
    let _ = gw.read(&mut holder).await;
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn receipt_permits_bound_concurrent_connections_doing_receipt() {
    // Plenty of memory, but one receipt permit.
    let gw = Gateway::start(1, 8192, &limits_json(2048, 0, 0, 500)).await;
    let mut holder = gw.open(CHUNKED_HEAD).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while gw.chat.admission().try_receipt().is_ok() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    expect(&gw.post_json(GOOD).await, 503, "overload");
    let _ = gw.read(&mut holder).await;
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn client_disconnect_during_receipt_releases_capacity() {
    let units = one_max_request_units(2048);
    let gw = Gateway::start(4, units, &limits_json(2048, 50, 4, 10_000)).await;
    let holder = gw.open(CHUNKED_HEAD).await;
    gw.wait_until_memory_held().await;
    drop(holder);
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn aggregate_budget_smaller_than_the_per_request_limit_clamps_the_body() {
    // Per-request limit 1 MiB, but the global budget only covers a few KiB.
    let gw = Gateway::start(4, 64, "{}").await;
    let max = gw.chat.effective_max_body();
    assert!(max > 0 && max < 1_048_576);
    let declared = max + 1;
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: application/json\r\nAuthorization: Bearer sk-SYNTHETIC-REVOKED-ADM-NOT-A-KEY\r\nContent-Length: {declared}\r\n\r\n"
    );
    expect(&gw.send(head.as_bytes()).await, 413, "limit_exceeded");
    gw.assert_idle().await;
    gw.shutdown().await;
}

// ---------------------------------------------------------------------------------------
// Standing evidence: nothing the gateway generates carries payload, and nothing is sent.
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn every_rejection_is_marker_free_and_upstream_stays_empty() {
    let gw = Gateway::roomy().await;
    let marker_bodies = [
        format!(r#"{{"model":"{BODY_MARKER}","messages":[]}}"#),
        format!(r#"{{"{KEY_MARKER}":"{BODY_MARKER}"}}"#),
        format!(r#"{{"model":"m","messages":[{{"role":"{BODY_MARKER}","content":"x"}}]}}"#),
        format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":[{{"type":"{BODY_MARKER}"}}]}}]}}"#
        ),
        format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"{BODY_MARKER}"}}],"temperature":"{BODY_MARKER}"}}"#
        ),
        format!(r#"{{"{KEY_MARKER}":1,"{KEY_MARKER}":"{BODY_MARKER}"}}"#),
        format!(r#"["{BODY_MARKER}""#),
    ];
    for body in &marker_bodies {
        let r = gw.post_json(body).await; // `read` scans the raw response bytes.
        assert!(r.status == 400 || r.status == 422, "{}", r.status);
    }
    let r = gw
        .post(
            &[
                ("Content-Type", "application/json"),
                ("Content-Encoding", HEADER_MARKER),
                ("X-Marker", HEADER_MARKER),
            ],
            GOOD.as_bytes(),
        )
        .await;
    expect(&r, 415, "unsupported_input");
    gw.assert_idle().await;
    gw.shutdown().await;
}
