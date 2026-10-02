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
    cases.compile_fail("tests/ui/fail_forward_received.rs");
    cases.compile_fail("tests/ui/fail_forward_validated.rs");
    cases.compile_fail("tests/ui/fail_forward_bytes.rs");
    cases.compile_fail("tests/ui/fail_forward_json_value.rs");
    cases.compile_fail("tests/ui/fail_construct_sanitized_literal.rs");
    cases.compile_fail("tests/ui/fail_construct_sanitized_new.rs");
    cases.compile_fail("tests/ui/fail_sanitized_clone.rs");
}
