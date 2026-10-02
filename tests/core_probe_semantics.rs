//! Complete-inspection semantics probe for the exact core pin (issue #5).
//!
//! Every fixture is synthetic: revoked-looking tokens built from fixed filler and invented
//! PII on the reserved `.invalid` domain. Nothing here is a real credential. The probe
//! records what the pinned core does; `docs/probes/core-bridge-probe.md` is the report.

// Test helpers may use expect() and plain arithmetic; production code may not (clippy.toml
// covers only #[test] functions).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic
)]

use redact_secret::{
    Action, ByteRange, Candidate, Confidence, DefaultPolicy, DetectedFinding, Detector,
    DetectorContext, DetectorFailure, DetectorRegistry, Finding, FormatterFailure,
    PlaceholderContext, Policy, PolicyContext, PolicyFailure, SecretScanErrorCode,
    WholeInputLimits, default_placeholder_formatter, redact, scan, scan_and_redact_with_limits,
};
use redact_secret_gateway::core_bridge::{
    CoreBridgeError, Inspector, InspectorSpec, PINNED_CORE_VERSION, RequestScope, map_core_error,
};

/// Synthetic revoked-style token (the core's own documentation fixture shape).
fn token(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

fn spec(profile: &str, max_bytes: usize, max_findings: usize) -> InspectorSpec {
    InspectorSpec::new(profile, &[], max_bytes, max_findings).expect("spec")
}

fn inspector(spec: &InspectorSpec) -> Inspector {
    Inspector::new(spec).expect("inspector")
}

// ---------------------------------------------------------------- success and proof

#[test]
fn success_redacts_and_mints_the_proof() {
    let spec = spec("full", 4096, 100);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let input = format!("API_KEY={} trailing", token(1));
    let out = inspector
        .inspect_text(&mut scope, &input)
        .expect("complete");
    assert_eq!(out.as_str(), "API_KEY=<SECRET_1> trailing");
    assert!(!out.as_str().contains("SYNTHETICREVOKED"));
    let proof = scope.finish(out.into_string().into_bytes()).expect("proof");
    let summary = proof.summary();
    assert_eq!(summary.leaves, 1);
    assert_eq!(summary.redactions, 1);
    assert_eq!(summary.unredacted_findings, 0);
    assert_eq!(summary.input_bytes, input.len());
    assert!(!format!("{proof:?}").contains("API_KEY"));
}

#[test]
fn clean_text_is_complete_with_no_findings() {
    let spec = spec("full", 4096, 100);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let out = inspector
        .inspect_text(&mut scope, "plain synthetic prose, nothing here")
        .expect("complete");
    assert_eq!(out.as_str(), "plain synthetic prose, nothing here");
    assert_eq!(scope.summary().redactions, 0);
    assert!(scope.finish(Vec::new()).is_ok());
}

#[test]
fn common_profile_is_a_strict_subset_and_does_not_cover_github_tokens() {
    // Probe finding: the `common` profile (6 detectors in the pinned core) has no GitHub token
    // detector, so a token the `full` profile redacts passes through `common` unchanged. This
    // is a profile-choice fact for #4/#19, not an incomplete inspection: `Ok` means every
    // registered detector of the chosen profile ran.
    let common = spec("common", 4096, 100);
    let mut scope = RequestScope::new(&common);
    let input = format!("k={}", token(2));
    let out = inspector(&common)
        .inspect_text(&mut scope, &input)
        .expect("complete under common");
    assert_eq!(out.as_str(), input);
    assert!(
        inspector(&common)
            .activation_identity()
            .contains("credentials=common")
    );

    let full = spec("full", 4096, 100);
    let mut scope = RequestScope::new(&full);
    let out = inspector(&full)
        .inspect_text(&mut scope, &input)
        .expect("full");
    assert_eq!(out.as_str(), "k=<SECRET_1>");
}

#[test]
fn pii_selector_activates_and_is_reported_in_identity() {
    let spec = InspectorSpec::new("full", &["pii:family:global:email"], 4096, 100).expect("spec");
    let inspector = inspector(&spec);
    assert!(inspector.activation_identity().contains("pii:global:email"));
    let mut scope = RequestScope::new(&spec);
    let out = inspector
        .inspect_text(&mut scope, "email: jordan.public@mailhost.net")
        .expect("complete");
    assert!(!out.as_str().contains("jordan.public"));
}

// ---------------------------------------------------------------- profiles

#[test]
fn malformed_and_unsupported_profiles_are_typed_failures() {
    for name in ["", "FULL", "Full", "nope", "full ", "pii:kr", "full\0"] {
        assert_eq!(
            InspectorSpec::new(name, &[], 10, 10).unwrap_err(),
            CoreBridgeError::UnsupportedProfile,
            "profile {name:?}"
        );
    }
}

#[test]
fn unsupported_and_invalid_pii_selectors_fail_closed() {
    // The pinned core supports only `us` among national selectors: `pii:kr` is rejected.
    for selector in ["pii:kr", "BAD", "pii:family:global:nonexistent", "pii:"] {
        let err = InspectorSpec::new("full", &[selector], 10, 10).unwrap_err();
        assert_eq!(err, CoreBridgeError::UnsupportedProfile, "{selector:?}");
    }
    let raw = redact_secret::PiiSelection::parse(&["pii:kr"]).unwrap_err();
    assert_eq!(raw.code(), SecretScanErrorCode::PiiSelectorUnsupported);
    let raw = redact_secret::PiiSelection::parse(&["BAD"]).unwrap_err();
    assert_eq!(raw.code(), SecretScanErrorCode::PiiSelectorInvalid);
}

#[test]
fn zero_limits_are_an_invalid_configuration() {
    assert_eq!(
        InspectorSpec::new("full", &[], 0, 10).unwrap_err(),
        CoreBridgeError::InvalidConfiguration
    );
    assert_eq!(
        InspectorSpec::new("full", &[], 10, 0).unwrap_err(),
        CoreBridgeError::InvalidConfiguration
    );
}

// ---------------------------------------------------------------- limits, never truncation

#[test]
fn input_at_limit_is_inspected_fully_and_one_over_is_an_error() {
    let body = token(3);
    let at = body.len();
    let spec_at = spec("full", at, 10);
    let mut scope = RequestScope::new(&spec_at);
    let out = inspector(&spec_at)
        .inspect_text(&mut scope, &body)
        .expect("exactly at the limit");
    assert_eq!(out.as_str(), "<SECRET_1>");

    let spec_under = spec("full", at - 1, 10);
    let mut scope = RequestScope::new(&spec_under);
    let err = inspector(&spec_under)
        .inspect_text(&mut scope, &body)
        .unwrap_err();
    assert_eq!(err, CoreBridgeError::LimitExceeded);
    // No partial result and no proof after a failure.
    assert!(scope.is_poisoned());
    assert_eq!(
        scope.finish(Vec::new()).unwrap_err(),
        CoreBridgeError::Incomplete
    );
}

#[test]
fn core_limit_errors_never_return_partial_output() {
    let registry = DetectorRegistry::with_built_in([]).expect("registry");
    let input = format!("{} {}", token(4), token(5));
    // Byte limit: the call fails; there is no truncated Ok.
    let limits = WholeInputLimits::new(10, 10).expect("limits");
    let err = scan_and_redact_with_limits(
        &input,
        &registry,
        &DefaultPolicy,
        &default_placeholder_formatter,
        &limits,
    )
    .unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::InputLimitExceeded);
    assert_eq!(map_core_error(&err), CoreBridgeError::LimitExceeded);
    // Finding limit: two findings against a limit of one is an error, not one redaction.
    let limits = WholeInputLimits::new(4096, 1).expect("limits");
    let err = scan_and_redact_with_limits(
        &input,
        &registry,
        &DefaultPolicy,
        &default_placeholder_formatter,
        &limits,
    )
    .unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::FindingLimitExceeded);
    assert_eq!(map_core_error(&err), CoreBridgeError::LimitExceeded);
    // At the limit it succeeds with every finding redacted.
    let limits = WholeInputLimits::new(4096, 2).expect("limits");
    let ok = scan_and_redact_with_limits(
        &input,
        &registry,
        &DefaultPolicy,
        &default_placeholder_formatter,
        &limits,
    )
    .expect("at the limit");
    assert_eq!(ok.text(), "<SECRET_1> <SECRET_2>");
    assert_eq!(ok.findings().len(), 2);
}

