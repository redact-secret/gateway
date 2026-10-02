//! End-to-end forwarding of ordinary JSON responses (#20; ADR 0017). Compiled only under
//! `cfg(test)`.
//!
//! The whole chat route (admission, strict parse, real pinned-core inspection on the real
//! worker pool, upstream permit, central transport, bounded relay) runs against the
//! loopback fake provider through the test-only destination constructors of #23. There is
//! deliberately no production path to such a fake (tests/destination_policy.rs), so these
//! tests live in the crate. Every credential and secret is synthetic and revoked-looking;
//! nothing leaves loopback.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use redact_secret::Profile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::fake_upstream::{SseFragment, SseFraming};
use super::leak::Markers;
use super::*;
use crate::boundary::Inspection;
use crate::chat_route::{self, ChatRoute, Reject};
use crate::config::ContentPolicy;
use crate::protocol::{self, Protocol};
use crate::telemetry::{Metrics, Stage};
use crate::transport::headers::{self, WIRE_HEADER_NAMES};

const MEM: u32 = 8192;
pub(super) const KEY: &str = "sk-SYNTHETIC-REVOKED-FWD0-0000-NOT-A-KEY";
const ORG: &str = "org-SYNTHETIC-ORG-0001";
pub(super) const TOKEN: &str = "ghp_SYNTHETICREVOKED00000000000000000001";
pub(super) const GOOD: &str =
    r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}"#;

/// A vetted credential for tests outside this module.
pub(super) fn vetted_for_forward(key: &str) -> headers::VettedHeaders {
    let mut m = reqwest::header::HeaderMap::new();
    m.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).unwrap(),
    );
    headers::vet_inbound(&m).unwrap()
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

#[derive(Clone, Copy)]
pub(super) struct Caps {
    pub(super) receipt: u32,
    pub(super) inspection: u32,
    pub(super) upstream: u32,
    pub(super) stream: u32,
}

impl Caps {
    pub(super) const ROOMY: Self = Self {
        receipt: 8,
        inspection: 2,
        upstream: 8,
        stream: 1,
    };
}

pub(super) struct Out {
    pub(super) status: u16,
    pub(super) headers: reqwest::header::HeaderMap,
    pub(super) body: Vec<u8>,
}

