//! Parser conformance (ADR 0007, ADR 0011; issue #6).
//!
//! Runs the shared case table (`support::parser_cases`) against the current baseline,
//! `protocol::json::parse_strict`, and through the receipt path (`protocol::validate`).
//! Any optimized, borrowed, or SIMD parser added later (#18/#19, gated on #5
//! measurements) must be wired into `run_conformance` here and pass the identical table:
//! a faster path may not accept anything the baseline rejects.

mod support;

use redact_secret_gateway::admission::Admission;
use redact_secret_gateway::protocol::json::parse_strict;
use redact_secret_gateway::protocol::{self, Protocol, ProtocolError};
use support::leak::Markers;
use support::parser_cases::{Expect, cases, run_conformance};

#[test]
fn baseline_parser_conforms_to_the_shared_table() {
    let mismatches = run_conformance(|bytes| parse_strict(bytes).is_ok());
    assert!(mismatches.is_empty(), "parser mismatches: {mismatches:?}");
}

#[test]
fn receipt_path_agrees_with_the_parser_on_every_case() {
    use redact_secret_gateway::admission::CapacityPlan;
    use std::num::NonZeroU32;

    let one = NonZeroU32::new(1).unwrap();
    let big = NonZeroU32::new(1 << 20).unwrap();
    let admission = Admission::new(&CapacityPlan::new(one, big, one, one, one));

    for case in cases() {
        let units = u32::try_from(case.bytes.len().max(1)).unwrap();
        let received = admission
            .begin_receipt(units)
            .unwrap()
            .complete(case.bytes.clone())
            .unwrap();
        let outcome = protocol::validate(received, Protocol::ChatCompletionsText);
        // Rejected by the parser => Malformed. Accepted by the parser => reaches the
        // skeleton's Unsupported stage. Nothing is ever accepted until #18/#19.
        let expected = match case.expect {
            Expect::Reject => ProtocolError::Malformed,
            Expect::Accept => ProtocolError::Unsupported,
        };
        assert_eq!(outcome.unwrap_err(), expected, "case {}", case.name);
    }
}

#[test]
fn the_harness_detects_a_non_conforming_parser() {
    // Negative controls: a parser that ignores duplicate keys, or that accepts
    // everything, must be reported with the names of the cases it gets wrong.
    let lenient =
        run_conformance(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).is_ok());
    let names: Vec<_> = lenient.iter().map(|m| m.name).collect();
    assert!(names.contains(&"reject_duplicate_top_level"));
    assert!(names.contains(&"reject_duplicate_ascii_escape_vs_literal"));

    let accept_all = run_conformance(|_| true);
    assert!(accept_all.len() > 20);
    let reject_all = run_conformance(|_| false);
    assert!(reject_all.iter().all(|m| m.expected == Expect::Accept));
}

#[test]
fn table_covers_duplicate_keys_and_decoded_unicode() {
    let names: Vec<_> = cases().iter().map(|c| c.name).collect();
    for required in [
        "reject_duplicate_top_level",
        "reject_duplicate_nested",
        "reject_duplicate_ascii_escape_vs_literal",
        "reject_duplicate_bmp_literal_vs_escape",
        "reject_duplicate_astral_literal_vs_surrogate_pair",
        "reject_lone_high_surrogate_value",
        "reject_invalid_utf8_value",
        "reject_excessive_array_nesting",
    ] {
        assert!(names.contains(&required), "missing case {required}");
    }
    // Case names are unique identifiers.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len());
}

#[test]
fn parse_failures_never_echo_input_text() {
    let markers = Markers::standard();
    for (name, body) in [
        (
            "dup",
            format!(r#"{{"{k}":1,"{k}":2}}"#, k = support::leak::KEY_MARKER),
        ),
        ("bad", format!(r#"{{"v":"{}""#, support::leak::BODY_MARKER)),
    ] {
        let err = parse_strict(body.as_bytes()).unwrap_err();
        markers.assert_clean_fmt(name, &err);
    }
}
