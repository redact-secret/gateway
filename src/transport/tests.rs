//! Unit tests for the outbound authority layer (#23). Compiled only under `cfg(test)`.
//!
//! This module is the *only* mechanism that can point the client at a loopback fake
//! upstream or at a non-reviewed host: the constructors it uses (`Origin::for_test_*`,
//! `AddressPolicy::PublicOrLoopback`, `Scheme::Http`) are `#[cfg(test)]` items that do not
//! exist in any non-test build, so no configuration, flag, or environment variable can
//! reach them (tests/destination_policy.rs checks this from the outside). All data is
//! synthetic; nothing leaves loopback.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::destination::{Destination, Origin, RouteBinding};
use super::resolver::{AddressPolicy, AddressSource, PolicyResolver};
use super::*;
use crate::admission::{Admission, CapacityPlan, RequestLimits};
use crate::boundary;
use crate::config::{Provider, RouteId, UpstreamAuthority};
use crate::core_bridge::CompleteInspection;
use crate::protocol::ValidatedRequest;

#[allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    missing_debug_implementations
)]
#[path = "../../tests/support/fake_upstream.rs"]
mod fake_upstream;

mod attack_tests;
mod forward_tests;
mod framing_matrix_tests;
mod header_cap_tests;
#[path = "../../tests/support/leak.rs"]
#[allow(dead_code)]
mod leak;
mod lifecycle_tests;
mod local_auth_tests;
mod responses_route_tests;
mod responses_stream_tests;
mod slot_coverage_tests;
mod stream_tests;
mod tool_history_tests;
mod wire_tests;

use fake_upstream::{Behavior, FakeUpstream};

const ROUTE: &str = "test.route";
const PATH: &str = "/v1/chat/completions";
const TEST_HOST: &str = "provider.test";

fn route() -> RouteId {
    RouteId::new(ROUTE)
}

/// The Chat protocol bound to `id`, as the served route builds it.
fn chat_route(id: RouteId) -> boundary::ProtocolRoute {
    boundary::ProtocolRoute::new(crate::protocol::Protocol::ChatCompletionsText, id)
}

/// Client over plain loopback HTTP (the one place `https_only` is lowered, test-only).
fn http_upstream(addr: SocketAddr) -> Upstream {
    http_upstream_with(addr, RequestLimits::provisional())
}

/// As [`http_upstream`] with explicit deadlines and bounds. The client carries no overall
/// timeout of its own: the deadlines under test are the transport's.
fn http_upstream_with(addr: SocketAddr, limits: RequestLimits) -> Upstream {
    let origin = Origin::for_test_http(addr);
    let dest = Destination::for_test(origin, PATH).expect("destination");
    let resolver = PolicyResolver::new(vec![], Arc::new(NoSource), AddressPolicy::PublicOrLoopback);
    let client = hardened_builder(resolver)
        .https_only(false)
        .connect_timeout(limits.upstream_connect())
        .build()
        .expect("client");
    Upstream {
        client,
        routes: vec![RouteBinding::for_test(route(), dest)].into_boxed_slice(),
        limits,
        metrics: None,
    }
}