pub(super) fn assert_gateway_error(out: &Out, status: u16, code: &str) {
    assert_eq!(out.status, status, "status");
    assert_eq!(
        String::from_utf8_lossy(&out.body),
        format!(r#"{{"error":{{"code":"{code}"}}}}"#)
    );
}

pub(super) struct Rig {
    pub(super) route: Arc<ChatRoute>,
    pub(super) admission: Arc<Admission>,
    pub(super) inspection: Arc<Inspection>,
    pub(super) fake: FakeUpstream,
    pub(super) metrics: Arc<Metrics>,
    pub(super) caps: Caps,
    pub(super) limits: RequestLimits,
}

impl Rig {
    pub(super) async fn new(behavior: Behavior) -> Self {
        Self::with(behavior, RequestLimits::provisional(), Caps::ROOMY).await
    }

    pub(super) async fn with(behavior: Behavior, limits: RequestLimits, caps: Caps) -> Self {
        let fake = FakeUpstream::start(behavior).await;
        let upstream = http_upstream_with(fake.addr(), limits);
        Self::over(fake, upstream, limits, caps)
    }

    pub(super) fn over(
        fake: FakeUpstream,
        upstream: Upstream,
        limits: RequestLimits,
        caps: Caps,
    ) -> Self {
        let metrics = Arc::new(Metrics::new());
        let upstream = Upstream {
            metrics: Some(Arc::clone(&metrics)),
            ..upstream
        };
        let plan = CapacityPlan::new(
            nz(caps.receipt),
            nz(MEM),
            nz(caps.inspection),
            nz(caps.upstream),
            nz(caps.stream),
        );
        let admission = Arc::new(Admission::new(&plan));
        let inspection = Arc::new(
            Inspection::start(
                Arc::clone(&admission),
                &ContentPolicy::new(Profile::Full),
                &limits,
                &plan,
            )
            .unwrap()
            .with_metrics(Arc::clone(&metrics)),
        );
        let route = Arc::new(
            ChatRoute::new(Arc::clone(&admission), limits, RouteId::new(ROUTE))
                .with_inspection(Arc::clone(&inspection))
                .with_upstream(Arc::new(upstream))
                .with_metrics(Arc::clone(&metrics)),
        );
        Self {
            route,
            admission,
            inspection,
            fake,
            metrics,
            caps,
            limits,
        }
    }

    pub(super) async fn post(&self, request: Request) -> Out {
        let response = self.route.handle(request).await;
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, 1 << 24).await.unwrap();
        Out {
            status: parts.status.as_u16(),
            headers: parts.headers,
            body: body.to_vec(),
        }
    }

    pub(super) async fn post_good(&self) -> Out {
        self.post(request(GOOD, KEY, &[])).await
    }

    /// Wait until every capacity class is back at its baseline.
    pub(super) async fn settle(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let memory = self.admission.try_reserve_memory(MEM);
            let receipts: Vec<_> = (0..self.caps.receipt)
                .map(|_| self.admission.try_receipt())
                .collect();
            let inspections: Vec<_> = (0..self.caps.inspection)
                .map(|_| self.admission.try_inspection())
                .collect();
            let upstreams: Vec<_> = (0..self.caps.upstream)
                .map(|_| self.admission.try_upstream())
                .collect();
            let stream: Vec<_> = (0..self.caps.stream)
                .map(|_| self.admission.try_stream())
                .collect();
            if memory.is_ok()
                && receipts.iter().all(Result::is_ok)
                && inspections.iter().all(Result::is_ok)
                && upstreams.iter().all(Result::is_ok)
                && stream.iter().all(Result::is_ok)
            {
                return;
            }
            drop((memory, receipts, inspections, upstreams, stream));
            assert!(
                tokio::time::Instant::now() < deadline,
                "capacity was not returned"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Exactly one upstream connection and request, and nothing more arrives later.
    pub(super) async fn assert_single_attempt(&self) {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let tally = self.fake.tally();
        assert_eq!(tally.connections, 1, "one connection");
        assert_eq!(tally.calls, 1, "one request");
        assert_eq!(self.metrics.upstream_attempts(), 1, "one send attempt");
    }
}

pub(super) fn request(body: &str, key: &str, extra: &[(&str, &str)]) -> Request {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("content-length", body.len().to_string());
    if !key.is_empty() {
        b = b.header("authorization", format!("Bearer {key}"));
    }
    for (n, v) in extra {
        b = b.header(*n, *v);
    }
    b.body(Body::from(body.to_owned())).unwrap()
}

pub(super) fn chat_body(content: &str) -> String {
    format!(r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"{content}"}}]}}"#)
}