#[test]
fn limits_are_request_wide_across_leaves() {
    let one = token(6);
    let spec = spec("full", one.len() * 2 - 1, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    assert!(inspector.inspect_text(&mut scope, &one).is_ok());
    // Each leaf alone fits; the request total does not.
    assert_eq!(
        inspector.inspect_text(&mut scope, &one).unwrap_err(),
        CoreBridgeError::LimitExceeded
    );
    assert!(scope.is_poisoned());

    let spec = spec_with_findings();
    let inspector = inspector_for(&spec);
    let mut scope = RequestScope::new(&spec);
    assert!(inspector.inspect_text(&mut scope, &token(7)).is_ok());
    assert!(inspector.inspect_text(&mut scope, &token(8)).is_ok());
    assert_eq!(
        inspector.inspect_text(&mut scope, &token(9)).unwrap_err(),
        CoreBridgeError::LimitExceeded
    );
}

fn spec_with_findings() -> InspectorSpec {
    spec("full", 1 << 20, 2)
}

fn inspector_for(spec: &InspectorSpec) -> Inspector {
    inspector(spec)
}

#[test]
fn a_poisoned_scope_refuses_further_leaves() {
    let spec = spec("full", 4096, 1);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let two = format!("{} {}", token(10), token(11));
    assert!(inspector.inspect_text(&mut scope, &two).is_err());
    assert_eq!(
        inspector.inspect_text(&mut scope, "harmless").unwrap_err(),
        CoreBridgeError::Incomplete
    );
}

// ---------------------------------------------------------------- policy decisions

#[test]
fn block_action_is_a_distinct_rejection() {
    // Synthetic PEM-shaped block with fixed filler; not key material.
    let pem = "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\n-----END PRIVATE KEY-----";
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let err = inspector.inspect_text(&mut scope, pem).unwrap_err();
    assert_eq!(err, CoreBridgeError::Blocked);
    assert!(scope.is_poisoned());
}

// ---------------------------------------------------------------- detector, policy, placeholder failures

struct FailingDetector;

impl Detector for FailingDetector {
    fn id(&self) -> &str {
        "synthetic-failing"
    }
    fn detect(&self, _: &str, _: &DetectorContext) -> Result<Vec<Candidate>, DetectorFailure> {
        Err(DetectorFailure)
    }
}

struct BadRangeDetector;

impl Detector for BadRangeDetector {
    fn id(&self) -> &str {
        "synthetic-bad-range"
    }
    fn detect(&self, input: &str, _: &DetectorContext) -> Result<Vec<Candidate>, DetectorFailure> {
        // Range past the end of the scan copy.
        let range = ByteRange::new(0, input.len() + 5).ok_or(DetectorFailure)?;
        Ok(vec![Candidate::new(
            "synthetic-type",
            Confidence::High,
            range,
        )])
    }
}

struct FailingPolicy;

impl Policy for FailingPolicy {
    fn evaluate(&self, _: &DetectedFinding, _: &PolicyContext) -> Result<Action, PolicyFailure> {
        Err(PolicyFailure)
    }
}

fn code_of<T>(result: Result<T, redact_secret::SecretScanError>) -> SecretScanErrorCode {
    result.err().expect("an error was expected").code()
}

#[test]
fn every_reachable_failure_is_an_err_and_maps_to_a_gateway_failure() {
    let input = format!("k={} text", token(12));

    // Detector failure: whole scan fails, no partial findings.
    let registry =
        DetectorRegistry::with_built_in([Box::new(FailingDetector) as Box<dyn Detector>])
            .expect("registry");
    let err = scan(&input, &registry, &DefaultPolicy).unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::DetectorFailure);
    assert_eq!(map_core_error(&err), CoreBridgeError::Incomplete);

    // Invalid candidate from a custom detector.
    let registry =
        DetectorRegistry::with_built_in([Box::new(BadRangeDetector) as Box<dyn Detector>])
            .expect("registry");
    let code = code_of(scan(&input, &registry, &DefaultPolicy));
    assert_eq!(code, SecretScanErrorCode::InvalidCandidate);

    // Policy failure.
    let registry = DetectorRegistry::with_built_in([]).expect("registry");
    let err = scan(&input, &registry, &FailingPolicy).unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::PolicyFailure);
    assert_eq!(map_core_error(&err), CoreBridgeError::Incomplete);

    // Placeholder failure.
    let failing = |_: &Finding, _: &PlaceholderContext| -> Result<String, FormatterFailure> {
        Err(FormatterFailure)
    };
    let findings = scan(&input, &registry, &DefaultPolicy).expect("scan");
    let err = redact(&input, &findings, &failing).unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::PlaceholderFailure);
    assert_eq!(map_core_error(&err), CoreBridgeError::Incomplete);

    // Empty, oversized, and value-reproducing placeholders are rejected, not applied.
    let empty = |_: &Finding, _: &PlaceholderContext| Ok(String::new());
    assert_eq!(
        code_of(redact(&input, &findings, &empty)),
        SecretScanErrorCode::InvalidPlaceholder
    );
    let long = |_: &Finding, _: &PlaceholderContext| {
        Ok("X".repeat(redact_secret::MAX_PLACEHOLDER_LENGTH + 1))
    };
    assert_eq!(
        code_of(redact(&input, &findings, &long)),
        SecretScanErrorCode::InvalidPlaceholder
    );
    let echo_value = token(12);
    let reproduces = move |_: &Finding, _: &PlaceholderContext| Ok(echo_value.clone());
    let err = redact(&input, &findings, &reproduces).unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::InvalidPlaceholder);
    assert!(!err.to_string().contains("SYNTHETIC"), "no payload echo");
    assert_eq!(map_core_error(&err), CoreBridgeError::Incomplete);

    // Findings the caller hand-built outside the input are rejected.
    let bogus = Finding::new(
        "finding-1",
        "synthetic-type",
        "synthetic-detector",
        Confidence::High,
        Action::Redact,
        ByteRange::new(0, input.len() + 9).expect("range"),
    )
    .expect("finding");
    let code = code_of(redact(&input, &[bogus], &default_placeholder_formatter));
    assert_eq!(code, SecretScanErrorCode::InvalidFindings);

    // Zero limits.
    let err = WholeInputLimits::new(0, 1).unwrap_err();
    assert_eq!(err.code(), SecretScanErrorCode::InvalidLimits);
    assert_eq!(map_core_error(&err), CoreBridgeError::InvalidConfiguration);
}