thread_local! {
    static CLIENT_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count clients built on this thread (every client goes through [`hardened_builder`]).
pub(super) fn note_client_build() {
    CLIENT_BUILDS.with(|c| c.set(c.get().saturating_add(1)));
}

fn client_builds() -> usize {
    CLIENT_BUILDS.with(std::cell::Cell::get)
}

struct NoSource;
impl AddressSource for NoSource {
    fn lookup(
        &self,
        _host: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<IpAddr>>> + Send>> {
        Box::pin(async { None })
    }
}

/// Fixed answer, counts lookups.
struct FixedSource {
    answer: Vec<IpAddr>,
    lookups: Arc<AtomicUsize>,
}
impl AddressSource for FixedSource {
    fn lookup(
        &self,
        _host: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<IpAddr>>> + Send>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let a = self.answer.clone();
        Box::pin(async move { Some(a) })
    }
}

// ---------------------------------------------------------------- TLS fake

struct Ca {
    der: Vec<u8>,
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
}

fn new_ca(name: &str) -> Ca {
    let key = rcgen::KeyPair::generate().expect("ca key");
    let mut params = rcgen::CertificateParams::new(vec![]).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    let cert = params.self_signed(&key).expect("ca cert");
    Ca {
        der: cert.der().to_vec(),
        issuer: rcgen::Issuer::new(params, key),
    }
}

fn leaf_signed_by(ca: &Ca, host: &str) -> (Vec<u8>, Vec<u8>) {
    let key = rcgen::KeyPair::generate().expect("leaf key");
    let params = rcgen::CertificateParams::new(vec![host.to_owned()]).expect("params");
    let cert = params.signed_by(&key, &ca.issuer).expect("leaf");
    (cert.der().to_vec(), key.serialize_der())
}

/// A leaf for `host` from a trusted CA whose validity ended long ago.
fn leaf_expired(ca: &Ca, host: &str) -> (Vec<u8>, Vec<u8>) {
    let key = rcgen::KeyPair::generate().expect("leaf key");
    let mut params = rcgen::CertificateParams::new(vec![host.to_owned()]).expect("params");
    params.not_before = rcgen::date_time_ymd(2000, 1, 1);
    params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    let cert = params.signed_by(&key, &ca.issuer).expect("leaf");
    (cert.der().to_vec(), key.serialize_der())
}

fn self_signed(host: &str) -> (Vec<u8>, Vec<u8>) {
    let key = rcgen::KeyPair::generate().expect("key");
    let params = rcgen::CertificateParams::new(vec![host.to_owned()]).expect("params");
    let cert = params.self_signed(&key).expect("cert");
    (cert.der().to_vec(), key.serialize_der())
}

struct TlsFake {
    addr: SocketAddr,
    tcp_connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TlsFake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TlsFake {
    async fn start(cert_der: Vec<u8>, key_der: Vec<u8>) -> Self {
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert_der)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
            )
            .expect("server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let tcp_connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let (tc, rq) = (Arc::clone(&tcp_connections), Arc::clone(&requests));
        let task = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                tc.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                let rq = Arc::clone(&rq);
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = vec![0_u8; 4096];
                    let mut seen = Vec::new();
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => seen.extend_from_slice(&buf[..n]),
                        }
                    }
                    rq.fetch_add(1, Ordering::SeqCst);
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Self {
            addr,
            tcp_connections,
            requests,
            task,
        }
    }

    fn tcp(&self) -> usize {
        self.tcp_connections.load(Ordering::SeqCst)
    }

    fn served(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// HTTPS client for `provider.test` resolved (test source) to the TLS fake's loopback
/// address, with full certificate and hostname verification. `trusted` adds one test root.
fn tls_upstream(
    fake: &TlsFake,
    trusted: Option<&Ca>,
    policy: AddressPolicy,
    answer: Vec<IpAddr>,
    lookups: Arc<AtomicUsize>,
) -> Upstream {
    let origin = Origin::for_test_https(TEST_HOST, fake.addr.port());
    let dest = Destination::for_test(origin, PATH).expect("destination");
    let resolver = PolicyResolver::new(
        vec![TEST_HOST.into()],
        Arc::new(FixedSource { answer, lookups }),
        policy,
    );
    let mut b = hardened_builder(resolver).timeout(Duration::from_secs(5));
    if let Some(ca) = trusted {
        b = b.add_root_certificate(reqwest::Certificate::from_der(&ca.der).expect("root"));
    }
    Upstream {
        client: b.build().expect("client"),
        routes: vec![RouteBinding::for_test(route(), dest)].into_boxed_slice(),
        limits: RequestLimits::provisional(),
        metrics: None,
    }
}

/// An untrusted-certificate client for the TLS fake, resolving to loopback (used by the
/// forwarding tests, which must not name the address-policy seam themselves).
fn untrusted_tls_upstream(fake: &TlsFake) -> Upstream {
    tls_upstream(
        fake,
        None,
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::new(AtomicUsize::new(0)),
    )
}

fn loopback() -> Vec<IpAddr> {
    vec![IpAddr::from([127, 0, 0, 1])]
}

async fn send(up: &Upstream) -> Result<reqwest::Response, reqwest::Error> {
    up.post(&route()).expect("route").body("{}").send().await
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn forward_to_an_unknown_route_fails_closed_before_any_send() {
    let one = NonZeroU32::new(1).expect("nonzero");
    let admission = Admission::new(&CapacityPlan::new(one, one, one, one, one));
    let v = ValidatedRequest::for_test(
        admission.try_reserve_memory(1).expect("reserve"),
        admission.try_receipt().expect("receipt"),
    );
    let s = boundary::approve(
        v,
        CompleteInspection::for_test(b"x".to_vec()),
        chat_route(RouteId::new("r")),
    )
    .expect("approved");
    let upstream = Upstream::new(None).expect("client");
    let headers = forward_tests::vetted_for_forward("sk-SYNTHETIC-REVOKED-ZZZZ-9999-NOT-A-KEY");
    let permit = admission.try_upstream().expect("upstream");
    assert_eq!(
        upstream.forward(s, headers, permit).await.unwrap_err(),
        TransportError::UnknownRoute
    );
    // The permit came back with the dropped error path.
    assert!(admission.try_upstream().is_ok());
}

#[test]
fn routes_resolve_only_by_exact_route_id() {
    let none = Upstream::new(None).expect("client");
    assert_eq!(
        none.destination(&RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE))
            .unwrap_err(),
        TransportError::UnknownRoute
    );
    assert!(none.post(&route()).is_err());

    let authority = UpstreamAuthority::new(Provider::OpenAi);
    let up = Upstream::new(Some(&authority)).expect("client");
    let d = up
        .destination(&RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE))
        .expect("reviewed route");
    assert_eq!(d.origin().host(), "api.openai.com");
    assert!(d.origin().is_https());
    assert_eq!(d.origin().port(), 443);
    for hostile in [
        "https://evil.example/v1/chat/completions",
        "//evil.example",
        "OPENAI.CHAT_COMPLETIONS",
        "openai.chat_completions ",
        " openai.chat_completions",
        "openai.chat_completions\0",
        "openai.chat_completions/../x",
        "/v1/chat/completions",
        "openai",
        "",
    ] {
        assert_eq!(
            up.destination(&RouteId::new(hostile)).unwrap_err(),
            TransportError::UnknownRoute,
            "{hostile:?}"
        );
    }
    let req = up
        .post(&RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE))
        .expect("builder")
        .build()
        .expect("request");
    assert_eq!(req.method(), reqwest::Method::POST);
    assert_eq!(
        req.url().as_str(),
        "https://api.openai.com/v1/chat/completions"
    );
}