pub(super) fn raw_post(body: &str, key: &str) -> Vec<u8> {
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\nAuthorization: Bearer {key}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn short_limits() -> RequestLimits {
    RequestLimits {
        upstream_header_ms: 250,
        upstream_total_ms: 600,
        ..RequestLimits::provisional()
    }
}

// ----------------------------------------------------------------- what upstream receives

#[tokio::test]
async fn upstream_receives_exactly_the_sanitized_body_and_only_vetted_headers() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let original = chat_body(&format!("my token is {TOKEN} thanks"));
    let out = rig
        .post(request(
            &original,
            KEY,
            &[
                ("host", "evil.example"),
                ("x-forwarded-for", "203.0.113.9"),
                ("forwarded", "for=203.0.113.9"),
                ("cookie", "session=SYNTH-COOKIE"),
                ("x-stainless-lang", "js"),
                ("user-agent", "SynthSDK/9.9"),
                ("openai-organization", ORG),
                ("x-gateway-local-token", "SYNTH-LOCAL"),
            ],
        ))
        .await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body, br#"{"synthetic":"fake-upstream-response"}"#);

    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.path, "/v1/chat/completions");
    assert!(call.body_complete);

    // The body is exactly what the real inspection pipeline produces for this input.
    let received = rig
        .admission
        .begin_body_receipt(original.len(), &rig.limits)
        .await
        .unwrap()
        .complete(original.clone().into_bytes())
        .unwrap();
    let validated =
        protocol::validate_with(received, Protocol::ChatCompletionsText, &rig.limits).unwrap();
    let expected = rig
        .inspection
        .inspect_and_approve(validated, RouteId::new(ROUTE))
        .await
        .unwrap();
    assert_eq!(call.body, expected.body(), "exactly the sanitized body");
    drop(expected); // its memory reservation must not mask a leak below
    let text = String::from_utf8(call.body.clone()).unwrap();
    assert!(!text.contains(TOKEN), "planted secret reached upstream");
    assert!(!text.contains("ghp_"), "secret prefix reached upstream");
    assert!(text.contains("my token is"), "surrounding text survives");
    assert_ne!(call.body, original.as_bytes());
    assert!(
        text.contains("my token is <SECRET_1> thanks"),
        "the redaction placeholder replaced the secret"
    );

    // Only vetted/regenerated headers: the credential, reviewed metadata, and gateway values.
    assert_eq!(
        call.header("authorization"),
        Some(&*format!("Bearer {KEY}"))
    );
    assert_eq!(call.header("openai-organization"), Some(ORG));
    let allowed: BTreeSet<&str> = WIRE_HEADER_NAMES.iter().copied().chain(["host"]).collect();
    for name in call.header_names() {
        assert!(
            allowed.contains(name.to_ascii_lowercase().as_str()),
            "unexpected upstream header {name}"
        );
    }
    assert_eq!(
        call.header("host"),
        Some(&*rig.fake.addr().to_string()),
        "Host comes from the reviewed destination, not the caller"
    );
    assert_eq!(
        call.header("content-length"),
        Some(&*call.body.len().to_string()),
        "regenerated Content-Length equals the sealed bytes"
    );
    assert_eq!(call.header("accept-encoding"), Some("identity"));
    assert!(
        call.header("user-agent")
            .is_some_and(|v| v.starts_with("redact-secret-gateway/"))
    );
    rig.settle().await;
}

// -------------------------------------------------------------------- provider responses

