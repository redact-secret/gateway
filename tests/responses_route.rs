//! The served `POST /v1/responses` route through the real server (#86; ADR 0031).
//!
//! No upstream is configured here (there is deliberately no production path to a fake
//! provider), so these tests prove the front half over a real socket: the exact route, local
//! caller authentication before anything else, the shared head and framing admission, the
//! Responses matrix, and the fail-closed `501` when no destination is configured. The
//! one-send and credential-isolation evidence against fake providers is in
//! `src/transport/tests/responses_route_tests.rs`. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use redact_secret_gateway::chat_route::{CHAT_COMPLETIONS_PATH, RESPONSES_PATH};
use redact_secret_gateway::config;
use redact_secret_gateway::server::{self, Services, StartupError};
use redact_secret_gateway::transport::local_auth::{LocalToken, TokenReference};
use support::raw_http::{Response, parse_response, post, post_exact};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const LOCAL: &str = "SYNTH-LOCAL-TOKEN-0123456789-abcdefghijklmno";
const GOOD: &str = r#"{"model":"gpt-4o-mini","input":"hello","store":false}"#;
const CHAT: &str = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}"#;
const JSON: (&str, &str) = ("Content-Type", "application/json");
const AUTH: (&str, &str) = (
    "Authorization",
    "Bearer sk-SYNTHETIC-REVOKED-RESP-0000-NOT-A-KEY",
);

struct Gateway {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

impl Gateway {
    async fn start(local_auth: bool) -> Self {
        let auth = if local_auth {
            r#","local_auth":{"mode":"token","token":{"env":"RSG_TEST_RESPONSES_TOKEN"}}"#
                .to_owned()
        } else {
            String::new()
        };
        let json = format!(
            r#"{{"schema_version":1,
              "deployment":{{"listener":{{"address":"127.0.0.1:0"}}{auth}}},
              "content":{{"profile":"common"}},
              "resources":{{"capacity":{{"receipt":4,"memory_units":8192,
                "inspection":2,"upstream":1,"stream":1}}}}}}"#
        );
        let resolve = |_: &TokenReference| LocalToken::from_bytes(LOCAL.as_bytes());
        let plan = Arc::new(config::parse_with(json.as_bytes(), &resolve).expect("plan"));
        let bound = server::bind(plan, Services::init).await.expect("bind");
        let addr = bound.local_addr().expect("addr");
        assert!(bound.responses().is_some(), "Responses is a served route");
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(bound.serve(async move {
            let _ = rx.await;
        }));
        Self {
            addr,
            stop: Some(stop),
            task,
        }
    }

    async fn send(&self, request: &[u8]) -> Response {
        let mut stream = TcpStream::connect(self.addr).await.expect("connect");
        let _ = stream.write_all(request).await;
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(8))
            .expect("deadline");
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Err(_) => panic!("timed out"),
            }
        }
        parse_response(&out).expect("a complete HTTP response")
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }
}

fn code(r: &Response) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

#[test]
fn the_paths_are_distinct_and_exact() {
    assert_eq!(RESPONSES_PATH, "/v1/responses");
    assert_ne!(RESPONSES_PATH, CHAT_COMPLETIONS_PATH);
}

#[tokio::test]
async fn a_valid_responses_request_without_a_configured_upstream_is_a_safe_501() {
    let gw = Gateway::start(false).await;
    let r = gw
        .send(&post(RESPONSES_PATH, &[JSON, AUTH], GOOD.as_bytes()))
        .await;
    assert_eq!(r.status, 501);
    assert_eq!(code(&r), r#"{"error":{"code":"not_implemented"}}"#);
    gw.stop().await;
}

#[tokio::test]
async fn path_method_and_protocol_mismatches_are_rejected_locally() {
    let gw = Gateway::start(false).await;
    for path in [
        "/v1/responses/",
        "/V1/responses",
        "/v1/response",
        "/v1/responses%2f",
        "//v1/responses",
    ] {
        let r = gw.send(&post(path, &[JSON, AUTH], GOOD.as_bytes())).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(code(&r), r#"{"error":{"code":"unsupported_input"}}"#);
    }
    for path in ["/v1/responses?", "/v1/responses?x=1"] {
        let r = gw.send(&post(path, &[JSON, AUTH], GOOD.as_bytes())).await;
        assert_eq!(r.status, 400, "{path}");
    }
    let get = b"GET /v1/responses HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let r = gw.send(get).await;
    assert_eq!(r.status, 405);
    // Bodies are matched to their own path: neither shape crosses over.
    let r = gw
        .send(&post(RESPONSES_PATH, &[JSON, AUTH], CHAT.as_bytes()))
        .await;
    assert_eq!(r.status, 422);
    let r = gw
        .send(&post(CHAT_COMPLETIONS_PATH, &[JSON, AUTH], GOOD.as_bytes()))
        .await;
    assert_eq!(r.status, 422);
    gw.stop().await;
}

#[tokio::test]
async fn the_provider_credential_is_required_and_the_matrix_applies() {
    let gw = Gateway::start(false).await;
    let r = gw
        .send(&post_exact(RESPONSES_PATH, &[JSON], GOOD.as_bytes()))
        .await;
    assert_eq!(r.status, 401);
    let stateful = br#"{"model":"m","input":"hi","store":true}"#;
    let r = gw
        .send(&post(RESPONSES_PATH, &[JSON, AUTH], stateful))
        .await;
    assert_eq!(r.status, 422);
    gw.stop().await;
}

#[tokio::test]
async fn local_auth_gates_the_responses_route_before_anything_else() {
    let gw = Gateway::start(true).await;
    let r = gw
        .send(&post(RESPONSES_PATH, &[JSON, AUTH], GOOD.as_bytes()))
        .await;
    assert_eq!(r.status, 401);
    assert!(code(&r).contains("local_auth_required"));
    let wrong = "SYNTH-LOCAL-TOKEN-0123456789-abcdefghijklmnX";
    let r = gw
        .send(&post(
            RESPONSES_PATH,
            &[JSON, AUTH, ("X-Gateway-Local-Token", wrong)],
            GOOD.as_bytes(),
        ))
        .await;
    assert_eq!(r.status, 401);
    assert!(code(&r).contains("local_auth_invalid"));
    assert!(!code(&r).contains(wrong));
    // Authenticated: passes the boundary and reaches the (unconfigured) destination check.
    let r = gw
        .send(&post(
            RESPONSES_PATH,
            &[JSON, AUTH, ("X-Gateway-Local-Token", LOCAL)],
            GOOD.as_bytes(),
        ))
        .await;
    assert_eq!(r.status, 501);
    gw.stop().await;
}