#[test]
fn profile_variation_cannot_change_destination_or_tls() {
    const DOC: &str = r#"{"schema_version":1,
        "deployment":{"listener":{"address":"127.0.0.1:0"},"upstream":{"provider":"openai"}},
        "content":{"profile":"PROFILE"},
        "resources":{"capacity":{"receipt":1,"memory_units":1,"inspection":1,"upstream":1,"stream":1}}}"#;
    let mut seen = Vec::new();
    for profile in ["common", "full"] {
        let plan = crate::config::parse(DOC.replace("PROFILE", profile).as_bytes()).expect("plan");
        let up = Upstream::from_plan(&plan).expect("client");
        let d = up
            .destination(&RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE))
            .expect("route");
        assert!(d.origin().is_https());
        assert_eq!(d.origin().port(), 443);
        seen.push(d.url().as_str().to_owned());
        assert_eq!(up.routes.len(), 2, "Chat and Responses");
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], "https://api.openai.com/v1/chat/completions");
}

#[tokio::test]
async fn caller_headers_cannot_change_destination_and_no_default_credentials() {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let up = http_upstream(fake.addr());
    let resp = up
        .post(&route())
        .expect("route")
        .header("Host", "evil.example")
        .header("X-Forwarded-Host", "evil.example")
        .header("X-Original-URL", "http://evil.example/steal")
        .header("X-Rewrite-URL", "/other")
        .header("Forwarded", "host=evil.example")
        .body("{}")
        .send()
        .await
        .expect("sent");
    assert_eq!(resp.status().as_u16(), 200);
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, PATH);
    assert_eq!(calls[0].method, "POST");

    // Without request-local headers the shared client adds no credential-class header.
    let plain = send(&up).await.expect("sent");
    assert_eq!(plain.status().as_u16(), 200);
    let calls = fake.calls();
    let last = &calls[1];
    for name in last.header_names() {
        let n = name.to_ascii_lowercase();
        assert!(
            !matches!(
                n.as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "x-api-key"
            ),
            "default credential-class header {n}"
        );
    }
    assert!(last.header("referer").is_none());
}

