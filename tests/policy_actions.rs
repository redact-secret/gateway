//! Request-level block/redact policy against the pinned core (issue #56; ADR 0026;
//! `docs/contracts/request-policy.md` and `core-completeness.md`).
//!
//! Drives the real `Inspection` service (bounded worker pool, one pinned-core registry per
//! worker) from a `ValidatedRequest` to a `SanitizedRequest`. Every credential is a
//! synthetic revoked-style token and every PII value is invented.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod support;

use std::num::NonZeroU32;
use std::sync::Arc;

use redact_secret::{Action, Profile};
use redact_secret_gateway::admission::{Admission, CapacityPlan, RequestLimits};
use redact_secret_gateway::boundary::{BoundaryError, Inspection, SanitizedRequest};
use redact_secret_gateway::config::{self, ContentPolicy, OnWarn, RouteId};
use redact_secret_gateway::core_bridge::{
    CoreBridgeError, FindingDisposition, Inspector, InspectorSpec, RequestScope, disposition,
};
use redact_secret_gateway::protocol::{self, Protocol};
use serde_json::{Value, json};
use support::leak::Markers;

fn token(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

const PEM_ESCAPED: &str = "-----BEGIN PRIVATE KEY-----\\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\\n-----END PRIVATE KEY-----";
const WARN: &str = "password=hunter2xyz";

fn pem() -> String {
    PEM_ESCAPED.replace("\\n", "\n")
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

struct Lab {
    admission: Arc<Admission>,
    inspection: Inspection,
    limits: RequestLimits,
}

impl Lab {
    fn new(content: &ContentPolicy) -> Self {
        Self::with(content, RequestLimits::provisional())
    }

    fn with(content: &ContentPolicy, inspect_limits: RequestLimits) -> Self {
        let plan = CapacityPlan::new(nz(4), nz(8192), nz(2), nz(1), nz(1));
        let admission = Arc::new(Admission::new(&plan));
        let inspection =
            Inspection::start(Arc::clone(&admission), content, &inspect_limits, &plan).unwrap();
        Self {
            admission,
            inspection,
            limits: RequestLimits::provisional(),
        }
    }

    async fn run(&self, body: &str) -> Result<SanitizedRequest, BoundaryError> {
        let received = self
            .admission
            .begin_body_receipt(body.len(), &self.limits)
            .await
            .unwrap()
            .complete(body.as_bytes().to_vec())
            .unwrap();
        let validated =
            protocol::validate_with(received, Protocol::ChatCompletionsText, &self.limits).unwrap();
        self.inspection
            .inspect_and_approve(validated, RouteId::new("synthetic-route"))
            .await
    }
}

fn policy(profile: Profile, pii: &[&str], on_warn: OnWarn) -> ContentPolicy {
    ContentPolicy::new(profile)
        .with_pii(pii.iter().map(|s| (*s).to_owned()).collect())
        .with_on_warn(on_warn)
}

fn chat(contents: &[String]) -> String {
    let messages: Vec<Value> = contents
        .iter()
        .map(|c| json!({"role": "user", "content": c}))
        .collect();
    json!({"model": "m", "messages": messages}).to_string()
}

fn contents(s: &SanitizedRequest) -> Vec<String> {
    let v: Value = serde_json::from_slice(s.body()).unwrap();
    v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect()
}

// ------------------------------------------------------------------ the action table

#[test]
fn every_core_action_maps_to_one_gateway_disposition() {
    use FindingDisposition as D;
    for (action, reject, expected) in [
        (Action::Redact, true, D::Redact),
        (Action::Redact, false, D::Redact),
        (Action::Block, true, D::Block),
        (Action::Block, false, D::Block),
        (Action::Warn, true, D::WarnReject),
        (Action::Warn, false, D::WarnForward),
        (Action::Allow, true, D::Allow),
        (Action::Allow, false, D::Allow),
    ] {
        assert_eq!(disposition(action, reject), expected, "{action:?} {reject}");
    }
}

#[tokio::test]
async fn complete_finding_classes_map_to_documented_behavior() {
    let markers = Markers::standard();
    // Redact class: credential redacted in place, nothing else changes.
    let lab = Lab::new(&policy(Profile::Full, &[], OnWarn::Reject));
    let out = lab.run(&chat(&[format!("k={}", token(1))])).await.unwrap();
    assert_eq!(contents(&out), ["k=<SECRET_1>"]);
    markers.assert_clean("redacted body", out.body());

    // Block class: a private key blocks in either warn mode.
    for on_warn in [OnWarn::Reject, OnWarn::Forward] {
        let lab = Lab::new(&policy(Profile::Full, &[], on_warn));
        let body = chat(&[format!("x {}", pem())]);
        assert_eq!(
            lab.run(&body).await.unwrap_err(),
            BoundaryError::Core(CoreBridgeError::Blocked)
        );
    }

    // Warn class: reject by default, forward unchanged only by explicit choice.
    let lab = Lab::new(&policy(Profile::Full, &[], OnWarn::Reject));
    assert_eq!(
        lab.run(&chat(&[WARN.into()])).await.unwrap_err(),
        BoundaryError::Core(CoreBridgeError::Warned)
    );
    let lab = Lab::new(&policy(Profile::Full, &[], OnWarn::Forward));
    assert_eq!(
        contents(&lab.run(&chat(&[WARN.into()])).await.unwrap()),
        [WARN]
    );

    // No findings: forwarded as a fresh document.
    assert_eq!(
        contents(&lab.run(&chat(&["plain 안녕".into()])).await.unwrap()),
        ["plain 안녕"]
    );
}

// ------------------------------------------------------------------ precedence

#[tokio::test]
async fn a_block_in_any_slot_blocks_the_whole_request_whatever_else_is_clean() {
    let block_last = chat(&[token(1), WARN.into(), "clean".into(), pem()]);
    let block_first = chat(&[pem(), token(2)]);
    for on_warn in [OnWarn::Reject, OnWarn::Forward] {
        let lab = Lab::new(&policy(Profile::Full, &[], on_warn));
        let last = lab.run(&block_last).await.unwrap_err();
        let first = lab.run(&block_first).await.unwrap_err();
        // With `forward`, the earlier warning is tolerated and the later Block decides.
        if on_warn == OnWarn::Forward {
            assert_eq!(last, BoundaryError::Core(CoreBridgeError::Blocked));
        }
        assert_eq!(first, BoundaryError::Core(CoreBridgeError::Blocked));
        // Block and rejected-warn share one public code, so precedence is not observable
        // by a client; both are `unsupported_input`.
        assert_eq!(last.code(), first.code());
    }
}

#[tokio::test]
async fn warning_forward_never_permits_an_incomplete_scan() {
    let lab = Lab::new(&policy(Profile::Full, &[], OnWarn::Forward).with_max_findings(2));
    // A tolerated warning plus findings over the request bound: the bound still rejects.
    let body = chat(&[WARN.into(), token(1), token(2)]);
    assert_eq!(
        lab.run(&body).await.unwrap_err(),
        BoundaryError::Core(CoreBridgeError::LimitExceeded)
    );
    // The request-wide input bound is likewise not relaxed by `forward`.
    let mut tight = RequestLimits::provisional();
    tight.max_body_bytes = 100;
    let lab = Lab::with(&policy(Profile::Full, &[], OnWarn::Forward), tight);
    let half = "a".repeat(60);
    assert_eq!(
        lab.run(&chat(&[half.clone(), half])).await.unwrap_err(),
        BoundaryError::Core(CoreBridgeError::LimitExceeded)
    );
}

#[test]
fn detect_only_checks_ignore_the_warning_mode_and_block_any_finding() {
    // Structural identifiers (for example `model`) are never rewritten and never "warned
    // through": even with `on_warn = forward` any finding blocks.
    let spec = InspectorSpec::new("full", &[], 4096, 100)
        .unwrap()
        .with_warning_rejection(false);
    let inspector = Inspector::new(&spec).unwrap();
    assert_eq!(
        inspector.reject_if_findings(WARN),
        Err(CoreBridgeError::Blocked)
    );
    assert_eq!(
        inspector.reject_if_findings(&token(3)),
        Err(CoreBridgeError::Blocked)
    );
    assert_eq!(inspector.reject_if_findings("gpt-4o-mini"), Ok(()));
}

#[test]
fn a_failed_leaf_poisons_the_scope_so_no_proof_can_follow() {
    let spec = InspectorSpec::new("full", &[], 4096, 100).unwrap();
    let inspector = Inspector::new(&spec).unwrap();
    let mut scope = RequestScope::new(&spec);
    inspector.inspect_text(&mut scope, &token(4)).unwrap();
    assert_eq!(
        inspector.inspect_text(&mut scope, &pem()).unwrap_err(),
        CoreBridgeError::Blocked
    );
    // Later clean leaves cannot revive it.
    assert_eq!(
        inspector.inspect_text(&mut scope, "clean").unwrap_err(),
        CoreBridgeError::Incomplete
    );
    assert_eq!(
        scope.finish(b"{}".to_vec()).unwrap_err(),
        CoreBridgeError::Incomplete
    );
}

// ------------------------------------------------------------------ profile and PII

#[tokio::test]
async fn pii_and_credential_profiles_combine_with_pinned_core_evidence() {
    // The pin's PII detection is context-gated (a bare address is not a finding), so each
    // address sits behind a label. `common` has 6 detectors and does not know `ghp_` tokens
    // but does redact bearer credentials; `full` redacts both.
    let email_en = "email: jordan.public@mailhost.net today";
    let email_kr = "이메일 주소: minsu.invented@mailhost.net 입니다";
    let cred = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789".to_owned();
    let all = [cred.clone(), email_en.into(), email_kr.into()];
    for profile in [Profile::Common, Profile::Full] {
        // Credentials only: credentials redacted, invented PII untouched.
        let lab = Lab::new(&policy(profile, &[], OnWarn::Reject));
        let out = contents(&lab.run(&chat(&all)).await.unwrap());
        assert_eq!(
            out,
            ["Authorization: Bearer <SECRET_1>", email_en, email_kr],
            "{profile:?}"
        );

        // Credentials + email selection: both redacted, numbering is request-wide.
        for selectors in [
            &["pii:family:global:email"][..],
            &["pii"][..],
            &["pii:global"][..],
        ] {
            let lab = Lab::new(&policy(profile, selectors, OnWarn::Reject));
            let out = contents(&lab.run(&chat(&all)).await.unwrap());
            assert_eq!(
                out,
                [
                    "Authorization: Bearer <SECRET_1>",
                    "email: <SECRET_2> today",
                    "이메일 주소: <SECRET_3> 입니다"
                ],
                "{profile:?} {selectors:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_pin_has_no_korean_selector_so_kr_is_text_only() {
    // `pii:kr` is rejected by the pinned core (startup test below). Korean text around
    // Latin-script identifiers is handled; Korean national identifiers are not detected by
    // this pin and the gateway adds no detector of its own.
    let lab = Lab::new(&policy(Profile::Full, &["pii"], OnWarn::Reject));
    let kr = "주민등록번호: 900101-1234567 phone: 010-1234-5678 SSN: 078-05-1120";
    let out = lab.run(&chat(&[kr.into()])).await.unwrap();
    assert_eq!(contents(&out), [kr]);
}

// ------------------------------------------------------------------ startup rejection

fn config_for(content: &str) -> String {
    format!(
        r#"{{"schema_version":1,
            "deployment":{{"listener":{{"address":"127.0.0.1:0"}},"upstream":{{"provider":"openai"}}}},
            "content":{content},
            "resources":{{"capacity":{{"receipt":4,"memory_units":8192,
                "inspection":2,"upstream":1,"stream":1}}}}}}"#
    )
}

#[test]
fn unsupported_selectors_actions_and_combinations_reject_at_startup() {
    for bad in [
        r#"{"profile":"full","pii":["pii:kr"]}"#,
        r#"{"profile":"common","pii":["pii:kr:rrn"]}"#,
        r#"{"profile":"full","pii":["pii:family:global:nonexistent"]}"#,
        r#"{"profile":"full","pii":["PII"]}"#,
        r#"{"profile":"strict"}"#,
        r#"{"profile":"FULL"}"#,
        r#"{"profile":"full","on_warn":"redact"}"#,
        r#"{"profile":"full","on_warn":"allow"}"#,
        r#"{"profile":"full","on_block":"redact"}"#,
        r#"{"profile":"full","on_block":"forward"}"#,
        r#"{"profile":"full","actions":{"private_key":"allow"}}"#,
        r#"{"profile":"full","policy":"custom"}"#,
        r#"{"profile":"full","bypass":true}"#,
        r#"{"profile":"full","route":"other"}"#,
        r#"{"profile":"full","upstream":{"provider":"openai"}}"#,
        r#"{"profile":"full","authorization":"x"}"#,
        r#"{"profile":"full","detectors":["custom"]}"#,
    ] {
        let err = config::parse(config_for(bad).as_bytes()).expect_err(bad);
        // Safe diagnostics: a fixed class and a path, never the configured value.
        let shown = format!("{err} {err:?}");
        for needle in ["rrn", "nonexistent", "custom", "strict"] {
            assert!(!shown.contains(needle), "{bad}: {shown}");
        }
    }
}

#[test]
fn supported_combinations_start_and_authority_sections_never_change() {
    let baseline = config::parse(config_for(r#"{"profile":"common"}"#).as_bytes()).unwrap();
    let want_deployment = format!("{:?}", baseline.deployment());
    let want_resources = format!("{:?}", baseline.resources());
    for content in [
        r#"{"profile":"full"}"#,
        r#"{"profile":"full","on_warn":"forward"}"#,
        r#"{"profile":"common","pii":["pii"]}"#,
        r#"{"profile":"full","pii":["pii:family:global:email"],"on_warn":"forward","max_findings":7}"#,
        r#"{"profile":"full","pii":["pii:global","pii:us"],"max_findings":50000}"#,
    ] {
        let plan = config::parse(config_for(content).as_bytes()).expect(content);
        assert_eq!(
            format!("{:?}", plan.deployment()),
            want_deployment,
            "{content}"
        );
        assert_eq!(
            format!("{:?}", plan.resources()),
            want_resources,
            "{content}"
        );
        assert!(plan.deployment().upstream().is_some());
    }
}

// ------------------------------------------------------------------ state and scale

#[tokio::test]
async fn numbering_restarts_per_request_and_is_unique_within_one() {
    let lab = Lab::new(&policy(
        Profile::Full,
        &["pii:family:global:email"],
        OnWarn::Reject,
    ));
    let a = chat(&[token(1), "email: a.invented@mailhost.net".into(), token(2)]);
    let b = chat(&[token(3)]);
    assert_eq!(
        contents(&lab.run(&a).await.unwrap()),
        ["<SECRET_1>", "email: <SECRET_2>", "<SECRET_3>"]
    );
    assert_eq!(contents(&lab.run(&b).await.unwrap()), ["<SECRET_1>"]);
}

#[tokio::test]
async fn many_findings_stay_bounded_and_numbered_without_gaps() {
    let n = 200_u32;
    let lab = Lab::new(&policy(Profile::Full, &[], OnWarn::Reject).with_max_findings(n));
    let text = (0..n).map(token).collect::<Vec<_>>().join(" ");
    let out = lab.run(&chat(&[text])).await.unwrap();
    let got = contents(&out).remove(0);
    let want = (1..=n)
        .map(|i| format!("<SECRET_{i}>"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(got, want);
    // One more finding than the bound rejects the whole request, with no partial output.
    let text = (0..=n).map(token).collect::<Vec<_>>().join(" ");
    assert_eq!(
        lab.run(&chat(&[text])).await.unwrap_err(),
        BoundaryError::Core(CoreBridgeError::LimitExceeded)
    );
}

#[tokio::test]
async fn placeholder_growth_beyond_the_bound_is_rejected_not_truncated() {
    // Short invented addresses grow when replaced by `<SECRET_n>`. With the bound equal to
    // the original body size the grown document cannot fit, so the request is rejected.
    let content = (0..40)
        .map(|i| format!("email: a{i}@b.io"))
        .collect::<Vec<_>>()
        .join(" ");
    let body = chat(&[content]);
    let mut tight = RequestLimits::provisional();
    tight.max_body_bytes = u32::try_from(body.len()).unwrap();
    let content_policy = policy(Profile::Full, &["pii:family:global:email"], OnWarn::Reject);
    let lab = Lab::with(&content_policy, tight);
    let err = lab.run(&body).await.unwrap_err();
    assert!(
        matches!(
            err,
            BoundaryError::OutputLimit | BoundaryError::Core(CoreBridgeError::LimitExceeded)
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn pre_redacted_input_and_escapes_stay_ordinary_text() {
    let lab = Lab::new(&policy(Profile::Full, &[], OnWarn::Reject));
    let raw = r#"{"model":"m","messages":[{"role":"user","content":"<SECRET_7> ghp_SYNTHETICREVOKED00000000000000000009 한글"}]}"#;
    let out = lab.run(raw).await.unwrap();
    assert_eq!(contents(&out), ["<SECRET_7> <SECRET_1> 한글"]);
}

#[tokio::test]
async fn the_common_profile_is_narrower_than_full_for_credentials() {
    // Pinned-core fact the operator must know: `common` does not detect a GitHub-style
    // token that `full` redacts. Choosing `common` is choosing less detection; the gateway
    // does not paper over it.
    let body = chat(&[token(6)]);
    let common = Lab::new(&policy(Profile::Common, &[], OnWarn::Reject));
    assert_eq!(contents(&common.run(&body).await.unwrap()), [token(6)]);
    let full = Lab::new(&policy(Profile::Full, &[], OnWarn::Reject));
    assert_eq!(contents(&full.run(&body).await.unwrap()), ["<SECRET_1>"]);
}