#[test]
fn unknown_core_codes_fail_closed_by_construction() {
    // Every code the mapping does not name, including incremental-only and future ones, is
    // Incomplete. Spot-check the ones whose names suggest success-like conditions.
    for code in [
        SecretScanErrorCode::InvalidInput,
        SecretScanErrorCode::InvalidState,
        SecretScanErrorCode::InvalidRuleset,
        SecretScanErrorCode::InvalidPolicyAction,
        SecretScanErrorCode::InvalidFindings,
        SecretScanErrorCode::InvalidDetector,
        SecretScanErrorCode::InvalidPlaceholder,
        SecretScanErrorCode::InvalidCandidate,
    ] {
        assert_eq!(
            map_core_error(&code.into()),
            CoreBridgeError::Incomplete,
            "{code:?}"
        );
    }
}

#[test]
fn error_display_never_echoes_input() {
    let spec = spec("full", 8, 1);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let secret_like = token(13);
    let err = inspector
        .inspect_text(&mut scope, &secret_like)
        .unwrap_err();
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains("SYNTHETIC"));
}

// ---------------------------------------------------------------- state scope

#[test]
fn core_numbering_restarts_per_call_and_the_scope_makes_it_request_wide() {
    // The raw core restarts at 1 on every call: two leaves, same placeholder, different secrets.
    let registry = DetectorRegistry::with_built_in([]).expect("registry");
    let call = |text: &str| {
        scan_and_redact_with_limits(
            text,
            &registry,
            &DefaultPolicy,
            &default_placeholder_formatter,
            &WholeInputLimits::default(),
        )
        .expect("ok")
        .text()
        .to_owned()
    };
    assert_eq!(call(&token(20)), "<SECRET_1>");
    assert_eq!(call(&token(21)), "<SECRET_1>");

    // The bridge scope offsets by replacements already made in this request.
    let spec = spec("full", 4096, 100);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let a = inspector.inspect_text(&mut scope, &token(20)).expect("a");
    let b = inspector
        .inspect_text(&mut scope, &format!("{} and {}", token(21), token(22)))
        .expect("b");
    assert_eq!(a.as_str(), "<SECRET_1>");
    assert_eq!(b.as_str(), "<SECRET_2> and <SECRET_3>");

    // A second request is isolated: it restarts at 1 and sees none of the first's counts.
    let mut other = RequestScope::new(&spec);
    let c = inspector.inspect_text(&mut other, &token(23)).expect("c");
    assert_eq!(c.as_str(), "<SECRET_1>");
    assert_eq!(other.summary().redactions, 1);
}

