//! Credential isolation and wire construction (#24). Compiled only under `cfg(test)`.
//!
//! Requests go over plain loopback HTTP to the test-only fake upstream, exactly like the
//! destination tests (#23). All keys are obviously synthetic and revoked-looking.

use super::leak::Markers;
use super::*;
use crate::transport::headers::{self, HeaderReject};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

const KEY_A: &str = "sk-SYNTHETIC-REVOKED-AAAA-0000-NOT-A-KEY";
const KEY_B: &str = "sk-SYNTHETIC-REVOKED-BBBB-1111-NOT-A-KEY";
const ORG: &str = "org-SYNTHETIC-ORG-0001";

fn inbound(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (n, v) in pairs {
        m.append(
            HeaderName::from_bytes(n.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    m
}

fn vetted(key: &str, extra: &[(&str, &str)]) -> headers::VettedHeaders {
    let auth = format!("Bearer {key}");
    let mut pairs = vec![("authorization", auth.as_str())];
    pairs.extend_from_slice(extra);
    headers::vet_inbound(&inbound(&pairs)).unwrap()
}

fn admission(units: u32) -> Admission {
    let one = NonZeroU32::new(1).unwrap();
    let receipts = NonZeroU32::new(units).unwrap();
    let memory = NonZeroU32::new(1 << 20).unwrap();
    Admission::new(&CapacityPlan::new(receipts, memory, one, one, one))
}

fn sealed(admission: &Admission, body: &[u8]) -> boundary::SanitizedRequest {
    let v = ValidatedRequest::for_test(
        admission
            .try_reserve_memory(u32::try_from(body.len()).unwrap())
            .unwrap(),
        admission.try_receipt().unwrap(),
    );
    boundary::approve(v, CompleteInspection::for_test(body.to_vec()), route()).unwrap()
}

#[tokio::test]
async fn credential_reaches_only_the_fixed_provider_with_regenerated_headers() {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let up = http_upstream(fake.addr());
    let adm = admission(4);
    let body = br#"{"model":"m","messages":[]}"#;
    let resp = up
        .outbound(
            vetted(
                KEY_A,
                &[("openai-organization", ORG), ("host", "evil.example")],
            ),
            &sealed(&adm, body),
        )
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!(
        c.header("authorization"),
        Some(format!("Bearer {KEY_A}").as_str())
    );
    assert_eq!(c.header("openai-organization"), Some(ORG));
    assert_eq!(c.header("content-type"), Some("application/json"));
    assert_eq!(
        c.header("content-length"),
        Some(body.len().to_string().as_str())
    );
    assert_eq!(c.body, body);
    assert!(c.body_complete);
    for name in c.header_names() {
        let n = name.to_ascii_lowercase();
        assert!(
            headers::WIRE_HEADER_NAMES.contains(&n.as_str()) || n == "host",
            "unreviewed outbound header {n}"
        );
    }
    // The credential is request-local: a following request without one carries none.
    let again = up.post(&route()).unwrap().body("{}").send().await.unwrap();
    assert_eq!(again.status().as_u16(), 200);
    assert!(fake.calls()[1].header("authorization").is_none());
}

#[tokio::test]
async fn caller_host_and_lengths_cannot_control_forwarding() {
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let up = http_upstream(fake.addr());
    let adm = admission(4);
    // `transfer-encoding` and `upgrade` are rejected by route admission before vetting
    // (covered in tests/header_credentials.rs); here every header that vetting ignores is
    // supplied, to show none of them reaches the wire.
    let hostile = inbound(&[
        ("authorization", &format!("Bearer {KEY_A}")),
        ("host", "evil.example"),
        ("content-length", "3"),
        ("x-forwarded-host", "evil.example"),
        ("x-forwarded-for", "203.0.113.9"),
        ("forwarded", "host=evil.example"),
        ("expect", "100-continue"),
        ("te", "trailers"),
        ("trailer", "x"),
        ("proxy-authorization", "Basic eA=="),
        ("cookie", "s=1"),
        ("x-gateway-local-caller-token", "LOCAL-ONLY-SYNTHETIC"),
        ("user-agent", "SynthSDK/9"),
        ("accept-encoding", "gzip, br"),
        ("x-stainless-lang", "python"),
    ]);
    let body = vec![b'x'; 777];
    let resp = up
        .outbound(
            headers::vet_inbound(&hostile).unwrap(),
            &sealed(&adm, &body),
        )
        .unwrap()
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let c = &fake.calls()[0];
    assert_eq!(c.header("host"), Some(fake.addr().to_string().as_str()));
    assert_eq!(c.header("content-length"), Some("777"));
    assert_eq!(c.body.len(), 777);
    assert!(c.header("transfer-encoding").is_none());
    for banned in [
        "x-forwarded-host",
        "x-forwarded-for",
        "forwarded",
        "expect",
        "te",
        "trailer",
        "proxy-authorization",
        "cookie",
        "x-gateway-local-caller-token",
        "x-stainless-lang",
        "upgrade",
        "connection",
    ] {
        assert!(c.header(banned).is_none(), "leaked {banned}");
    }
    assert_eq!(c.header("accept-encoding"), Some("identity"));
    assert!(
        c.header("user-agent")
            .unwrap()
            .starts_with("redact-secret-gateway/")
    );
    let all = format!("{:?}", c.headers);
    assert!(!all.contains("LOCAL-ONLY-SYNTHETIC") && !all.contains("SynthSDK"));
}

#[tokio::test]
async fn concurrent_requests_with_different_keys_do_not_cross() {
    const N: usize = 24;
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let up = Arc::new(http_upstream(fake.addr()));
    let adm = Arc::new(admission(64));
    let mut tasks = Vec::new();
    for i in 0..N {
        let (up, adm) = (Arc::clone(&up), Arc::clone(&adm));
        tasks.push(tokio::spawn(async move {
            let key = format!("sk-SYNTHETIC-REVOKED-{i:04}-NOT-A-KEY");
            let org = format!("org-SYNTH-{i:04}");
            let body = format!(r#"{{"id":{i}}}"#);
            let request = sealed(&adm, body.as_bytes());
            up.outbound(vetted(&key, &[("openai-organization", &org)]), &request)
                .unwrap()
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let calls = fake.calls();
    assert_eq!(calls.len(), N);
    let mut seen = [false; N];
    for c in &calls {
        let body = String::from_utf8(c.body.clone()).unwrap();
        let i: usize = body
            .trim_start_matches(r#"{"id":"#)
            .trim_end_matches('}')
            .parse()
            .unwrap();
        assert_eq!(
            c.header("authorization"),
            Some(format!("Bearer sk-SYNTHETIC-REVOKED-{i:04}-NOT-A-KEY").as_str())
        );
        assert_eq!(
            c.header("openai-organization"),
            Some(format!("org-SYNTH-{i:04}").as_str())
        );
        seen[i] = true;
    }
    assert!(
        seen.iter().all(|s| *s),
        "every request arrived exactly once"
    );
}

#[tokio::test]
async fn credential_bearing_objects_do_not_leak_through_debug_or_errors() {
    let markers = Markers::empty()
        .with("key-a", KEY_A)
        .with("key-b", KEY_B)
        .with("org", ORG);
    let v = vetted(KEY_A, &[("openai-organization", ORG)]);
    markers.assert_clean_debug("VettedHeaders", &v);
    markers.assert_clean_debug("VettedHeaders pretty", &format!("{v:#?}"));

    let cred = crate::transport::credential::ProviderCredential::parse(
        format!("Bearer {KEY_B}").as_bytes(),
    )
    .unwrap();
    markers.assert_clean_debug("ProviderCredential", &cred);

    // The built request (what the HTTP library would log) marks the value sensitive.
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let up = http_upstream(fake.addr());
    let adm = admission(4);
    let request = sealed(&adm, b"{}");
    let builder = up
        .outbound(vetted(KEY_A, &[("openai-organization", ORG)]), &request)
        .unwrap();
    markers.assert_clean_debug("RequestBuilder", &builder);
    let built = builder.build().unwrap();
    markers.assert_clean_debug("Request", &built);
    markers.assert_clean_debug("Upstream", &up);
    markers.assert_clean_debug("SanitizedRequest", &request);

    // Error values are fixed codes.
    let none = Upstream::new(None).unwrap();
    let err = none
        .outbound(vetted(KEY_A, &[]), &sealed(&adm, b"{}"))
        .unwrap_err();
    markers.assert_clean_fmt("TransportError", &err);
    for e in [
        HeaderReject::MissingCredential,
        HeaderReject::Credential,
        HeaderReject::Metadata,
        HeaderReject::Connection,
        HeaderReject::TooLarge,
        HeaderReject::Expectation,
    ] {
        markers.assert_clean_fmt("HeaderReject", &e);
    }
    // A rejected inbound header set reports a fixed reason without echoing any value.
    let bad = inbound(&[("authorization", &format!("Basic {KEY_A}"))]);
    let e = headers::vet_inbound(&bad).unwrap_err();
    markers.assert_clean_fmt("vet_inbound error", &e);
}

#[test]
fn profile_and_deployment_state_do_not_alter_header_policy() {
    const DOC: &str = r#"{"schema_version":1,
        "deployment":{"listener":{"address":"127.0.0.1:0"},"upstream":{"provider":"openai"}},
        "content":{"profile":"PROFILE"},
        "resources":{"capacity":{"receipt":1,"memory_units":4,"inspection":1,"upstream":1,"stream":1}}}"#;
    let adm = admission(2);
    let mut built = Vec::new();
    for profile in ["common", "full"] {
        let plan = crate::config::parse(DOC.replace("PROFILE", profile).as_bytes()).unwrap();
        let up = Upstream::from_plan(&plan).unwrap();
        let route = RouteId::new(destination::OPENAI_CHAT_COMPLETIONS_ROUTE);
        let v = ValidatedRequest::for_test(
            adm.try_reserve_memory(2).unwrap(),
            adm.try_receipt().unwrap(),
        );
        let request =
            boundary::approve(v, CompleteInspection::for_test(b"{}".to_vec()), route).unwrap();
        let req = up
            .outbound(vetted(KEY_A, &[("openai-project", "proj_1")]), &request)
            .unwrap()
            .build()
            .unwrap();
        let mut names: Vec<String> = req
            .headers()
            .keys()
            .map(|n| n.as_str().to_owned())
            .collect();
        names.sort();
        built.push((
            req.url().as_str().to_owned(),
            names,
            req.headers()["content-length"].clone(),
        ));
    }
    assert_eq!(built[0], built[1]);
    assert_eq!(built[0].0, "https://api.openai.com/v1/chat/completions");
}