#[tokio::test]
async fn provider_error_responses_are_relayed_as_provider_responses() {
    for (status, body) in [
        (
            400_u16,
            r#"{"error":{"message":"synthetic bad request","type":"invalid_request_error"}}"#,
        ),
        (
            401,
            r#"{"error":{"message":"synthetic bad key","code":"invalid_api_key"}}"#,
        ),
        (
            429,
            r#"{"error":{"message":"synthetic rate limit","code":"rate_limit_exceeded"}}"#,
        ),
        (500, r#"{"error":{"message":"synthetic provider failure"}}"#),
        (503, r#"{"error":{"message":"synthetic overloaded"}}"#),
    ] {
        let rig = Rig::new(Behavior::Json {
            status,
            body: body.as_bytes().to_vec(),
        })
        .await;
        let out = rig.post_good().await;
        assert_eq!(out.status, status);
        assert_eq!(out.body, body.as_bytes(), "provider body relayed unchanged");
        assert_eq!(
            out.headers.get("content-type").map(|v| v.as_bytes()),
            Some(&b"application/json"[..])
        );
        rig.assert_single_attempt().await;
        rig.settle().await;
    }
}

#[tokio::test]
async fn only_allowlisted_provider_headers_are_relayed() {
    let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Request-Id: req_synth\r\nSet-Cookie: s=SYNTH\r\nServer: synth\r\nLocation: https://evil.example/\r\nWWW-Authenticate: Bearer\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let rig = Rig::new(Behavior::Malformed(raw.as_bytes().to_vec())).await;
    let out = rig.post_good().await;
    assert_eq!(out.status, 200);
    assert_eq!(out.body, b"{}");
    assert!(out.headers.contains_key("content-type"));
    assert!(out.headers.contains_key("x-request-id"));
    for dropped in ["set-cookie", "server", "location", "www-authenticate"] {
        assert!(!out.headers.contains_key(dropped), "{dropped} was relayed");
    }
    rig.settle().await;
}

// -------------------------------------------------------------------- pre-forward failures

#[tokio::test]
async fn every_pre_forward_failure_delivers_zero_bytes_upstream() {
    let limits = RequestLimits {
        admission_wait_ms: 0,
        ..RequestLimits::provisional()
    };
    let caps = Caps {
        receipt: 1,
        inspection: 1,
        upstream: 1,
        stream: 1,
    };
    let rig = Rig::with(Behavior::ok_json(), limits, caps).await;

    // Validation, framing, and credential rejections.
    for (req, status, code) in [
        (request(GOOD, "", &[]), 401, "missing_credential"),
        (request("{", KEY, &[]), 400, "malformed_input"),
        (
            request(r#"{"model":"m","messages":[],"tools":[]}"#, KEY, &[]),
            422,
            "unsupported_input",
        ),
    ] {
        let out = rig.post(req).await;
        assert_gateway_error(&out, status, code);
        rig.fake.assert_nothing_sent();
    }

    // Receipt capacity held elsewhere: admission overload.
    let held = rig.admission.try_receipt().unwrap();
    let out = rig.post_good().await;
    assert_gateway_error(&out, 503, "overload");
    assert_eq!(out.headers.get("retry-after").unwrap(), "1");
    rig.fake.assert_nothing_sent();
    drop(held);

    // Inspection capacity held elsewhere: inspection overload.
    let held = rig.admission.try_inspection().unwrap();
    let out = rig.post_good().await;
    assert_gateway_error(&out, 503, "overload");
    rig.fake.assert_nothing_sent();
    drop(held);

    // Upstream capacity held elsewhere: overload after inspection, still zero bytes.
    let held = rig.admission.try_upstream().unwrap();
    let out = rig.post_good().await;
    assert_gateway_error(&out, 503, "overload");
    assert_eq!(out.headers.get("retry-after").unwrap(), "1");
    rig.fake.assert_nothing_sent();
    drop(held);

    rig.settle().await;
    // Control: with everything free the same request is forwarded.
    assert_eq!(rig.post_good().await.status, 200);
    assert_eq!(rig.fake.tally().calls, 1);
    rig.settle().await;
}

#[tokio::test]
async fn a_cancelled_waiter_never_starts_an_upstream_request() {
    let rig = Rig::new(Behavior::ok_json()).await;
    // Poll the request exactly once, so it suspends waiting on the inspection worker, then
    // drop it: exactly what hyper does when the caller goes away.
    let mut fut = Box::pin(rig.route.handle(request(GOOD, KEY, &[])));
    let finished = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(fut.as_mut(), cx).is_ready())
    })
    .await;
    assert!(!finished, "the request must still have been in flight");
    drop(fut);
    // Give the (discarded) inspection job time to finish; its result must go nowhere.
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

#[tokio::test]
async fn no_configured_upstream_route_is_a_local_501_with_no_send() {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let none = Upstream::new(None).unwrap();
    let rig = Rig::over(fake, none, RequestLimits::provisional(), Caps::ROOMY);
    let out = rig.post_good().await;
    assert_gateway_error(&out, 501, "not_implemented");
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

// ------------------------------------------------------------------- post-forward failures

#[tokio::test]
async fn post_forward_failures_terminate_safely_with_one_attempt_and_no_fallback() {
    const PROVIDER_MARKER: &str = "SYNTH-PROVIDER-BODY-MARKER-3C7";
    let big_header = format!(
        "HTTP/1.1 200 OK\r\nX-Big: {}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
        "a".repeat(1000)
    );
    let big_body = vec![b'x'; 5000];
    let fragment = |n: usize| SseFragment::now(vec![b'y'; n]);
    let cases: Vec<(&str, Behavior, RequestLimits, u16, &str)> = vec![
        (
            "disconnect before any response byte",
            Behavior::DisconnectBeforeResponse,
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "disconnect mid body",
            Behavior::DisconnectMidBody {
                declared_len: 100,
                sent: PROVIDER_MARKER.as_bytes().to_vec(),
            },
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "malformed response",
            Behavior::Malformed(format!("NOT HTTP {PROVIDER_MARKER}\r\n\r\n").into_bytes()),
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "content coding the gateway cannot relay",
            Behavior::Malformed(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec(),
            ),
            RequestLimits::provisional(),
            502,
            "upstream_invalid_response",
        ),
        (
            "slow response header",
            Behavior::Slow {
                delay: Duration::from_secs(5),
                then: Box::new(Behavior::ok_json()),
            },
            short_limits(),
            504,
            "upstream_timeout",
        ),
        (
            "total deadline during the body",
            Behavior::Sse {
                fragments: vec![fragment(10), SseFragment::after(Duration::from_secs(5), "late")],
                framing: SseFraming::CloseDelimited,
                finish: true,
            },
            short_limits(),
            504,
            "upstream_timeout",
        ),
        (
            "declared body over the cap",
            Behavior::Json {
                status: 200,
                body: big_body,
            },
            RequestLimits {
                max_response_body_bytes: 1024,
                ..RequestLimits::provisional()
            },
            502,
            "upstream_response_too_large",
        ),
        (
            "undeclared body over the cap",
            Behavior::Sse {
                fragments: vec![fragment(2048), fragment(2048), fragment(2048)],
                framing: SseFraming::CloseDelimited,
                finish: true,
            },
            RequestLimits {
                max_response_body_bytes: 3000,
                ..RequestLimits::provisional()
            },
            502,
            "upstream_response_too_large",
        ),
        (
            "header block over the cap",
            Behavior::Malformed(big_header.into_bytes()),
            RequestLimits {
                max_response_header_bytes: 256,
                ..RequestLimits::provisional()
            },
            502,
            "upstream_response_too_large",
        ),
    ];
    let leaks = Markers::empty()
        .with("provider-body", PROVIDER_MARKER)
        .with("key", KEY)
        .with("token", TOKEN);
    for (name, behavior, limits, status, code) in cases {
        let rig = Rig::with(behavior, limits, Caps::ROOMY).await;
        let out = rig
            .post(request(&chat_body(&format!("x {TOKEN}")), KEY, &[]))
            .await;
        assert_eq!(out.status, status, "{name}");
        assert_gateway_error(&out, status, code);
        // Error bodies and headers carry no provider, payload, or credential content.
        leaks.assert_clean(name, &out.body);
        for (n, v) in &out.headers {
            leaks.assert_clean(name, n.as_str().as_bytes());
            leaks.assert_clean(name, v.as_bytes());
        }
        // The one request that was sent carried the sanitized body; nothing was retried
        // and no raw fallback followed.
        rig.assert_single_attempt().await;
        let calls = rig.fake.calls();
        assert!(
            !String::from_utf8_lossy(&calls[0].body).contains(TOKEN),
            "{name}"
        );
        rig.settle().await;
    }
}

#[tokio::test]
async fn connect_failure_is_unavailable_and_not_retried() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let up = http_upstream_with(closed, RequestLimits::provisional());
    let rig = Rig::over(fake, up, RequestLimits::provisional(), Caps::ROOMY);
    let out = rig.post_good().await;
    assert_gateway_error(&out, 502, "upstream_unavailable");
    assert_eq!(rig.metrics.upstream_attempts(), 1);
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

#[tokio::test]
async fn tls_failure_is_a_distinct_safe_code_and_sends_no_request() {
    let (cert, key) = self_signed(TEST_HOST);
    let tls = TlsFake::start(cert, key).await;
    let up = untrusted_tls_upstream(&tls);
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let rig = Rig::over(fake, up, RequestLimits::provisional(), Caps::ROOMY);
    let out = rig.post_good().await;
    assert_gateway_error(&out, 502, "upstream_tls_failure");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tls.tcp(), 1, "one connection attempt, no retry");
    assert_eq!(
        tls.served(),
        0,
        "no request bytes crossed an unverified session"
    );
    rig.settle().await;
}

// ------------------------------------------------------------- ownership and cancellation

pub(super) fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

async fn serve_route(route: Arc<ChatRoute>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = chat_route::mount(axum::Router::new(), route);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, task)
}