// ---------------------------------------------------------------- deterministic traversal

/// Reference traversal for the probe: depth-first, arrays in index order, objects in the key
/// order of the gateway's JSON model (`serde_json` default map: lexicographic by key bytes),
/// string values only. Keys are structural and are not inspected.
fn leaves<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(s) => out.push(s),
        serde_json::Value::Array(items) => items.iter().for_each(|v| leaves(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| leaves(v, out)),
        _ => {}
    }
}

fn run_request(json: &str) -> (Vec<String>, usize) {
    let value: serde_json::Value = serde_json::from_str(json).expect("json");
    let mut texts = Vec::new();
    leaves(&value, &mut texts);
    let spec = spec("full", 1 << 20, 100);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let out: Vec<String> = texts
        .iter()
        .map(|t| {
            inspector
                .inspect_text(&mut scope, t)
                .expect("complete")
                .into_string()
        })
        .collect();
    (out, scope.summary().redactions)
}

#[test]
fn traversal_order_and_numbering_are_deterministic_and_key_order_independent() {
    let a = format!(
        r#"{{"b":"{}","a":["x","{}"],"c":{{"z":"{}","y":"plain"}}}}"#,
        token(30),
        token(31),
        token(32)
    );
    // Same document with members written in another order.
    let b = format!(
        r#"{{"c":{{"y":"plain","z":"{}"}},"a":["x","{}"],"b":"{}"}}"#,
        token(32),
        token(31),
        token(30)
    );
    let (first, n) = run_request(&a);
    assert_eq!(n, 3);
    // Sorted keys: a[0]="x", a[1]=token31, b=token30, c.y="plain", c.z=token32.
    assert_eq!(
        first,
        ["x", "<SECRET_1>", "<SECRET_2>", "plain", "<SECRET_3>"]
    );
    assert_eq!(run_request(&a), run_request(&a));
    assert_eq!(run_request(&b).0, first);
}

