//! Parser conformance table (ADR 0007, ADR 0011).
//!
//! Every JSON parser the gateway uses for request bodies (today the `serde_json`-based
//! baseline, later any borrowed or SIMD-accelerated parser, #18/#19) must give identical
//! accept/reject answers on these cases. Duplicate-key detection must happen on the
//! *decoded* key, so escape forms and literal forms of the same text collide, and
//! decoded Unicode (BMP escapes, surrogate pairs) must be handled identically.
//!
//! Optimized parsers are verified by calling [`run_conformance`] with their entry point.
//! A new rejection rule is added here once, and every parser then has to pass it.
//!
//! Case names are stable identifiers. Case bytes are synthetic and never credentials.

/// Expected outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Accept,
    Reject,
}

/// One conformance case.
pub struct Case {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub expect: Expect,
}

fn case(name: &'static str, bytes: &[u8], expect: Expect) -> Case {
    Case {
        name,
        bytes: bytes.to_vec(),
        expect,
    }
}

/// All cases.
#[must_use]
pub fn cases() -> Vec<Case> {
    use Expect::{Accept, Reject};
    let mut v = vec![
        // Baseline acceptance, so rejections below are caused by the rule under test.
        case("accept_empty_object", b"{}", Accept),
        case(
            "accept_nested",
            br#"{"a":[1,2.5,null,true,{"b":"c"}]}"#,
            Accept,
        ),
        case(
            "accept_same_key_in_sibling_objects",
            br#"{"a":{"k":1},"b":{"k":2}}"#,
            Accept,
        ),
        case(
            "accept_same_key_in_array_elements",
            br#"[{"k":1},{"k":2}]"#,
            Accept,
        ),
        case("accept_key_differing_by_case", br#"{"a":1,"A":2}"#, Accept),
        case(
            "accept_literal_unicode_value",
            "{\"a\":\"\u{e9}\u{1f600}\"}".as_bytes(),
            Accept,
        ),
        case(
            "accept_escaped_unicode_value",
            br#"{"a":"\u00e9\ud83d\ude00"}"#,
            Accept,
        ),
        // Duplicate keys: literal, nested, array-nested, empty key.
        case("reject_duplicate_top_level", br#"{"a":1,"a":2}"#, Reject),
        case("reject_duplicate_nested", br#"{"o":{"k":1,"k":2}}"#, Reject),
        case(
            "reject_duplicate_in_array_element",
            br#"[{"k":1,"k":2}]"#,
            Reject,
        ),
        case("reject_duplicate_empty_key", br#"{"":1,"":2}"#, Reject),
        case(
            "reject_duplicate_with_different_value_types",
            br#"{"a":1,"a":"x"}"#,
            Reject,
        ),
        case(
            "reject_duplicate_non_adjacent",
            br#"{"a":1,"b":2,"a":3}"#,
            Reject,
        ),
        // Duplicate detection after escape decoding.
        case(
            "reject_duplicate_ascii_escape_vs_literal",
            br#"{"a":1,"\u0061":2}"#,
            Reject,
        ),
        case(
            "reject_duplicate_both_escaped",
            br#"{"\u0061":1,"\u0061":2}"#,
            Reject,
        ),
        case(
            "reject_duplicate_short_escape_vs_unicode_escape",
            br#"{"\n":1,"\u000a":2}"#,
            Reject,
        ),
        case(
            "reject_duplicate_slash_escape",
            br#"{"a/b":1,"a\/b":2}"#,
            Reject,
        ),
        case(
            "reject_duplicate_quote_escape",
            br#"{"\"":1,"\u0022":2}"#,
            Reject,
        ),
        // Decoded Unicode: BMP literal vs escape, case of hex digits, surrogate pairs.
        case(
            "reject_duplicate_bmp_literal_vs_escape",
            "{\"\u{e9}\":1,\"\\u00e9\":2}".as_bytes(),
            Reject,
        ),
        case(
            "reject_duplicate_escape_hex_case",
            br#"{"\u00e9":1,"\u00E9":2}"#,
            Reject,
        ),
        case(
            "reject_duplicate_astral_literal_vs_surrogate_pair",
            "{\"\u{1f600}\":1,\"\\ud83d\\ude00\":2}".as_bytes(),
            Reject,
        ),
        case(
            "reject_duplicate_surrogate_pair_hex_case",
            br#"{"\ud83d\ude00":1,"\uD83D\uDE00":2}"#,
            Reject,
        ),
        // Distinct decoded keys are not duplicates (no normalization is performed).
        case(
            "accept_nfc_and_nfd_are_distinct_keys",
            "{\"\u{e9}\":1,\"e\u{301}\":2}".as_bytes(),
            Accept,
        ),
        // Malformed Unicode.
        case("reject_lone_high_surrogate_value", br#""\ud800""#, Reject),
        case("reject_lone_low_surrogate_value", br#""\udc00""#, Reject),
        case("reject_lone_high_surrogate_key", br#"{"\ud800":1}"#, Reject),
        case(
            "reject_reversed_surrogate_pair",
            br#""\ude00\ud83d""#,
            Reject,
        ),
        case("reject_invalid_utf8_value", &[b'"', 0xff, b'"'], Reject),
        case(
            "reject_invalid_utf8_key",
            &[b'{', b'"', 0xc3, b'"', b':', b'1', b'}'],
            Reject,
        ),
        case("reject_overlong_utf8", &[b'"', 0xc0, 0xaf, b'"'], Reject),
        case(
            "reject_truncated_utf8_sequence",
            &[b'"', 0xe2, 0x82, b'"'],
            Reject,
        ),
        case("reject_bad_escape", br#""\x41""#, Reject),
        case("reject_short_unicode_escape", br#""\u12""#, Reject),
        case(
            "reject_raw_control_character_in_string",
            b"\"a\nb\"",
            Reject,
        ),
        // Document structure.
        case("reject_empty_input", b"", Reject),
        case("reject_truncated_object", b"{", Reject),
        case("reject_trailing_bytes", br#"{"a":1} x"#, Reject),
        case("reject_second_document", br#"{"a":1}{"b":2}"#, Reject),
        case("reject_trailing_comma", br#"{"a":1,}"#, Reject),
        case("reject_unquoted_key", br#"{a:1}"#, Reject),
        case("reject_single_quotes", b"{'a':1}", Reject),
        case("reject_nan_literal", br#"{"a":NaN}"#, Reject),
        case(
            "reject_utf8_bom_prefix",
            &[0xef, 0xbb, 0xbf, b'{', b'}'],
            Reject,
        ),
    ];

    // Nesting well beyond any sane limit must be rejected, not crash.
    let depth = 10_000_usize;
    let mut deep = "[".repeat(depth);
    deep.push_str(&"]".repeat(depth));
    v.push(Case {
        name: "reject_excessive_array_nesting",
        bytes: deep.into_bytes(),
        expect: Reject,
    });
    let mut deep_obj = String::new();
    for _ in 0..depth {
        deep_obj.push_str("{\"k\":");
    }
    deep_obj.push('1');
    deep_obj.push_str(&"}".repeat(depth));
    v.push(Case {
        name: "reject_excessive_object_nesting",
        bytes: deep_obj.into_bytes(),
        expect: Reject,
    });
    v
}

/// A case whose observed outcome differs from the table. Names only; never bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub name: &'static str,
    pub expected: Expect,
    pub got: Expect,
}

/// Run every case through `accepts` (true when the parser accepts the document) and
/// return the mismatches. An empty result means the parser conforms.
pub fn run_conformance(mut accepts: impl FnMut(&[u8]) -> bool) -> Vec<Mismatch> {
    cases()
        .into_iter()
        .filter_map(|c| {
            let got = if accepts(&c.bytes) {
                Expect::Accept
            } else {
                Expect::Reject
            };
            (got != c.expect).then_some(Mismatch {
                name: c.name,
                expected: c.expect,
                got,
            })
        })
        .collect()
}