#[tokio::test]
async fn redirects_are_never_followed() {
    let second = FakeUpstream::start(Behavior::ok_json()).await;
    let first = FakeUpstream::start(Behavior::ok_json()).await;
    let up = http_upstream(first.addr());
    for status in [301_u16, 302, 303, 307, 308] {
        for target in [
            format!("http://{}/second", second.addr()),
            "/second".to_owned(),
            format!("http://{}{PATH}", first.addr()),
        ] {
            let raw = format!(
                "HTTP/1.1 {status} Moved\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            first.set_default(Behavior::Malformed(raw.into_bytes()));
            let before = first.calls().len();
            let resp = send(&up)
                .await
                .expect("redirect response is returned as is");
            assert_eq!(resp.status().as_u16(), status);
            assert_eq!(
                first.calls().len(),
                before + 1,
                "exactly one request per call, {status} {target}"
            );
        }
    }
    second.assert_nothing_sent();
}

#[tokio::test]
async fn https_only_refuses_plain_http_even_for_a_reviewed_looking_url() {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    // The production-hardened builder with nothing lowered.
    let resolver = PolicyResolver::new(vec![], Arc::new(NoSource), AddressPolicy::Public);
    let client = hardened_builder(resolver).build().expect("client");
    let err = client.post(fake.base_url()).body("{}").send().await;
    assert!(err.is_err());
    fake.assert_nothing_sent();
}

#[tokio::test]
async fn tls_positive_control_then_invalid_tls_rejects() {
    let lookups = Arc::new(AtomicUsize::new(0));
    let trusted = new_ca("synthetic trusted root");

    // Control: valid chain for the right hostname succeeds (so the rejections below are
    // caused by the verification, not by a broken harness).
    let (c, k) = leaf_signed_by(&trusted, TEST_HOST);
    let ok = TlsFake::start(c, k).await;
    let up = tls_upstream(
        &ok,
        Some(&trusted),
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::clone(&lookups),
    );
    assert_eq!(send(&up).await.expect("valid tls").status().as_u16(), 200);
    assert_eq!(ok.served(), 1);

    // Wrong hostname: chain is trusted, name is not.
    let (c, k) = leaf_signed_by(&trusted, "other.test");
    let wrong_name = TlsFake::start(c, k).await;
    let up = tls_upstream(
        &wrong_name,
        Some(&trusted),
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::clone(&lookups),
    );
    assert!(send(&up).await.is_err(), "hostname mismatch must reject");
    assert_eq!(wrong_name.served(), 0);

    // Self-signed leaf for the right name, not trusted.
    let (c, k) = self_signed(TEST_HOST);
    let selfsigned = TlsFake::start(c, k).await;
    let up = tls_upstream(
        &selfsigned,
        Some(&trusted),
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::clone(&lookups),
    );
    assert!(send(&up).await.is_err(), "self-signed must reject");
    assert_eq!(selfsigned.served(), 0);

    // Right name, chain from an unknown CA (nothing trusted beyond the platform roots).
    let rogue = new_ca("synthetic rogue root");
    let (c, k) = leaf_signed_by(&rogue, TEST_HOST);
    let unknown_ca = TlsFake::start(c, k).await;
    let up = tls_upstream(
        &unknown_ca,
        Some(&trusted),
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::clone(&lookups),
    );
    assert!(send(&up).await.is_err(), "unknown CA must reject");
    assert_eq!(unknown_ca.served(), 0);

    // Right name, trusted chain, but the certificate expired long ago (#25).
    let (c, k) = leaf_expired(&trusted, TEST_HOST);
    let expired = TlsFake::start(c, k).await;
    let up = tls_upstream(
        &expired,
        Some(&trusted),
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::clone(&lookups),
    );
    assert!(send(&up).await.is_err(), "expired certificate must reject");
    assert_eq!(expired.served(), 0);
}

#[tokio::test]
async fn disallowed_addresses_reject_before_any_connection() {
    let trusted = new_ca("synthetic trusted root");
    let (c, k) = leaf_signed_by(&trusted, TEST_HOST);
    let fake = TlsFake::start(c, k).await;
    for bad in [
        "127.0.0.1",
        "10.1.2.3",
        "169.254.169.254",
        "192.168.0.10",
        "0.0.0.0",
        "224.0.0.1",
        "::1",
        "::ffff:127.0.0.1",
        "fd00::1",
        "fe80::1",
    ] {
        let up = tls_upstream(
            &fake,
            Some(&trusted),
            AddressPolicy::Public,
            vec![bad.parse().expect("ip")],
            Arc::new(AtomicUsize::new(0)),
        );
        assert!(send(&up).await.is_err(), "{bad} must be rejected");
    }
    assert_eq!(fake.tcp(), 0, "no connection may reach a denied address");
}

#[tokio::test]
async fn validation_and_connection_share_one_resolution() {
    let trusted = new_ca("synthetic trusted root");
    let (c, k) = leaf_signed_by(&trusted, TEST_HOST);
    let fake = TlsFake::start(c, k).await;
    let lookups = Arc::new(AtomicUsize::new(0));
    let up = tls_upstream(
        &fake,
        Some(&trusted),
        AddressPolicy::PublicOrLoopback,
        loopback(),
        Arc::clone(&lookups),
    );
    assert!(send(&up).await.is_ok());
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        1,
        "one lookup per connection"
    );
    assert_eq!(fake.tcp(), 1);
}