// ---------------------------------------------------------------- Unicode and decoded text

#[test]
fn json_escapes_are_inspected_only_after_decoding() {
    // A `\u` escape inside the token in the JSON source: the raw source text is not a token.
    let backslash = '\\';
    let hidden = format!(r#"{{"m":"k=ghp_SYNTH{backslash}u0045TICREVOKED00000000000000000099"}}"#);
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);

    // Inspecting the raw serialized JSON misses it. This is why the gateway must decode first
    // and never replace text in raw JSON (field-classification contract).
    let mut raw_scope = RequestScope::new(&spec);
    let raw = inspector.inspect_text(&mut raw_scope, &hidden).expect("ok");
    assert!(!raw.as_str().contains("SYNTHETIC"));
    assert_eq!(raw.as_str(), hidden);
    assert_eq!(raw_scope.summary().redactions, 0);

    // Decoded, the same leaf is found and redacted.
    let value: serde_json::Value = serde_json::from_str(&hidden).expect("json");
    let decoded = value["m"].as_str().expect("string");
    let mut scope = RequestScope::new(&spec);
    let out = inspector.inspect_text(&mut scope, decoded).expect("ok");
    assert_eq!(out.as_str(), "k=<SECRET_1>");
}

#[test]
fn invisible_code_points_cannot_split_a_token_and_text_is_preserved() {
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    // Zero-width space inside the synthetic token, surrounded by Korean text.
    let split = "안녕 k=ghp_SYNTHETICREVOKED\u{200B}00000000000000000040 끝";
    let out = inspector.inspect_text(&mut scope, split).expect("ok");
    assert_eq!(out.as_str(), "안녕 k=<SECRET_1> 끝");
}

