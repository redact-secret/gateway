//! No-forward and no-leakage assertions for what the skeleton implements (issue #6,
//! ADR 0002, ADR 0003).
//!
//! Scope: only the implemented interfaces, i.e. bounded receipt (`admission`), strict
//! parse and rejection (`protocol::validate`), the sealed transport entry point, and the
//! binary's startup/usage output. There is no listener, route, or forwarding path yet, so
//! full route and stream qualification (#18-#25) is intentionally absent. A fake upstream
//! is running for every case and must observe zero connections, calls, and body bytes;
//! when #18 wires a real upstream URL, these same assertions apply unchanged.

mod support;

use std::num::NonZeroU32;
use std::process::Command;

use redact_secret_gateway::admission::{Admission, AdmissionError, CapacityPlan};
use redact_secret_gateway::protocol::{self, Protocol, ProtocolError};
use support::fake_upstream::{Behavior, FakeUpstream};
use support::leak::{ARG_MARKER, BODY_MARKER, HEADER_MARKER, KEY_MARKER, Markers};
use support::permits::{Capacity, Sizes};

fn sizes() -> Sizes {
    Sizes {
        receipt: 2,
        memory: 4096,
        inspection: 1,
        upstream: 1,
        stream: 1,
    }
}

/// Bodies the skeleton must reject, each carrying synthetic markers that must never
/// appear in any gateway-generated text.
fn rejected_bodies() -> Vec<(&'static str, Vec<u8>, ProtocolError)> {
    vec![
        (
            "duplicate_key_with_marker_key",
            format!(r#"{{"{KEY_MARKER}":1,"{KEY_MARKER}":"{BODY_MARKER}"}}"#).into_bytes(),
            ProtocolError::Malformed,
        ),
        (
            "malformed_json_with_marker",
            format!(r#"{{"content":"{BODY_MARKER}""#).into_bytes(),
            ProtocolError::Malformed,
        ),
        (
            "trailing_bytes_with_marker",
            format!(r#"{{"a":1}} {BODY_MARKER}"#).into_bytes(),
            ProtocolError::Malformed,
        ),
        (
            "invalid_utf8",
            [b"{\"a\":\"", BODY_MARKER.as_bytes(), &[0xff], b"\"}"].concat(),
            ProtocolError::Malformed,
        ),
        (
            "well_formed_but_unsupported_until_classification",
            format!(r#"{{"messages":[{{"role":"user","content":"{BODY_MARKER}"}}]}}"#).into_bytes(),
            ProtocolError::Unsupported,
        ),
    ]
}

#[tokio::test]
async fn rejected_requests_send_zero_upstream_bytes_and_release_capacity() {
    let upstream = FakeUpstream::start(Behavior::ok_json()).await;
    let cap = Capacity::new(sizes());
    let markers = Markers::standard();

    for (name, body, expected) in rejected_bodies() {
        let ticket = cap.admission.begin_receipt(4096).expect("admit");
        let received = ticket.complete(body).expect("fits reservation");
        markers.assert_clean_debug(name, &received);

        let err = protocol::validate(received, Protocol::ChatCompletionsText)
            .expect_err("skeleton must reject");
        assert_eq!(err, expected, "{name}");
        markers.assert_clean_fmt(name, &err);
        markers.assert_clean(name, err.code().as_str().as_bytes());

        // Nothing reached upstream, and the rejection path returned every permit.
        upstream.assert_nothing_sent();
        cap.assert_all_free();
    }
}

#[tokio::test]
async fn oversized_body_is_rejected_before_anything_is_sent() {
    let upstream = FakeUpstream::start(Behavior::ok_json()).await;
    let cap = Capacity::new(sizes());
    let ticket = cap.admission.begin_receipt(8).expect("admit");
    let err = ticket
        .complete(BODY_MARKER.as_bytes().to_vec())
        .expect_err("body exceeds reservation");
    assert_eq!(err, AdmissionError::InvalidReservation);
    Markers::standard().assert_clean_fmt("admission error", &err);
    upstream.assert_nothing_sent();
    cap.assert_all_free();
}

#[tokio::test]
async fn overload_is_a_safe_typed_error_without_upstream_traffic() {
    let upstream = FakeUpstream::start(Behavior::ok_json()).await;
    let one = NonZeroU32::new(1).unwrap();
    let admission = Admission::new(&CapacityPlan::new(one, one, one, one, one));
    let _held = admission.begin_receipt(1).expect("first admitted");
    let err = admission.begin_receipt(1).expect_err("second overloads");
    assert_eq!(err, AdmissionError::Overload);
    assert_eq!(err.code().as_str(), "overload");
    upstream.assert_nothing_sent();
}

/// The binary's own output is gateway-generated text: it must not echo arguments or
/// environment values, whether it succeeds or prints usage. Robust to later CLI growth:
/// asserts on leakage only, not on exit code or exact wording.
#[test]
fn binary_output_never_echoes_arguments_or_environment() {
    let markers = Markers::standard();
    let bin = env!("CARGO_BIN_EXE_redact-secret-gateway");
    let runs: Vec<Vec<&str>> = vec![
        vec![ARG_MARKER],
        vec!["--version", ARG_MARKER],
        vec!["--config", ARG_MARKER],
        vec![],
    ];
    for args in runs {
        let out = Command::new(bin)
            .args(&args)
            .env("SYNTHETIC_CANARY_HEADER", HEADER_MARKER)
            .env("SYNTHETIC_CANARY_BODY", BODY_MARKER)
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .output()
            .expect("run binary");
        markers.assert_clean("binary stdout", &out.stdout);
        markers.assert_clean("binary stderr", &out.stderr);
    }
}

#[test]
fn version_output_is_a_single_line_without_markers() {
    let out = Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
        .arg("--version")
        .env("SYNTHETIC_CANARY_BODY", BODY_MARKER)
        .output()
        .expect("run binary");
    assert!(out.status.success());
    Markers::standard().assert_clean("version stdout", &out.stdout);
    assert_eq!(String::from_utf8(out.stdout).unwrap().lines().count(), 1);
}

mod scanner_controls {
    //! Negative controls: the leakage assertions must fail when a violation exists.
    use super::support::leak::{BODY_MARKER, KEY_MARKER, Markers};

    #[test]
    fn scanner_detects_exact_case_variant_and_truncated_markers() {
        let m = Markers::standard();
        for haystack in [
            format!("error: {BODY_MARKER}"),
            format!("error: {}", BODY_MARKER.to_ascii_lowercase()),
            format!("error: {}", &BODY_MARKER[..16]),
            format!("{{\"k\":\"{KEY_MARKER}\"}}"),
        ] {
            assert!(m.scan_str("planted", &haystack).is_err(), "missed a leak");
        }
        assert!(m.scan_str("clean", "overload").is_ok());
    }

    #[test]
    fn leak_report_does_not_contain_the_marker() {
        let m = Markers::standard();
        let leak = m
            .scan_str("planted output", &format!("x {BODY_MARKER} y"))
            .unwrap_err();
        let shown = format!("{leak} {leak:?} {m:?}");
        assert!(
            m.scan_str("report", &shown).is_ok(),
            "report leaked: {shown}"
        );
        assert_eq!(leak.marker_label, "body");
        assert_eq!(leak.offset, 2);
    }

    #[test]
    #[should_panic(expected = "leaked")]
    fn assert_clean_panics_on_a_leak() {
        Markers::standard().assert_clean("planted", BODY_MARKER.as_bytes());
    }
}
