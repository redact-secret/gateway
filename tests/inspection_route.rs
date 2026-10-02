//! `POST /v1/chat/completions` with core inspection wired in, over real HTTP on loopback
//! (issue #19). A fake upstream runs for every case and must observe zero connections and
//! zero body bytes: inspection and approval exist, forwarding (#20) does not, so an approved
//! request ends in a local `501 not_implemented` and every rejection is a fixed safe code.
//! Also covers that readiness depends on successful core initialization. All data is
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use redact_secret::Profile;
use redact_secret_gateway::admission::CapacityPlan;
use redact_secret_gateway::chat_route::{CHAT_COMPLETIONS_PATH, ChatRoute};
use redact_secret_gateway::config::{
    self, ContentPolicy, DeploymentAuthority, ListenerAuthority, ResourcePolicy, RuntimePlan,
};
use redact_secret_gateway::server::{self, Services, StartupError};
use support::fake_upstream::{Behavior, FakeUpstream};
use support::leak::Markers;
use support::raw_http::{Response, parse_response, post};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

fn token(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

fn config_json(content: &str) -> String {
    format!(
        r#"{{"schema_version":1,
            "deployment":{{"listener":{{"address":"127.0.0.1:0"}}}},
            "content":{content},
            "resources":{{"capacity":{{"receipt":4,"memory_units":8192,
                "inspection":2,"upstream":1,"stream":1}}}}}}"#
    )
}

struct Gateway {
    addr: SocketAddr,
    chat: Arc<ChatRoute>,
    upstream: FakeUpstream,
    markers: Markers,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

impl Gateway {
    async fn start(content: &str) -> Self {
        let plan = Arc::new(config::parse(config_json(content).as_bytes()).unwrap());
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
            upstream: FakeUpstream::start(Behavior::ok_json()).await,
            markers: Markers::standard(),
            stop: Some(stop),
            task,
        }
    }

    async fn post_json(&self, body: &str) -> Response {
        let request = post(
            CHAT_COMPLETIONS_PATH,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        );
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(&request).await.unwrap();
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Err(_) => panic!("timed out waiting for a response"),
            }
        }
        // Gateway text never echoes request content, secrets included.
        self.markers.assert_clean("response", &out);
        assert!(!String::from_utf8_lossy(&out).contains("SYNTHETICREVOKED"));
        parse_response(&out).expect("a complete HTTP response")
    }

    async fn assert_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let adm = self.chat.admission();
            let memory = adm.try_reserve_memory(8192);
            let permits: Vec<_> = (0..2).map(|_| adm.try_inspection()).collect();
            let receipts: Vec<_> = (0..4).map(|_| adm.try_receipt()).collect();
            if memory.is_ok()
                && permits.iter().all(Result::is_ok)
                && receipts.iter().all(Result::is_ok)
            {
                break;
            }
            assert!(Instant::now() < deadline, "capacity was not returned");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // No request reaches upstream, whatever its inspection outcome.
        self.upstream.assert_nothing_sent();
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.task).await;
    }
}

fn expect(r: &Response, status: u16, code: &str) {
    let body = String::from_utf8(r.body.clone()).unwrap();
    assert_eq!(r.status, status, "{body}");
    assert_eq!(body, format!(r#"{{"error":{{"code":"{code}"}}}}"#));
}

fn body_with(text: &str) -> String {
    serde_json::json!({"model":"gpt-4o-mini","messages":[{"role":"user","content":text}]})
        .to_string()
}

#[tokio::test]
async fn an_inspected_and_approved_request_still_ends_locally_and_sends_nothing() {
    let gw = Gateway::start(r#"{"profile":"full"}"#).await;
    for text in ["hello", &format!("k={}", token(1)), "안녕하세요"] {
        let r = gw.post_json(&body_with(text)).await;
        // Forwarding is #20: approval produces a sealed request that is dropped locally.
        expect(&r, 501, "not_implemented");
        gw.assert_idle().await;
    }
    gw.shutdown().await;
}

#[tokio::test]
async fn block_and_default_warn_reject_with_safe_codes_and_send_nothing() {
    let gw = Gateway::start(r#"{"profile":"full"}"#).await;
    let pem = "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\n-----END PRIVATE KEY-----";
    let r = gw.post_json(&body_with(pem)).await;
    expect(&r, 422, "unsupported_input");
    gw.assert_idle().await;
    // `on_warn` defaults to reject.
    let r = gw.post_json(&body_with("password=hunter2xyz")).await;
    expect(&r, 422, "unsupported_input");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn on_warn_forward_is_an_explicit_operator_choice() {
    let gw = Gateway::start(r#"{"profile":"full","on_warn":"forward"}"#).await;
    let r = gw.post_json(&body_with("password=hunter2xyz")).await;
    expect(&r, 501, "not_implemented");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn finding_limit_rejects_the_whole_request() {
    let gw = Gateway::start(r#"{"profile":"full","max_findings":1}"#).await;
    let r = gw
        .post_json(&body_with(&format!("{} {}", token(2), token(3))))
        .await;
    expect(&r, 413, "limit_exceeded");
    gw.assert_idle().await;
    // One finding is within the bound.
    let r = gw.post_json(&body_with(&token(4))).await;
    expect(&r, 501, "not_implemented");
    gw.assert_idle().await;
    gw.shutdown().await;
}

#[tokio::test]
async fn secret_in_model_rejects() {
    let gw = Gateway::start(r#"{"profile":"full"}"#).await;
    let body = serde_json::json!({
        "model": token(5),
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let r = gw.post_json(&body).await;
    expect(&r, 422, "unsupported_input");
    gw.assert_idle().await;
    gw.shutdown().await;
}

fn plan_with(content: ContentPolicy) -> Arc<RuntimePlan> {
    let nz = |n| NonZeroU32::new(n).unwrap();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    Arc::new(RuntimePlan::new(
        DeploymentAuthority::new(ListenerAuthority::new(addr, false).unwrap()),
        content,
        ResourcePolicy::new(CapacityPlan::new(nz(1), nz(8192), nz(1), nz(1), nz(1))),
    ))
}

#[tokio::test]
async fn readiness_depends_on_successful_core_initialization() {
    // A PII selector the pinned core cannot activate fails core initialization at startup:
    // no listener is bound and nothing is ever ready.
    let bad = ContentPolicy::new(Profile::Full).with_pii(vec!["pii:kr".to_owned()]);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let result = server::bind(plan_with(bad), move |plan| {
        seen.fetch_add(1, Ordering::SeqCst);
        Services::init(plan)
    })
    .await;
    assert_eq!(result.unwrap_err(), StartupError::Init);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // A supported profile initializes the workers and the server reports ready.
    let ok = ContentPolicy::new(Profile::Common);
    let bound = server::bind(plan_with(ok), Services::init).await.unwrap();
    let health = bound.health();
    assert!(
        !health.is_ready(),
        "not ready until the server is accepting"
    );
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(bound.serve(async move {
        let _ = rx.await;
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !health.is_ready() {
        assert!(Instant::now() < deadline, "never became ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = stop.send(());
    let _ = task.await;
}
