//! Local caller authentication qualification (#63; ADR 0030,
//! docs/contracts/local-caller-auth.md). Compiled only under `cfg(test)`.
//!
//! Like the attack suite, these tests drive the production connection handling (head guard,
//! write-stall deadline, `Connection: close`) in front of the real chat route, the real
//! pinned-core inspection, and the central transport pointed at a loopback fake provider.
//! "Rejected" means the fake saw zero connections, zero calls, and zero body bytes, and the
//! admission counters returned to baseline. Every value is synthetic.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::attack_tests::{exchange, read_all, serve};
use super::fake_upstream::{Behavior, FakeUpstream};
use super::forward_tests::{Caps, GOOD, KEY, Rig, assert_gateway_error, request};
use super::leak::Markers;
use super::{RequestLimits, http_upstream_with};
use crate::telemetry::SafeCode;
use crate::transport::local_auth::{LocalAuth, LocalToken};

/// Synthetic local token, 43 characters of the token alphabet. Not a credential.
const LOCAL: &str = "SYNTH-LOCAL-TOKEN-0123456789-abcdefghijklmno";
/// A same-shape token that is not the configured one.
const WRONG: &str = "SYNTH-LOCAL-TOKEN-0123456789-abcdefghijklmnX";

fn auth() -> LocalAuth {
    LocalAuth::Token(Arc::new(LocalToken::from_bytes(LOCAL.as_bytes()).unwrap()))
}

async fn rig_with(auth: LocalAuth, caps: Caps) -> Rig {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let limits = RequestLimits::provisional();
    let upstream = http_upstream_with(fake.addr(), limits);
    Rig::over_auth(fake, upstream, limits, caps, auth)
}

async fn rig() -> Rig {
    rig_with(auth(), Caps::ROOMY).await
}