#[test]
fn production_resolver_policy_is_public_only() {
    let authority = UpstreamAuthority::new(Provider::OpenAi);
    // Built by the production path; its policy cannot be loopback-permissive because the
    // permissive variant is a cfg(test) enum arm that production construction never names.
    let up = Upstream::new(Some(&authority)).expect("client");
    assert_eq!(up.routes.len(), 2, "Chat and Responses");
    let r = PolicyResolver::system(vec!["api.openai.com".into()]);
    assert!(format!("{r:?}").contains("Public"));
    assert!(!format!("{r:?}").contains("Loopback"));
}

// ------------------------------------------------------------ proxy environment

const CHILD_ENV: &str = "GATEWAY_TEST_PROXY_CHILD_FAKE";

/// Child half of the proxy-environment test: only does work when launched by the parent
/// with proxy variables set. It sends one request over the production-hardened builder
/// and checks that the fake upstream (not any proxy) received it.
#[tokio::test]
async fn proxy_env_child() {
    let Ok(target) = std::env::var(CHILD_ENV) else {
        return;
    };
    let addr: SocketAddr = target.parse().expect("fake addr");
    // Prove the environment really carries proxy settings in this process.
    assert!(std::env::var("HTTP_PROXY").is_ok() && std::env::var("HTTPS_PROXY").is_ok());
    let origin = Origin::for_test_http(addr);
    let dest = Destination::for_test(origin, PATH).expect("destination");
    let resolver = PolicyResolver::new(vec![], Arc::new(NoSource), AddressPolicy::PublicOrLoopback);
    let client = hardened_builder(resolver)
        .https_only(false)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    let up = Upstream {
        client,
        routes: vec![RouteBinding::for_test(route(), dest)].into_boxed_slice(),
        limits: RequestLimits::provisional(),
        metrics: None,
    };
    let resp = send(&up).await.expect("direct request");
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn inherited_proxy_environment_does_not_reroute() {
    if std::env::var(CHILD_ENV).is_ok() {
        return; // running as the child; the child test above does the work
    }
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
    proxy.set_nonblocking(true).expect("nonblocking");
    let proxy_url = format!("http://{}", proxy.local_addr().expect("addr"));
    let fake = FakeUpstream::start(Behavior::ok_json()).await;

    let exe = std::env::current_exe().expect("test exe");
    let fake_addr = fake.addr().to_string();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .args([
                "--exact",
                "transport::tests::proxy_env_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, fake_addr)
            .env("HTTP_PROXY", &proxy_url)
            .env("HTTPS_PROXY", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("http_proxy", &proxy_url)
            .env("https_proxy", &proxy_url)
            .env("all_proxy", &proxy_url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
    })
    .await
    .expect("join")
    .expect("spawn child");
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("proxy_env_child ... ok"),
        "child test must have actually run"
    );
    assert_eq!(
        fake.calls().len(),
        1,
        "request went direct to the destination"
    );
    match proxy.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("proxy must see no connection, got {other:?}"),
    }
}
