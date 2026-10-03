//! The Responses endpoint beside the Chat endpoint (#86; ADR 0031, ADR 0013). Compiled only
//! under `cfg(test)`.
//!
//! Both endpoints are the same [`EndpointRoute`] type over one shared admission, inspection,
//! and transport, each bound to its own protocol and fixed route id. The Chat route is
//! pointed at one loopback fake provider and the Responses route at another, so any request
//! that reaches the wrong origin is visible. "Rejected" means neither fake saw a connection,
//! a call, or a body byte. Every value is synthetic.

use axum::body::Body;
use axum::extract::Request;
use redact_secret::Profile;

use super::fake_upstream::{Behavior, FakeUpstream};
use super::forward_tests::{GOOD, KEY, TOKEN, assert_gateway_error, request};
use super::leak::Markers;
use super::*;
use crate::boundary::Inspection;
use crate::chat_route::{ChatRoute, EndpointRoute, ResponsesRoute};
use crate::config::ContentPolicy;
use crate::protocol::Protocol;
use crate::transport::local_auth::{LocalAuth, LocalToken};

const CHAT_ROUTE_ID: &str = "test.chat";
const RESPONSES_ROUTE_ID: &str = "test.responses";
const LOCAL: &str = "SYNTH-LOCAL-TOKEN-0123456789-abcdefghijklmno";
const GOOD_RESPONSES: &str = r#"{"model":"gpt-4o-mini","input":"hello","store":false}"#;

struct Pair {
    chat: Arc<ChatRoute>,
    responses: Arc<ResponsesRoute>,
    chat_fake: FakeUpstream,
    responses_fake: FakeUpstream,
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

impl Pair {
    async fn new(auth: LocalAuth) -> Self {
        let limits = RequestLimits::provisional();
        let chat_fake = FakeUpstream::start(Behavior::ok_json()).await;
        let responses_fake = FakeUpstream::start(Behavior::ok_json()).await;
        let mut upstream = http_upstream_with(chat_fake.addr(), limits);
        let chat_binding = RouteBinding::for_test(
            RouteId::new(CHAT_ROUTE_ID),
            upstream.routes[0].destination().clone(),
        );
        let other = http_upstream_with(responses_fake.addr(), limits);
        let responses_destination = Destination::for_test(
            other.routes[0].destination().origin().clone(),
            "/v1/responses",
        )
        .expect("destination");
        upstream.routes = vec![
            chat_binding,
            RouteBinding::for_test(RouteId::new(RESPONSES_ROUTE_ID), responses_destination),
        ]
        .into_boxed_slice();
        let plan = CapacityPlan::new(nz(8), nz(8192), nz(2), nz(8), nz(1));
        let admission = Arc::new(Admission::new(&plan));
        let inspection = Arc::new(
            Inspection::start(
                Arc::clone(&admission),
                &ContentPolicy::new(Profile::Full),
                &limits,
                &plan,
            )
            .unwrap(),
        );
        let upstream = Arc::new(upstream);
        let build = |protocol, id: &str| {
            Arc::new(
                EndpointRoute::for_protocol(
                    protocol,
                    Arc::clone(&admission),
                    limits,
                    RouteId::new(id),
                )
                .with_inspection(Arc::clone(&inspection))
                .with_upstream(Arc::clone(&upstream))
                .with_local_auth(auth.clone()),
            )
        };
        Self {
            chat: build(Protocol::ChatCompletionsText, CHAT_ROUTE_ID),
            responses: build(Protocol::ResponsesText, RESPONSES_ROUTE_ID),
            chat_fake,
            responses_fake,
        }
    }