#[tokio::test]
async fn downstream_disconnect_cancels_the_upstream_exchange_and_releases_everything() {
    let rig = Rig::new(Behavior::Slow {
        delay: Duration::from_millis(1500),
        then: Box::new(Behavior::ok_json()),
    })
    .await;
    let (addr, server) = serve_route(Arc::clone(&rig.route)).await;
    let baseline = alive_tasks();

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(&raw_post(GOOD, KEY)).await.unwrap();
    assert!(
        rig.fake.wait_for_calls(1, Duration::from_secs(5)).await,
        "the request reached the fake"
    );
    // While in flight the request owns an upstream permit.
    let probe: Vec<_> = (0..rig.caps.upstream)
        .map(|_| rig.admission.try_upstream())
        .collect();
    assert!(
        probe.iter().any(Result::is_err),
        "upstream permit held in flight"
    );
    drop(probe);
    drop(client); // downstream disconnect
    rig.settle().await;

    // Tasks owned by the connection and the client exchange are gone (the fake's own
    // connection task ends when its scripted delay ends).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while alive_tasks() > baseline {
        assert!(tokio::time::Instant::now() < deadline, "leaked tasks");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.fake.tally().calls, 1, "no retry after the disconnect");
    server.abort();
}

#[tokio::test]
async fn shutdown_cancels_in_flight_requests_and_returns_every_permit() {
    let rig = Rig::new(Behavior::Slow {
        delay: Duration::from_secs(10),
        then: Box::new(Behavior::ok_json()),
    })
    .await;
    let route = Arc::clone(&rig.route);
    let (cancelled, ()) = tokio::join!(rig.post_good(), async {
        assert!(rig.fake.wait_for_calls(1, Duration::from_secs(5)).await);
        route.cancel_in_flight();
    });
    assert_gateway_error(&cancelled, 503, "not_ready");
    rig.settle().await;
    assert_eq!(rig.fake.tally().calls, 1);
}