/// A complete raw `POST` with the given extra header lines (each ending in CRLF).
fn raw(key: Option<&str>, extra: &str, body: &str) -> Vec<u8> {
    let authorization = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\n\
         Content-Type: application/json\r\n{authorization}\
         Content-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn local(token: &str) -> String {
    format!("X-Gateway-Local-Token: {token}\r\n")
}

fn body_of(wire: &super::attack_tests::Wire) -> String {
    wire.text()
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

fn assert_local_rejection(wire: &super::attack_tests::Wire, code: SafeCode, what: &str) {
    assert_eq!(wire.status(), Some(401), "{what}");
    assert!(wire.closed, "{what}: connection closes");
    assert_eq!(
        body_of(wire),
        format!(r#"{{"error":{{"code":"{}"}}}}"#, code.as_str()),
        "{what}"
    );
    let head = wire.text().to_ascii_lowercase();
    let head = head.split("\r\n\r\n").next().unwrap_or_default();
    assert!(head.contains("connection: close"), "{what}");
    assert!(head.contains("cache-control: no-store"), "{what}");
    assert!(head.contains("content-type: application/json"), "{what}");
    assert!(
        !head.contains("www-authenticate"),
        "{what}: Bearer is the provider scheme"
    );
    Markers::empty()
        .with("local", LOCAL)
        .with("wrong", WRONG)
        .with("key", KEY)
        .assert_clean("response", &wire.bytes);
}

#[tokio::test]
async fn both_credentials_succeed_and_the_local_token_never_reaches_the_provider() {
    let rig = rig().await;
    let (addr, server) = serve(&rig).await;
    let wire = exchange(addr, &raw(Some(KEY), &local(LOCAL), GOOD)).await;
    assert_eq!(wire.status(), Some(200));
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    // The provider credential is the caller's, forwarded as is.
    assert_eq!(
        call.header("authorization"),
        Some(&*format!("Bearer {KEY}"))
    );
    for (name, value) in &call.headers {
        assert!(
            !name.to_ascii_lowercase().starts_with("x-gateway-"),
            "{name}: local authority headers are never forwarded"
        );
        assert!(
            !value.contains("SYNTH-LOCAL"),
            "{name}: local token in a value"
        );
    }
    Markers::empty()
        .with("local", LOCAL)
        .assert_clean("provider body", &call.body);
    Markers::empty()
        .with("local", LOCAL)
        .assert_clean("response", &wire.bytes);
    assert_eq!(
        rig.metrics
            .local_auth_rejections(SafeCode::LocalAuthInvalid),
        0
    );
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn header_name_case_does_not_matter_and_duplicates_of_other_headers_do_not_help() {
    let rig = rig().await;
    let (addr, server) = serve(&rig).await;
    for extra in [
        format!("x-gateway-local-token: {LOCAL}\r\n"),
        format!("X-GATEWAY-LOCAL-TOKEN: {LOCAL}\r\n"),
        format!("X-Gateway-Local-Token: {LOCAL}\r\nX-Gateway-Local-Tokens: {WRONG}\r\n"),
    ] {
        let wire = exchange(addr, &raw(Some(KEY), &extra, GOOD)).await;
        assert_eq!(wire.status(), Some(200), "{extra:?}");
    }
    assert_eq!(rig.fake.calls().len(), 3);
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn every_negative_case_sends_nothing_upstream_and_names_no_secret() {
    let rig = rig().await;
    let (addr, server) = serve(&rig).await;
    let long = "a".repeat(129);
    let short = "a".repeat(31);
    let cases: Vec<(&str, String, SafeCode)> = vec![
        ("absent", String::new(), SafeCode::LocalAuthRequired),
        ("wrong", local(WRONG), SafeCode::LocalAuthInvalid),
        (
            "duplicate identical",
            format!("{}{}", local(LOCAL), local(LOCAL)),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "duplicate different",
            format!("{}{}", local(LOCAL), local(WRONG)),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "duplicate with mixed-case names",
            format!("{}x-gateway-local-token: {LOCAL}\r\n", local(LOCAL)),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "comma list",
            local(&format!("{LOCAL},{WRONG}")),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "inner space",
            local(&format!("{LOCAL} {WRONG}")),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "bearer form",
            local(&format!("Bearer {LOCAL}")),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "quoted",
            local(&format!("\"{LOCAL}\"")),
            SafeCode::LocalAuthInvalid,
        ),
        ("empty value", local(""), SafeCode::LocalAuthInvalid),
        ("too short", local(&short), SafeCode::LocalAuthInvalid),
        ("too long", local(&long), SafeCode::LocalAuthInvalid),
        (
            "out of alphabet",
            local(&format!("{}+/=", &LOCAL[..32])),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "prefix of the token",
            local(&LOCAL[..LOCAL.len() - 1]),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "token plus a character",
            local(&format!("{LOCAL}x")),
            SafeCode::LocalAuthInvalid,
        ),
        (
            "nominated away by Connection",
            format!("{}Connection: X-Gateway-Local-Token\r\n", local(LOCAL)),
            SafeCode::LocalAuthRequired,
        ),
        (
            "token in an unrelated header",
            format!("X-Unrelated: {LOCAL}\r\n"),
            SafeCode::LocalAuthRequired,
        ),
    ];
    let count = cases.len();
    for (what, extra, code) in cases {
        let wire = exchange(addr, &raw(Some(KEY), &extra, GOOD)).await;
        assert_local_rejection(&wire, code, what);
    }
    rig.fake.assert_nothing_sent();
    rig.settle().await;
    let counted = rig
        .metrics
        .local_auth_rejections(SafeCode::LocalAuthRequired)
        + rig
            .metrics
            .local_auth_rejections(SafeCode::LocalAuthInvalid);
    assert_eq!(usize::try_from(counted).unwrap(), count);
    server.abort();
}

#[tokio::test]
async fn the_two_credentials_are_independent() {
    let rig = rig().await;
    let (addr, server) = serve(&rig).await;
    // A provider key alone does not authenticate.
    let wire = exchange(addr, &raw(Some(KEY), "", GOOD)).await;
    assert_local_rejection(&wire, SafeCode::LocalAuthRequired, "provider key only");
    // The local token placed in `Authorization` does not authenticate either.
    let wire = exchange(addr, &raw(Some(LOCAL), "", GOOD)).await;
    assert_local_rejection(&wire, SafeCode::LocalAuthRequired, "token in Authorization");
    // The local token alone does not satisfy the provider credential.
    let wire = exchange(addr, &raw(None, &local(LOCAL), GOOD)).await;
    assert_eq!(wire.status(), Some(401));
    assert_eq!(
        body_of(&wire),
        r#"{"error":{"code":"missing_credential"}}"#,
        "a missing provider key after local authentication is still missing_credential"
    );
    assert!(
        wire.text()
            .to_ascii_lowercase()
            .contains("www-authenticate: bearer")
    );
    // The local token is not accepted as a provider key replacement.
    let wire = exchange(addr, &raw(Some(LOCAL), &local(LOCAL), GOOD)).await;
    assert_eq!(wire.status(), Some(200));
    let calls = rig.fake.calls();
    assert_eq!(
        calls.len(),
        1,
        "only the fully credentialed request went upstream"
    );
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn rejection_happens_before_any_body_read_or_continue() {
    let rig = rig().await;
    let (addr, server) = serve(&rig).await;
    // Declared huge body that never arrives, with Expect: 100-continue.
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\n\
         Content-Type: application/json\r\nAuthorization: Bearer {KEY}\r\n\
         Content-Length: 900000\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let started = std::time::Instant::now();
    let wire = read_all(&mut stream, Duration::from_secs(5)).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "answered without waiting for the body"
    );
    assert_local_rejection(&wire, SafeCode::LocalAuthRequired, "no token, no body");
    assert!(
        !wire.text().contains("100 Continue"),
        "no 100 Continue for an unauthenticated caller"
    );
    // Chunked framing with a body that never starts.
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\n\
         Content-Type: application/json\r\nAuthorization: Bearer {KEY}\r\n{}\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        local(WRONG)
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let wire = read_all(&mut stream, Duration::from_secs(5)).await;
    assert_local_rejection(&wire, SafeCode::LocalAuthInvalid, "wrong token, no body");
    rig.fake.assert_nothing_sent();
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn an_unauthenticated_caller_consumes_no_capacity_and_no_inspection() {
    // One receipt permit in total, held elsewhere: an authenticated request would wait for
    // it (and time out as overload), an unauthenticated one must not even try.
    let caps = Caps {
        receipt: 1,
        inspection: 1,
        upstream: 1,
        stream: 1,
    };
    let rig = rig_with(auth(), caps).await;
    let (addr, server) = serve(&rig).await;
    let held = rig
        .admission
        .try_receipt()
        .expect("the only receipt permit");
    for extra in [String::new(), local(WRONG)] {
        let wire = exchange(addr, &raw(Some(KEY), &extra, GOOD)).await;
        assert_eq!(wire.status(), Some(401), "never 503 overload");
    }
    assert!(
        rig.admission.try_receipt().is_err(),
        "the held permit is still the only one outstanding"
    );
    drop(held);
    rig.fake.assert_nothing_sent();
    rig.settle().await;
    // Inspection never ran for them: the stage histograms are empty.
    for stage in [
        crate::telemetry::Stage::AdmissionWait,
        crate::telemetry::Stage::Parse,
        crate::telemetry::Stage::Inspection,
        crate::telemetry::Stage::UpstreamTotal,
    ] {
        assert!(rig.metrics.histogram(stage).is_empty(), "{stage:?}");
    }
    server.abort();
}

#[tokio::test]
async fn other_methods_get_the_routes_405_without_a_secret() {
    let rig = rig().await;
    let (addr, server) = serve(&rig).await;
    let wire = exchange(
        addr,
        b"GET /v1/chat/completions HTTP/1.1\r\nHost: gw.test\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(wire.status(), Some(405));
    assert_eq!(body_of(&wire), r#"{"error":{"code":"unsupported_input"}}"#);
    rig.fake.assert_nothing_sent();
    server.abort();
}

#[tokio::test]
async fn concurrent_requests_keep_credentials_isolated() {
    let caps = Caps {
        receipt: 32,
        inspection: 8,
        upstream: 32,
        stream: 1,
    };
    let rig = Arc::new(rig_with(auth(), caps).await);
    let (addr, server) = serve(&rig).await;
    let mut tasks = Vec::new();
    for i in 0..16_u32 {
        tasks.push(tokio::spawn(async move {
            let key = format!("sk-SYNTHETIC-REVOKED-CC{i:02}-0000-NOT-A-KEY");
            let good = i % 2 == 0;
            let extra = local(if good { LOCAL } else { WRONG });
            let wire = exchange(addr, &raw(Some(&key), &extra, GOOD)).await;
            (i, key, good, wire)
        }));
    }
    let mut expected_keys = BTreeSet::new();
    for task in tasks {
        let (i, key, good, wire) = task.await.unwrap();
        if good {
            assert_eq!(wire.status(), Some(200), "request {i}");
            expected_keys.insert(format!("Bearer {key}"));
        } else {
            assert_local_rejection(&wire, SafeCode::LocalAuthInvalid, "concurrent wrong token");
        }
        Markers::empty()
            .with("key", &key)
            .assert_clean("response", &wire.bytes);
    }
    let calls = rig.fake.calls();
    assert_eq!(
        calls.len(),
        8,
        "only authenticated requests reached the provider"
    );
    let seen: BTreeSet<String> = calls
        .iter()
        .map(|c| c.header("authorization").unwrap().to_owned())
        .collect();
    assert_eq!(seen, expected_keys, "each request carried only its own key");
    for call in &calls {
        assert!(call.headers.iter().all(|(n, v)| {
            !n.to_ascii_lowercase().starts_with("x-gateway-") && !v.contains("SYNTH-LOCAL")
        }));
    }
    rig.settle().await;
    server.abort();
}

#[tokio::test]
async fn shutdown_does_not_change_the_authentication_decision() {
    let rig = rig().await;
    rig.route.cancel_in_flight();
    // Authentication still runs first: an unauthenticated caller gets the same 401, and an
    // authenticated one gets the shutdown answer, never a bypass.
    let unauthenticated = rig.post(request(GOOD, KEY, &[])).await;
    assert_gateway_error(&unauthenticated, 401, "local_auth_required");
    let wrong = rig
        .post(request(GOOD, KEY, &[("x-gateway-local-token", WRONG)]))
        .await;
    assert_gateway_error(&wrong, 401, "local_auth_invalid");
    let authenticated = rig
        .post(request(GOOD, KEY, &[("x-gateway-local-token", LOCAL)]))
        .await;
    assert_gateway_error(&authenticated, 503, "not_ready");
    rig.fake.assert_nothing_sent();
}

#[tokio::test]
async fn disabled_mode_ignores_and_never_forwards_the_header() {
    let rig = rig_with(LocalAuth::Disabled, Caps::ROOMY).await;
    let (addr, server) = serve(&rig).await;
    for extra in [
        String::new(),
        local(WRONG),
        format!("{}{}", local(LOCAL), local(WRONG)),
    ] {
        let wire = exchange(addr, &raw(Some(KEY), &extra, GOOD)).await;
        assert_eq!(wire.status(), Some(200), "{extra:?}");
    }
    for call in rig.fake.calls() {
        assert!(call.header("x-gateway-local-token").is_none());
    }
    assert_eq!(
        rig.metrics
            .local_auth_rejections(SafeCode::LocalAuthRequired),
        0
    );
    rig.settle().await;
    server.abort();
}