    fn assert_nothing_sent(&self) {
        self.chat_fake.assert_nothing_sent();
        self.responses_fake.assert_nothing_sent();
    }
}

async fn send(route: &EndpointRoute, request: Request) -> (u16, Vec<u8>) {
    let response = route.handle(request).await;
    let (parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, 1 << 24).await.unwrap();
    (parts.status.as_u16(), body.to_vec())
}

fn at(path: &str, body: &str, key: &str, extra: &[(&str, &str)]) -> Request {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
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

fn token_auth() -> LocalAuth {
    LocalAuth::Token(Arc::new(LocalToken::from_bytes(LOCAL.as_bytes()).unwrap()))
}

#[test]
fn protocol_decides_the_exact_path_and_route_name() {
    assert_eq!(
        crate::chat_route::path_for(Protocol::ChatCompletionsText),
        "/v1/chat/completions"
    );
    assert_eq!(
        crate::chat_route::path_for(Protocol::ResponsesText),
        "/v1/responses"
    );
    assert_eq!(
        Protocol::ResponsesText.route_name(),
        destination::OPENAI_RESPONSES_ROUTE
    );
}

#[tokio::test]
async fn responses_sends_the_sealed_sanitized_body_once_to_its_fixed_route_only() {
    let pair = Pair::new(LocalAuth::Disabled).await;
    let body =
        format!(r#"{{"model":"gpt-4o-mini","input":"my token is {TOKEN} thanks","store":false}}"#);
    let (status, _) = send(&pair.responses, at("/v1/responses", &body, KEY, &[])).await;
    assert_eq!(status, 200);
    assert!(
        pair.responses_fake
            .wait_for_calls(1, Duration::from_secs(5))
            .await
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    let tally = pair.responses_fake.tally();
    assert_eq!((tally.connections, tally.calls), (1, 1), "exactly one send");
    pair.chat_fake.assert_nothing_sent();
    let call = &pair.responses_fake.calls()[0];
    assert_eq!(call.method, "POST");
    assert_eq!(call.path, "/v1/responses");
    let text = String::from_utf8(call.body.clone()).unwrap();
    assert!(!text.contains(TOKEN), "planted secret reached upstream");
    assert!(text.contains("my token is <SECRET_1> thanks"), "{text}");
    assert!(text.contains(r#""store":false"#));
}

#[tokio::test]
async fn mixed_endpoints_isolate_credentials_and_forward_no_local_authority() {
    let pair = Pair::new(token_auth()).await;
    let chat_key = "sk-SYNTHETIC-REVOKED-CHAT-0000-NOT-A-KEY";
    let resp_key = "sk-SYNTHETIC-REVOKED-RESP-0000-NOT-A-KEY";
    let local = [("x-gateway-local-token", LOCAL)];
    let (s1, _) = send(
        &pair.chat,
        at("/v1/chat/completions", GOOD, chat_key, &local),
    )
    .await;
    let (s2, _) = send(
        &pair.responses,
        at("/v1/responses", GOOD_RESPONSES, resp_key, &local),
    )
    .await;
    assert_eq!((s1, s2), (200, 200));
    let chat_call = &pair.chat_fake.calls()[0];
    let resp_call = &pair.responses_fake.calls()[0];
    assert_eq!(chat_call.path, "/v1/chat/completions");
    assert_eq!(resp_call.path, "/v1/responses");
    assert_eq!(
        chat_call.header("authorization"),
        Some(&*format!("Bearer {chat_key}"))
    );
    assert_eq!(
        resp_call.header("authorization"),
        Some(&*format!("Bearer {resp_key}"))
    );
    for (call, other_key) in [(chat_call, resp_key), (resp_call, chat_key)] {
        for (name, value) in &call.headers {
            assert!(
                !name.to_ascii_lowercase().starts_with("x-gateway-"),
                "{name}"
            );
            assert!(!value.contains(LOCAL), "{name}: local token forwarded");
            assert!(
                !value.contains(other_key),
                "{name}: credential crossed endpoints"
            );
        }
        let body = String::from_utf8_lossy(&call.body);
        assert!(!body.contains(LOCAL) && !body.contains(other_key));
    }
}

#[tokio::test]
async fn local_auth_failures_on_either_endpoint_send_nothing() {
    let pair = Pair::new(token_auth()).await;
    let wrong = "SYNTH-LOCAL-TOKEN-0123456789-abcdefghijklmnX";
    for (route, path, body) in [
        (&pair.chat, "/v1/chat/completions", GOOD),
        (&pair.responses, "/v1/responses", GOOD_RESPONSES),
    ] {
        let (status, out) = send(route, at(path, body, KEY, &[])).await;
        assert_eq!(status, 401);
        assert!(String::from_utf8_lossy(&out).contains("local_auth_required"));
        let (status, out) = send(
            route,
            at(path, body, KEY, &[("x-gateway-local-token", wrong)]),
        )
        .await;
        assert_eq!(status, 401);
        assert!(String::from_utf8_lossy(&out).contains("local_auth_invalid"));
        Markers::empty()
            .with("wrong", wrong)
            .with("key", KEY)
            .assert_clean("response", &out);
    }
    pair.assert_nothing_sent();
}

#[tokio::test]
async fn protocol_mismatch_in_either_direction_sends_nothing() {
    let pair = Pair::new(LocalAuth::Disabled).await;
    let (status, out) = send(&pair.responses, at("/v1/responses", GOOD, KEY, &[])).await;
    assert!(
        status == 422 || status == 400,
        "chat body on responses: {status}"
    );
    assert!(!out.is_empty());
    let (status, _) = send(
        &pair.chat,
        at("/v1/chat/completions", GOOD_RESPONSES, KEY, &[]),
    )
    .await;
    assert!(
        status == 422 || status == 400,
        "responses body on chat: {status}"
    );
    pair.assert_nothing_sent();
}

#[tokio::test]
async fn responses_head_framing_and_header_failures_send_nothing() {
    let pair = Pair::new(LocalAuth::Disabled).await;
    let route = &pair.responses;
    let ok = |extra: &[(&str, &str)]| at("/v1/responses", GOOD_RESPONSES, KEY, extra);

    let get = Request::builder()
        .method("GET")
        .uri("/v1/responses")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(route, get).await.0, 405);
    for uri in [
        "/v1/responses?x=1",
        "/v1/responses?",
        "http://evil.example/v1/responses",
    ] {
        assert_eq!(
            send(route, at(uri, GOOD_RESPONSES, KEY, &[])).await.0,
            400,
            "{uri}"
        );
    }
    assert_eq!(send(route, ok(&[("upgrade", "websocket")])).await.0, 400);
    assert_eq!(
        send(route, ok(&[("content-encoding", "gzip")])).await.0,
        415
    );
    assert_eq!(
        send(route, ok(&[("transfer-encoding", "chunked")])).await.0,
        400
    );
    assert_eq!(
        send(route, at("/v1/responses", GOOD_RESPONSES, "", &[]))
            .await
            .0,
        401
    );
    assert_eq!(
        send(route, ok(&[("authorization", "Bearer dup")])).await.0,
        400
    );
    let mut bad_type = ok(&[]);
    bad_type
        .headers_mut()
        .insert("content-type", "text/plain".parse().unwrap());
    assert_eq!(send(route, bad_type).await.0, 415);
    let truncated = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("content-length", "999")
        .header("authorization", format!("Bearer {KEY}"))
        .body(Body::from(GOOD_RESPONSES))
        .unwrap();
    assert_eq!(send(route, truncated).await.0, 400);
    // The matrix still applies: stateful and unknown fields never leave.
    for body in [
        r#"{"model":"m","input":"hi","store":true}"#,
        r#"{"model":"m","input":"hi"}"#,
        r#"{"model":"m","input":"hi","store":false,"previous_response_id":"resp_x"}"#,
    ] {
        assert_eq!(
            send(route, at("/v1/responses", body, KEY, &[])).await.0,
            422,
            "{body}"
        );
    }
    pair.assert_nothing_sent();
}

#[tokio::test]
async fn a_route_missing_from_the_destination_table_fails_closed_without_sending() {
    let limits = RequestLimits::provisional();
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    // Only the Chat route exists in this table; the Responses id is unknown to it.
    let upstream = http_upstream_with(fake.addr(), limits);
    let plan = CapacityPlan::new(nz(8), nz(8192), nz(2), nz(8), nz(1));
    let admission = Arc::new(Admission::new(&plan));
    let inspection = Arc::new(
        Inspection::start(
            Arc::clone(&admission),
            &ContentPolicy::new(Profile::Full),
            &limits,
            &plan,
        )
        .unwrap(),
    );
    let route = EndpointRoute::responses(admission, limits, RouteId::new(RESPONSES_ROUTE_ID))
        .with_inspection(inspection)
        .with_upstream(Arc::new(upstream));
    let (status, out) = send(&route, at("/v1/responses", GOOD_RESPONSES, KEY, &[])).await;
    assert_eq!(status, 501);
    assert!(String::from_utf8_lossy(&out).contains("not_implemented"));
    fake.assert_nothing_sent();

    // No upstream at all.
    let none = EndpointRoute::responses(
        Arc::new(Admission::new(&plan)),
        limits,
        RouteId::new(RESPONSES_ROUTE_ID),
    );
    let (status, _) = send(&none, at("/v1/responses", GOOD_RESPONSES, KEY, &[])).await;
    assert_eq!(status, 501);
    let _ = (request, assert_gateway_error);
}

#[test]
fn production_table_binds_responses_to_the_fixed_https_destination() {
    let authority = UpstreamAuthority::new(Provider::OpenAi);
    let up = Upstream::new(Some(&authority)).expect("client");
    let d = up
        .destination(&RouteId::new(destination::OPENAI_RESPONSES_ROUTE))
        .expect("reviewed route");
    assert_eq!(d.origin().host(), "api.openai.com");
    assert!(d.origin().is_https());
    assert_eq!(d.origin().port(), 443);
    assert_eq!(d.path(), "/v1/responses");
    let chat = up
        .destination(&RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE))
        .expect("chat");
    assert_eq!(chat.origin(), d.origin());
    assert_ne!(chat.path(), d.path());
    for hostile in [
        "OPENAI.RESPONSES",
        "openai.responses ",
        "/v1/responses",
        "openai.responses/../x",
    ] {
        assert_eq!(
            up.destination(&RouteId::new(hostile)).unwrap_err(),
            TransportError::UnknownRoute
        );
    }
    let none = Upstream::new(None).expect("client");
    assert!(
        none.destination(&RouteId::new(destination::OPENAI_RESPONSES_ROUTE))
            .is_err()
    );
}