#[tokio::test]
async fn graceful_shutdown_is_bounded_by_the_drain_deadline() {
    let rig = Rig::new(Behavior::Slow {
        delay: Duration::from_secs(20),
        then: Box::new(Behavior::ok_json()),
    })
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

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(&raw_post(GOOD, KEY)).await.unwrap();
    assert!(rig.fake.wait_for_calls(1, Duration::from_secs(5)).await);
    let asked = tokio::time::Instant::now();
    stop.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("shutdown must not wait for the 20 s upstream")
        .unwrap();
    assert!(result.is_ok());
    let took = asked.elapsed();
    assert!(
        took >= Duration::from_millis(150),
        "drain was not honoured: {took:?}"
    );
    assert!(
        took < Duration::from_secs(4),
        "drain was not bounded: {took:?}"
    );

    // The in-flight caller was told the gateway is going away; nothing was retried.
    let mut seen = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut seen)).await;
    let seen = String::from_utf8_lossy(&seen);
    assert!(seen.starts_with("HTTP/1.1 503"), "{seen}");
    assert!(seen.contains("not_ready"));
    rig.settle().await;
    assert_eq!(rig.fake.tally().calls, 1);
}

#[tokio::test]
async fn overload_is_immediate_bounded_and_returns_all_capacity() {
    let caps = Caps {
        receipt: 8,
        inspection: 2,
        upstream: 1,
        stream: 1,
    };
    let rig = Rig::with(
        Behavior::Slow {
            delay: Duration::from_millis(400),
            then: Box::new(Behavior::ok_json()),
        },
        RequestLimits::provisional(),
        caps,
    )
    .await;
    let (a, b) = tokio::join!(rig.post_good(), async {
        assert!(rig.fake.wait_for_calls(1, Duration::from_secs(5)).await);
        rig.post_good().await
    });
    assert_eq!(a.status, 200);
    assert_gateway_error(&b, 503, "overload");
    assert_eq!(
        rig.fake.tally().calls,
        1,
        "the overloaded request sent nothing"
    );
    rig.settle().await;
}

