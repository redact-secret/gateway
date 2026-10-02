//! Shared test-support harness (issue #6; ADR 0002, 0003, 0004, 0007, 0008).
//!
//! Test-only: this directory is compiled solely into integration-test (and the
//! `perf_workloads` example) crates and is never part of the shipped binary or library.
//! All data is synthetic. Include from a test crate root with `mod support;`.
//!
//! | Module | Purpose | Reused by |
//! | --- | --- | --- |
//! | [`fake_upstream`] | loopback fake provider: records calls, scripted failure modes | #7, #18-#25 |
//! | [`raw_http`] | tiny dependency-free HTTP/1.1 client for harness tests | all |
//! | [`leak`] | synthetic canary markers and leakage scanner | all |
//! | [`permits`] | capacity probes, controllable non-interruptible job | #18 |
//! | [`parser_cases`] | parser conformance table every parser must pass | #18, #19 |
//! | [`workloads`] | synthetic payload shapes, timing and RSS measurement | #5, #18 |
//!
//! Test code may unwrap and index freely (CONVENTIONS.md); production lints do not apply.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

pub mod fake_upstream;
pub mod leak;
pub mod parser_cases;
pub mod permits;
pub mod raw_http;
pub mod workloads;
