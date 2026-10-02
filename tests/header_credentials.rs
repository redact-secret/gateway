//! Header, credential, and framing admission over real HTTP on loopback (issue #24).
//!
//! Every case runs against the served router with raw bytes, so what is tested is the
//! actual parser plus the gateway's own checks. Forwarding is #20; the vetted outbound
//! wire (regenerated headers, credential isolation, concurrency) is tested against the
//! fake upstream in `src/transport/tests/wire_tests.rs`. All data is synthetic.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use redact_secret_gateway::chat_route::CHAT_COMPLETIONS_PATH;
use redact_secret_gateway::config;
use redact_secret_gateway::server::{self, Services, StartupError};
use support::leak::Markers;
use support::raw_http::{Response, exchange, parse_response, post_exact};
use tokio::sync::oneshot;

const GOOD: &str = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}"#;
const KEY: &str = "sk-SYNTHETIC-REVOKED-HDR1-0000-NOT-A-KEY";
const ORG: &str = "org-SYNTHETIC-ORG-HDR1";
const JSON: (&str, &str) = ("Content-Type", "application/json");

struct Gateway {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

impl Gateway {
    async fn start() -> Self {
        let doc = r#"{"schema_version":1,
            "deployment":{"listener":{"address":"127.0.0.1:0"}},
            "content":{"profile":"common"},
            "resources":{"capacity":{"receipt":4,"memory_units":8192,
                "inspection":1,"upstream":1,"stream":1},"limits":{}}}"#;
        let plan = Arc::new(config::parse(doc.as_bytes()).unwrap());
        let bound = server::bind(plan, Services::init).await.unwrap();
        let addr = bound.local_addr().unwrap();
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

    async fn raw(&self, bytes: &[u8]) -> (Option<Response>, Vec<u8>) {
        let (out, _) = exchange(self.addr, bytes, Duration::from_secs(5))
            .await
            .unwrap();
        (parse_response(&out), out)
    }