#[tokio::test]
async fn concurrent_requests_with_different_keys_do_not_cross() {
    const N: usize = 6;
    let rig = Rig::with(
        Behavior::Slow {
            delay: Duration::from_millis(60),
            then: Box::new(Behavior::ok_json()),
        },
        RequestLimits::provisional(),
        Caps {
            receipt: 8,
            inspection: 8,
            upstream: 8,
            stream: 1,
        },
    )
    .await;
    let key = |i: usize| format!("sk-SYNTHETIC-REVOKED-K{i}00-0000-NOT-A-KEY");
    let mut set = tokio::task::JoinSet::new();
    for i in 0..N {
        let route = Arc::clone(&rig.route);
        let req = request(&chat_body(&format!("payload-{i}")), &key(i), &[]);
        set.spawn(async move { route.handle(req).await.status().as_u16() });
    }
    while let Some(status) = set.join_next().await {
        assert_eq!(status.unwrap(), 200);
    }
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), N);
    let mut seen = BTreeSet::new();
    for call in &calls {
        let body = String::from_utf8_lossy(&call.body).into_owned();
        let i: usize = (0..N)
            .find(|i| body.contains(&format!("payload-{i}\"")))
            .expect("payload index");
        assert_eq!(
            call.header("authorization"),
            Some(&*format!("Bearer {}", key(i))),
            "credential crossed requests"
        );
        seen.insert(i);
    }
    assert_eq!(seen.len(), N);
    // Keys are not visible in shared state or diagnostics.
    let mut markers = Markers::empty();
    for i in 0..N {
        let k = key(i);
        markers = markers.with("synthetic-key", &k);
    }
    markers.assert_clean_debug("route", &rig.route);
    markers.assert_clean_debug("admission", rig.route.admission());
    markers.assert_clean_debug("inspection", &rig.inspection);
    rig.settle().await;
}

// -------------------------------------------------------------- hot path, telemetry, leaks

#[tokio::test]
async fn client_construction_and_config_parsing_are_absent_from_the_request_path() {
    let rig = Rig::new(Behavior::ok_json()).await;
    // Everything is built above; now run requests and count.
    let clients = client_builds();
    let parses = crate::config::parses_on_this_thread();
    for _ in 0..4 {
        assert_eq!(rig.post_good().await.status, 200);
    }
    assert_eq!(
        client_builds(),
        clients,
        "an HTTP client was built per request"
    );
    assert_eq!(
        crate::config::parses_on_this_thread(),
        parses,
        "configuration was parsed per request"
    );
    assert_eq!(rig.fake.tally().calls, 4);
    rig.settle().await;
}