#[test]
fn multibyte_and_normalized_text_without_findings_is_returned_unchanged() {
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    // NFC and NFD Hangul, emoji, and combining marks are returned byte-for-byte.
    let nfc = "한글 테스트 \u{1F600}";
    let nfd = "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF} e\u{0301}";
    assert_eq!(
        inspector
            .inspect_text(&mut scope, nfc)
            .expect("ok")
            .as_str(),
        nfc
    );
    assert_eq!(
        inspector
            .inspect_text(&mut scope, nfd)
            .expect("ok")
            .as_str(),
        nfd
    );
}

#[test]
fn offsets_are_byte_offsets_into_the_original_multibyte_input() {
    let registry = DetectorRegistry::with_built_in([]).expect("registry");
    let prefix = "한글 ";
    let input = format!("{prefix}k={}", token(41));
    let findings = scan(&input, &registry, &DefaultPolicy).expect("scan");
    let range = findings.first().expect("finding").range();
    assert_eq!(&input[range.start()..range.end()], token(41));
    assert_eq!(range.start(), prefix.len() + 2);
}

// ---------------------------------------------------------------- pre-redacted input

#[test]
fn pre_redacted_input_is_not_a_finding_and_double_redaction_is_idempotent() {
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let first = inspector
        .inspect_text(&mut scope, &format!("k={}", token(50)))
        .expect("first")
        .into_string();
    assert_eq!(first, "k=<SECRET_1>");

    let mut again = RequestScope::new(&spec);
    let second = inspector.inspect_text(&mut again, &first).expect("second");
    assert_eq!(second.as_str(), first);
    assert_eq!(again.summary().redactions, 0);
    assert_eq!(again.summary().unredacted_findings, 0);
}

#[test]
fn client_supplied_placeholders_are_kept_and_never_mistaken_for_proof() {
    // A client may send text that already looks redacted. It is ordinary text: it is inspected
    // like any other, a real token beside it is still redacted, and the literal stays as is.
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let input = format!("<SECRET_1> already scanned by client {}", token(51));
    let out = inspector.inspect_text(&mut scope, &input).expect("ok");
    assert_eq!(
        out.as_str(),
        "<SECRET_1> already scanned by client <SECRET_1>"
    );
    // Known ambiguity, recorded in the probe report: a literal client placeholder and a
    // generated one can be equal strings. Security is unaffected (nothing is trusted); the
    // numbering is not a unique mapping back to values.
    assert_eq!(scope.summary().redactions, 1);
}

// ---------------------------------------------------------------- thread-safety tripwires

trait AmbiguousIfSend<A> {
    fn probe() {}
}
impl<T: ?Sized> AmbiguousIfSend<()> for T {}
impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn spec_is_shareable_and_the_registry_owner_is_not() {
    assert_send_sync::<InspectorSpec>();
    // Compiles only while `Inspector` (the core registry owner) is `!Send`. If a core upgrade
    // makes it `Send`, this line becomes ambiguous and the build fails: revisit ADR 0004/0006.
    <Inspector as AmbiguousIfSend<_>>::probe();
    // Proof types and results that cross threads must be `Send`.
    assert_send_sync::<redact_secret::ScanResult>();
}

// ---------------------------------------------------------------- no client "scanned" claim

#[test]
fn nothing_in_the_gateway_reads_a_client_scanned_claim() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty());
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_lowercase();
        for needle in ["scanned", "x-redact", "already-sanitized", "x-sanitized"] {
            assert!(
                !code.contains(needle),
                "{} mentions {needle}",
                path.display()
            );
        }
    }
}

#[test]
fn pin_constant_matches_the_linked_core() {
    assert_eq!(PINNED_CORE_VERSION, redact_secret::VERSION);
}

#[test]
fn warn_findings_stay_in_the_text_and_are_counted_not_hidden() {
    // Probe finding: the core's default policy leaves a medium-confidence contextual finding
    // (`Warn`) in the output. `Ok` therefore means "inspected completely", not "nothing
    // sensitive remains". The bridge surfaces the count; whether Warn must reject is a
    // policy decision recorded as an open item for #19 in the probe report.
    let spec = spec("full", 4096, 10);
    let inspector = inspector(&spec);
    let mut scope = RequestScope::new(&spec);
    let out = inspector
        .inspect_text(&mut scope, "password=hunter2xyz")
        .expect("complete");
    assert_eq!(out.as_str(), "password=hunter2xyz");
    assert_eq!(scope.summary().redactions, 0);
    assert_eq!(scope.summary().unredacted_findings, 1);
}