    /// Send `GOOD` with exactly `headers` plus the JSON content type.
    async fn send(&self, headers: &[(&str, &str)]) -> (u16, String) {
        let mut all = vec![JSON];
        all.extend_from_slice(headers);
        let (resp, raw) = self
            .raw(&post_exact(CHAT_COMPLETIONS_PATH, &all, GOOD.as_bytes()))
            .await;
        outcome(resp, &raw)
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = self.task.await;
    }
}

/// Status and the safe error code (empty when the body is not a gateway error).
fn outcome(resp: Option<Response>, raw: &[u8]) -> (u16, String) {
    let resp = resp.unwrap_or_else(|| panic!("no parsable response ({} bytes)", raw.len()));
    let body = String::from_utf8_lossy(&resp.body).into_owned();
    let code = body
        .split("\"code\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap_or("")
        .to_owned();
    (resp.status, code)
}

fn bearer() -> String {
    format!("Bearer {KEY}")
}

#[tokio::test]
async fn provider_credential_outcomes_are_explicit() {
    let gw = Gateway::start().await;
    let ok = bearer();

    // Present and valid: admitted (forwarding is not wired yet, so the local 501).
    assert_eq!(
        gw.send(&[("Authorization", &ok)]).await,
        (501, "not_implemented".into())
    );
    // Scheme case is insensitive.
    let lower = format!("bearer {KEY}");
    assert_eq!(gw.send(&[("Authorization", &lower)]).await.0, 501);

    // Missing: 401 with a fixed code.
    assert_eq!(gw.send(&[]).await, (401, "missing_credential".into()));
    // Another credential-looking header is not a substitute.
    assert_eq!(
        gw.send(&[("X-Api-Key", KEY)]).await,
        (401, "missing_credential".into())
    );
    assert_eq!(
        gw.send(&[("Proxy-Authorization", &ok)]).await,
        (401, "missing_credential".into())
    );
    // Nominated away by Connection: removed, hence missing.
    assert_eq!(
        gw.send(&[("Authorization", &ok), ("Connection", "Authorization")])
            .await,
        (401, "missing_credential".into())
    );

    // Duplicate (even identical), malformed, or non-Bearer: 400 malformed_input.
    for headers in [
        vec![
            ("Authorization", ok.as_str()),
            ("Authorization", ok.as_str()),
        ],
        vec![
            ("Authorization", ok.as_str()),
            ("Authorization", "Bearer other"),
        ],
        vec![("Authorization", "Basic dXNlcjpwYXNz")],
        vec![("Authorization", "Bearer")],
        vec![("Authorization", "Bearer a b")],
        vec![("Authorization", "Bearer a, Bearer b")],
    ] {
        assert_eq!(gw.send(&headers).await, (400, "malformed_input".into()));
    }

    // The 401 carries a fixed challenge and no request content.
    let (resp, _) = gw
        .raw(&post_exact(CHAT_COMPLETIONS_PATH, &[JSON], GOOD.as_bytes()))
        .await;
    let resp = resp.unwrap();
    assert!(
        resp.headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("www-authenticate") && v == "Bearer")
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn organization_and_project_are_validated_not_trusted() {
    let gw = Gateway::start().await;
    let auth = bearer();
    let base = ("Authorization", auth.as_str());
    assert_eq!(
        gw.send(&[
            base,
            ("OpenAI-Organization", ORG),
            ("OpenAI-Project", "proj_abc-1.2")
        ])
        .await
        .0,
        501
    );
    for bad in [
        vec![base, ("OpenAI-Organization", "")],
        vec![base, ("OpenAI-Organization", "org evil")],
        vec![base, ("OpenAI-Project", "a/b")],
        vec![base, ("OpenAI-Project", "p1"), ("OpenAI-Project", "p2")],
    ] {
        assert_eq!(gw.send(&bad).await, (400, "malformed_input".into()));
    }
    // A nominated metadata header is dropped, not rejected.
    assert_eq!(
        gw.send(&[
            base,
            ("Connection", "OpenAI-Project"),
            ("OpenAI-Project", "a/b")
        ])
        .await
        .0,
        501
    );
    gw.shutdown().await;
}

#[tokio::test]
async fn unreviewed_and_local_headers_are_ignored_and_ambiguous_ones_rejected() {
    let gw = Gateway::start().await;
    let auth = bearer();
    let base = ("Authorization", auth.as_str());
    // Ignored (stripped from any future outbound request): the request is still admitted.
    for ignored in [
        ("X-Forwarded-For", "203.0.113.5"),
        ("X-Forwarded-Host", "evil.example"),
        ("Forwarded", "host=evil.example"),
        ("Via", "1.1 proxy"),
        ("Cookie", "s=1"),
        ("User-Agent", "SynthSDK/1"),
        ("X-Stainless-Lang", "python"),
        ("Accept-Encoding", "gzip"),
        ("Accept", "text/html"),
        ("X-Gateway-Local-Caller-Token", "LOCAL-ONLY-SYNTHETIC"),
        ("Connection", "keep-alive, X-Foo"),
    ] {
        assert_eq!(gw.send(&[base, ignored]).await.0, 501, "{}", ignored.0);
    }
    // Rejected.
    assert_eq!(
        gw.send(&[base, ("Expect", "200-ok")]).await,
        (417, "unsupported_input".into())
    );
    assert_eq!(
        gw.send(&[base, ("Upgrade", "websocket"), ("Connection", "Upgrade")])
            .await
            .0,
        400
    );
    for nominated in ["Content-Length", "Transfer-Encoding", "Host", "a b"] {
        assert_eq!(
            gw.send(&[base, ("Connection", nominated)]).await,
            (400, "malformed_input".into()),
            "{nominated}"
        );
    }
    gw.shutdown().await;
}

#[tokio::test]
async fn ambiguous_framing_and_header_forms_are_rejected() {
    let gw = Gateway::start().await;
    let auth = bearer();
    let len = GOOD.len();
    let head = |extra: &str| {
        format!(
            "POST {CHAT_COMPLETIONS_PATH} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nAuthorization: {auth}\r\n{extra}\r\n"
        )
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "conflicting content-length",
            [
                head(&format!(
                    "Content-Length: {len}\r\nContent-Length: {}\r\n",
                    len + 1
                ))
                .as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "content-length list",
            [
                head(&format!("Content-Length: {len}, {len}\r\n")).as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "signed content-length",
            [
                head(&format!("Content-Length: +{len}\r\n")).as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "obsolete line folding",
            [
                head(&format!("Content-Length: {len}\r\nX-Folded: a\r\n b\r\n")).as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "invalid header name",
            [
                head(&format!("Content-Length: {len}\r\nBad Name: x\r\n")).as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "control byte in value",
            [
                head(&format!("Content-Length: {len}\r\nX-A: a\u{1}b\r\n")).as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "header without colon",
            [
                head(&format!("Content-Length: {len}\r\nNoColonHere\r\n")).as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "oversized header block",
            [
                head(&format!(
                    "Content-Length: {len}\r\nX-Big: {}\r\n",
                    "a".repeat(20_000)
                ))
                .as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "many headers",
            [
                head(&format!(
                    "Content-Length: {len}\r\n{}",
                    "X-N: 1\r\n".repeat(300)
                ))
                .as_bytes(),
                GOOD.as_bytes(),
            ]
            .concat(),
        ),
        (
            "transfer-encoding gzip chunked",
            [
                head("Transfer-Encoding: gzip, chunked\r\n").as_bytes(),
                format!("{len:x}\r\n{GOOD}\r\n0\r\n\r\n").as_bytes(),
            ]
            .concat(),
        ),
    ];
    for (label, bytes) in cases {
        let (out, _) = exchange(gw.addr, &bytes, Duration::from_secs(5))
            .await
            .unwrap();
        let status = parse_response(&out).map(|r| r.status);
        let expected = match label {
            "oversized header block" | "many headers" => 431,
            "transfer-encoding gzip chunked" => 415,
            _ => 400,
        };
        assert_eq!(status, Some(expected), "{label}");
        assert!(
            !String::from_utf8_lossy(&out).contains("not_implemented"),
            "{label}"
        );
    }
    // Identical duplicate `Content-Length` values are collapsed to one by the HTTP layer
    // (RFC 9112 section 6.3 permits this); the request is then an ordinary single-length
    // request whose body length is still enforced. Reviewed outcome: admitted.
    let dup = [
        head(&format!(
            "Content-Length: {len}\r\nContent-Length: {len}\r\n"
        ))
        .as_bytes(),
        GOOD.as_bytes(),
    ]
    .concat();
    let (resp, raw) = gw.raw(&dup).await;
    assert_eq!(outcome(resp, &raw).0, 501);
    // KNOWN LIMITATION, pinned so a change is noticed: hyper resolves `Content-Length` +
    // `Transfer-Encoding: chunked` itself, discarding the length (RFC 9112 section 6.3
    // allows this) before the handler runs, so the gateway cannot see the conflict and
    // cannot reject it; the request is framed as chunked and its body bound still applies.
    // Nothing from either header reaches the provider (outbound framing is regenerated from
    // the sealed bytes), so this cannot desynchronize the provider connection. Operators
    // who place a proxy in front must have it reject such requests (docs/contracts/
    // headers-and-credentials.md). A length that is invalid is also discarded when it
    // follows `Transfer-Encoding`.
    for cl in [format!("{len}"), "not-a-number".to_owned()] {
        let both = [
            head(&format!(
                "Transfer-Encoding: chunked\r\nContent-Length: {cl}\r\n"
            ))
            .as_bytes(),
            format!("{len:x}\r\n{GOOD}\r\n0\r\n\r\n").as_bytes(),
        ]
        .concat();
        let (resp, raw) = gw.raw(&both).await;
        assert_eq!(outcome(resp, &raw).0, 501);
    }
    // Control: the same head without the defect is admitted.
    let ok = [
        head(&format!("Content-Length: {len}\r\n")).as_bytes(),
        GOOD.as_bytes(),
    ]
    .concat();
    let (resp, raw) = gw.raw(&ok).await;
    assert_eq!(outcome(resp, &raw).0, 501);
    gw.shutdown().await;
}

#[tokio::test]
async fn credentials_never_appear_in_any_gateway_output() {
    let gw = Gateway::start().await;
    let markers = Markers::empty().with("provider-key", KEY).with("org", ORG);
    let auth = bearer();
    let mut outputs: Vec<Vec<u8>> = Vec::new();
    let variants: Vec<Vec<(&str, &str)>> = vec![
        vec![
            ("Authorization", auth.as_str()),
            ("OpenAI-Organization", ORG),
        ],
        vec![
            ("Authorization", auth.as_str()),
            ("Authorization", auth.as_str()),
        ],
        vec![
            ("Authorization", auth.as_str()),
            ("OpenAI-Organization", "bad value"),
        ],
        vec![("Authorization", auth.as_str()), ("Expect", "nope")],
        vec![
            ("Authorization", auth.as_str()),
            ("Connection", "Authorization"),
        ],
        vec![
            ("Authorization", auth.as_str()),
            ("Content-Type", "text/plain"),
        ],
    ];
    for headers in variants {
        let mut all = vec![JSON];
        all.extend_from_slice(&headers);
        let (_, raw) = gw
            .raw(&post_exact(CHAT_COMPLETIONS_PATH, &all, GOOD.as_bytes()))
            .await;
        outputs.push(raw);
    }
    // Credential-bearing request that fails on the body.
    let (_, raw) = gw
        .raw(&post_exact(
            CHAT_COMPLETIONS_PATH,
            &[
                JSON,
                ("Authorization", auth.as_str()),
                ("OpenAI-Organization", ORG),
            ],
            b"{not json",
        ))
        .await;
    outputs.push(raw);
    for path in ["/healthz", "/readyz", "/metrics", "/v1/other"] {
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: {auth}\r\n\r\n"
        );
        let (_, raw) = gw.raw(req.as_bytes()).await;
        outputs.push(raw);
    }
    for (i, out) in outputs.iter().enumerate() {
        assert!(!out.is_empty(), "response {i} was empty");
        markers.assert_clean(&format!("response {i}"), out);
    }
    gw.shutdown().await;
}