#[tokio::test]
async fn stage_timings_are_recorded_without_labels() {
    let rig = Rig::new(Behavior::ok_json()).await;
    assert_eq!(rig.post_good().await.status, 200);
    for stage in [
        Stage::AdmissionWait,
        Stage::Parse,
        Stage::Inspection,
        Stage::Serialization,
        Stage::UpstreamFirstResponse,
        Stage::UpstreamTotal,
    ] {
        assert_eq!(rig.metrics.stage(stage).count, 1, "{stage:?}");
    }
    assert_eq!(rig.metrics.upstream_attempts(), 1);
    // The counters hold numbers only; their Debug output has no request text.
    let rendered = format!("{:?}", rig.metrics);
    assert!(!rendered.contains("hello") && !rendered.contains(KEY));
    rig.settle().await;
}

#[tokio::test]
async fn diagnostics_errors_and_debug_output_do_not_leak_payload_or_credentials() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let markers = Markers::standard()
        .with("key", KEY)
        .with("token", TOKEN)
        .with("org", ORG);

    // An admitted request carries the credential request-locally; its Debug shows none.
    let body = chat_body(&format!("see {TOKEN} and {}", super::leak::BODY_MARKER));
    let mut admitted = rig
        .route
        .admit(request(&body, KEY, &[("openai-organization", ORG)]))
        .await
        .unwrap();
    markers.assert_clean_debug("admitted", &admitted);
    let vetted = admitted.take_headers().unwrap();
    markers.assert_clean_debug("vetted", &vetted);
    drop(vetted);
    drop(admitted);

    // Every error the gateway can generate for a forwarded request, rendered.
    for error in [
        TransportError::ClientInit,
        TransportError::UnknownRoute,
        TransportError::Timeout,
        TransportError::Connect,
        TransportError::Tls,
        TransportError::InvalidResponse,
        TransportError::ResponseTooLarge,
    ] {
        markers.assert_clean_fmt("transport error", &error);
        let reject = Reject::Transport(error);
        markers.assert_clean_debug("reject", &reject);
        let (status, code) = reject.status_and_code();
        assert!(status.is_client_error() || status.is_server_error());
        assert!(code.as_str().is_ascii());
    }

    // A relayed response's Debug shows status and length only.
    let out = rig.post(request(&body, KEY, &[])).await;
    assert_eq!(out.status, 200);
    markers.assert_clean(
        "gateway response headers",
        format!("{:?}", out.headers).as_bytes(),
    );
    markers.assert_clean_debug("route", &rig.route);
    markers.assert_clean_debug("metrics", &rig.metrics);
    rig.settle().await;
}

#[tokio::test]
async fn upstream_response_debug_never_prints_the_provider_body() {
    let secret = "SYNTH-PROVIDER-SECRET-ECHO-9Z";
    let rig = Rig::new(Behavior::Json {
        status: 200,
        body: format!(r#"{{"echo":"{secret}"}}"#).into_bytes(),
    })
    .await;
    let adm = Admission::new(&CapacityPlan::new(nz(1), nz(1), nz(1), nz(1), nz(1)));
    let v = ValidatedRequest::for_test(
        adm.try_reserve_memory(1).unwrap(),
        adm.try_receipt().unwrap(),
    );
    let sealed = boundary::approve(
        v,
        CompleteInspection::for_test(GOOD.as_bytes().to_vec()),
        route(),
    )
    .unwrap();
    let up = http_upstream(rig.fake.addr());
    let response = up
        .forward(sealed, vetted_for_forward(KEY), adm.try_upstream().unwrap())
        .await
        .unwrap();
    let rendered = format!("{response:?}");
    assert!(!rendered.contains(secret));
    assert!(rendered.contains("body_len"));
    // The permit stays with the response body until it is dropped.
    assert!(adm.try_upstream().is_err());
    let (_, _, body) = response.into_parts();
    assert!(!format!("{body:?}").contains(secret));
    assert!(adm.try_upstream().is_err());
    drop(body);
    assert!(adm.try_upstream().is_ok());
}
