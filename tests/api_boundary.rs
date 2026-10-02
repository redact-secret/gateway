//! API-boundary tests (ADR 0002, request-state contract).
//!
//! `trybuild` compiles each `tests/ui/*.rs` file as a separate crate that depends on this
//! package from the outside. `pass_*` files must compile (so the failures below are
//! caused by the type rules, not by a broken harness). `fail_*` files must NOT compile,
//! and their diagnostics are pinned in the matching `.stderr` files. Diagnostics depend on
//! the compiler, which is pinned by `rust-toolchain.toml`; regenerate with
//! `TRYBUILD=overwrite cargo test --locked --test api_boundary` and review the diff.

#[test]
fn unvalidated_types_cannot_reach_forwarding() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/pass_forward_accepts_sanitized.rs");
    cases.pass("tests/ui/pass_forward_stream_accepts_sanitized.rs");
    cases.compile_fail("tests/ui/fail_forward_stream_bytes.rs");
    cases.compile_fail("tests/ui/fail_forward_received.rs");
    cases.compile_fail("tests/ui/fail_forward_validated.rs");
    cases.compile_fail("tests/ui/fail_forward_bytes.rs");
    cases.compile_fail("tests/ui/fail_forward_json_value.rs");
    cases.compile_fail("tests/ui/fail_construct_sanitized_literal.rs");
    cases.compile_fail("tests/ui/fail_construct_sanitized_new.rs");
    cases.compile_fail("tests/ui/fail_sanitized_clone.rs");
    cases.compile_fail("tests/ui/fail_sanitized_default.rs");
    cases.compile_fail("tests/ui/fail_mutate_sanitized_body.rs");
    cases.compile_fail("tests/ui/fail_mutate_sanitized_slice.rs");
    cases.compile_fail("tests/ui/fail_construct_complete_inspection.rs");
    cases.compile_fail("tests/ui/fail_approve_raw_output.rs");
    // #25: forged forwarding, forged or duplicated capacity proofs, copyable credentials,
    // and serializable diagnostics.
    cases.compile_fail("tests/ui/fail_forward_stream_validated.rs");
    cases.compile_fail("tests/ui/fail_construct_validated_request.rs");
    cases.compile_fail("tests/ui/fail_construct_permit.rs");
    cases.compile_fail("tests/ui/fail_clone_permit.rs");
    cases.compile_fail("tests/ui/fail_credential_is_not_copyable_or_comparable.rs");
    cases.compile_fail("tests/ui/fail_serialize_error_types.rs");
}
