# ADR 0011: Toolchain, dependency, and core pin selection

Status: Accepted (design); implemented by the scaffold in #2. Implementation status: implemented (scaffold). Date: 2026-10-02. Resolves the "transport stack versions are chosen in the #2 dependency ADR" deferral in [ADR 0001](0001-repository-and-ownership.md). Core capability verification remains with #5.

## Context

Issue #2 asks for a pinned toolchain, MSRV, async/transport/JSON versions with minimal features, and an exact core pin with provenance. ADR 0001 requires `Cargo.lock` committed, no floating core dependency, and one private binary crate. ADR 0005 fixes the module set. ADR 0007 requires a duplicate-key-rejecting JSON path and defers any optimized parser to measurement.

## Decision

### Crate shape

One package `redact-secret-gateway`, `publish = false`, edition 2024. It has a binary target (`redact-secret-gateway`) and a `[lib]` target (`redact_secret_gateway`). The library target exists only so the binary and the `trybuild` API-boundary tests (which compile as external crates) share one crate. It is private, not published, and not a supported API or plugin surface. Modules: `config`, `admission`, `protocol`, `boundary`, `core_bridge`, `transport`, `health`, `telemetry`. No Cargo workspace section exists; a workspace is added only if a second crate is approved by ADR.

### Toolchain and MSRV

| Item | Value | Reason |
| --- | --- | --- |
| `rust-toolchain.toml` channel | `1.98.1` (exact, `minimal` profile + `rustfmt`, `clippy`) | Exact, reproducible compiler for builds, lints, and pinned `trybuild` diagnostics. Not `stable`, so a new stable release cannot change lint or diagnostic output without a reviewed bump. |
| `rust-version` (MSRV) | `1.88` | Floor set by the core pin (`rust-version = 1.88`, edition 2024) and `trybuild` 1.0.121. Verified: `cargo +1.88 check --locked --lib --bins` passes. |

The gateway does not inherit core's workspace or toolchain file; it matches core only at the MSRV floor. MSRV is a compatibility floor, not a CI matrix promise; CI (#6) builds with the pinned toolchain and may add a 1.88 `check` job.

### Dependencies (resolved in the committed `Cargo.lock`)

| Crate | Requirement | Locked | Features (default features off) | Why |
| --- | --- | --- | --- | --- |
| `redact-secret` | `=0.1.0-beta.12` | 0.1.0-beta.12 | none (core has no features) | Core pin, see below |
| `tokio` | `1` | 1.53.1 | `sync`, `net`, `rt`, `signal`, `macros` (#4: listener, current-thread runtime, SIGINT/SIGTERM; dev adds `io-util`, `time`; adds `signal-hook-registry` and `errno` to the lock) | `admission` uses `tokio::sync::Semaphore` as the permit primitive. Axum and hyper additionally enable `net`, `rt`, `time`, `macros`, `io-util` in the normal graph by feature unification. |
| `axum` | `0.8` | 0.8.9 | `http1`, `tokio` | HTTP server and routing for `health` and later routes. No `json`, `query`, `form`, `multipart`, `ws`, `http2`, or `tower-log`. |
| `reqwest` | `0.13` | 0.13.5 | `rustls` | Single upstream client, held only by `transport`. `rustls` uses the platform certificate verifier, so server certificates are verified. Excluded: `system-proxy` (no proxy inherited from the environment), `charset`, `http2`, `json`, `cookies`, compression, `native-tls`, `socks`. Client built with redirects disabled and `no_proxy()`. |
| `serde` | `1` | 1.0.229 | `std` | Visitor traits for the strict JSON path. No `derive`. |
| `serde_json` | `1` | 1.0.151 | `std` | Tokenizer for the strict JSON path. No `arbitrary_precision`, `preserve_order`, `raw_value`, or `unbounded_depth` (so its 128-level recursion limit stays on). |
| `trybuild` (dev) | `1` | 1.0.121 | defaults | Compile-fail API-boundary tests (ADR 0002). Chosen over `compile_fail` doc-tests because it pins diagnostics and compiles tests as external crates against the library target. |

Versions follow the semver ranges recorded in `Cargo.toml`; `Cargo.lock` is committed and every command uses `--locked`. Pre-1.0 compatible ranges (`axum 0.8`, `reqwest 0.13`) never float across minor versions. The rest of the graph is whatever the lockfile records; upgrades are reviewed lockfile diffs. `cargo deny check` passes with the policy in `deny.toml` (advisories, bans, licenses, sources).

### JSON path (ADR 0007)

Selected candidate: `serde_json`'s tokenizer driven by a custom `serde::de::Visitor` that builds an owned tree (`protocol::json::Json`) and rejects duplicate object keys after escape decoding (`"a"` and `"a"` collide) at every nesting level. `serde_json` already rejects malformed JSON, trailing bytes, invalid UTF-8, lone surrogate escapes, and nesting beyond 128 levels. The scaffold implements this baseline (`protocol::json::parse_strict`) with tests, because it is small and fixes the rejection semantics early. It is not claimed fast. Node, string, and output budgets, memory accounting for the parsed structure, and any borrowed or SIMD parser remain #18/#19 work gated on #5 measurements; any replacement must pass the same rejection tests (ADR 0007 invariant 5). Rejected candidates: default `serde_json::Value` (last duplicate key wins, which would let a duplicate bypass inspection); `simd-json` and similar (adoption needs measured benefit, ADR 0007).

### Core pin and provenance

Dependency: `redact-secret = "=0.1.0-beta.12"` from crates.io (library name `redact_secret`, package path `crates/secret-scan-core` in the core repository). Verified 2026-10-02:

| Item | Value |
| --- | --- |
| crates.io version | `0.1.0-beta.12`, published 2026-10-01T09:57Z, not yanked, `rust-version = 1.88`, MIT |
| crates.io sha256 (recorded in `Cargo.lock`) | `2cc951e8b991e9ec27343a872627f53a25b252cc8148cb4ec7160192ed9d856d` |
| Git tag | `v0.1.0-beta.12` (annotated tag object `8973ab50e068058693bcac9a0b2b8343bd327e85`; GitHub does not report the tag signature as verified) |
| Tagged commit | `4227160c4dac402d7add53d3f8fe990f693912c1`; the published `.cargo_vcs_info.json` records the same sha1 and `path_in_vcs = crates/secret-scan-core` |
| GitHub Release | None exists for beta.12 (the latest GitHub Release is beta.7). |
| Runtime dependencies of core | `unicode-normalization 0.1.25` (+ `tinyvec`); no features, no network or async crates |

"Exact core release" in #2 is interpreted as the crates.io publication plus the matching tag and commit: crates.io versions are immutable (yankable but not overwritable) and the lockfile checksum binds the exact artifact. The absence of a GitHub Release for beta.12 is a core-side housekeeping gap, not a blocker. Fallback if crates.io is rejected later: `git` dependency pinned by `rev = "4227160c..."` with a `deny.toml` `allow-git` entry. Never pin a branch or core `main`. The `tests/dependency_policy.rs` test fails if `Cargo.toml`, `Cargo.lock`, the checksum above, or `core_bridge::PINNED_CORE_VERSION` disagree, so a core bump requires an explicit change to this ADR and that test.

Core has no reverse dependency: `cargo tree -i redact-secret` shows only `redact-secret-gateway` depending on it, and `cargo tree -p redact-secret` shows only `unicode-normalization` and `tinyvec` beneath it.

Known constraints of the pinned core that shape the scaffold (verification and measurement stay with #5): `DetectorRegistry` is `!Send + !Sync`, so there is no shared `Arc<Registry>` and no global mutex, and `RuntimePlan` holds only `Profile` (a `Copy` enum) rather than a registry; `Policy::compile()` does not exist; there is no cooperative cancellation. `core_bridge` is therefore a boundary scaffold (profile-name parsing via the core's own `Profile::from_name`, and the sealed `CompleteInspection` proof type) with no core construction yet.

### Application unsafe, panic, and error conventions

- Unsafe: `unsafe_code = "forbid"` under `[lints.rust]` plus `#![forbid(unsafe_code)]` in both crate roots. No `#[allow(unsafe_code)]`. Dependencies may contain unsafe code (audited via `cargo deny`; further audit under the dependency rules).
- Panics: `[lints.clippy]` denies `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `indexing_slicing`, `arithmetic_side_effects`, and `dbg_macro` for production code. `clippy.toml` relaxes unwrap/expect/panic/indexing inside `#[test]` code only. Integration-test helper functions that need `expect` carry an explicit `#![allow(clippy::expect_used)]` at the test file level. Use checked or saturating arithmetic and `get()` on untrusted input.
- Errors: every module has a small typed error enum (`#[non_exhaustive]`, `Copy`) that maps to a `telemetry::SafeCode`. Errors hold no bodies, keys, headers, URLs, or credentials, and `Display` prints only the fixed code string. Types that hold request content (`ReceivedRequest`, `ValidatedRequest`, `SanitizedRequest`, `CompleteInspection`, `Json`) implement `Debug` manually and print lengths or kinds only. Panics in third-party callbacks are an open item for #5 (`catch_unwind` or built-in-only core use).

### Sealed types and ownership scaffold

Implemented per ADR 0002/0003/0005/0006: `admission::ReceivedRequest` -> `protocol::ValidatedRequest` -> `boundary::SanitizedRequest`; the sealed type's constructor is `pub(super)` inside `boundary`, with private fields and no `Clone`, `Default`, `Deserialize`, or setters; `transport::Upstream::forward` accepts only `SanitizedRequest`. `boundary::approve` is the sole path and needs a validated request, a `core_bridge::CompleteInspection` proof, and a `config::RouteId`. `admission` provides `ReceiptPermit`, `MemoryReservation`, `InspectionPermit`, `UpstreamPermit`, and `StreamPermit` as RAII `tokio::sync::Semaphore` permits, all acquired by `try_*` (no queue, no wait, no spawn, no numeric default); `config::RuntimePlan` is an immutable placeholder. `tests/api_boundary.rs` proves with `trybuild` that `ReceivedRequest`, `ValidatedRequest`, `Vec<u8>`, and a JSON tree cannot be passed to `forward`, and that `SanitizedRequest` can be neither literal-constructed, built via `new`, nor cloned outside `boundary`.

## Owner

Gateway maintainer. Dependency, feature, toolchain, and core-pin changes need a revision of this ADR in the same PR (CONVENTIONS.md).

## Invariants

1. `Cargo.lock` is committed and all commands use `--locked`.
2. The core pin is exact and identical in `Cargo.toml`, `Cargo.lock`, this ADR, and `core_bridge::PINNED_CORE_VERSION`.
3. Only `transport` references `reqwest`; `protocol` references neither `transport` nor `boundary` (checked by `tests/dependency_policy.rs`).
4. No OpenSSL-family or non-verifying TLS configuration is introduced.
5. Core never gains a dependency on the gateway.

## Failure behavior

A drifted pin, an unlisted source, a yanked crate, or a license outside `deny.toml` fails the dependency test or `cargo deny check`. Failed client initialization yields `TransportError::ClientInit` and readiness stays false (#4).

## Implementation handoff

- #4: `config` parsing into `RuntimePlan`, health routes on `health::router()`, readiness.
- #5: core initialization and probe in `core_bridge` (per-owner registry construction), worker choice, measurements; may add `tokio` features (`rt`, `time`) it needs.
- #6: CI using `rust-toolchain.toml`, `cargo deny`, fake upstream harness, permit-ordering and capacity tests; replace the grep-style module check if a stronger one is chosen.
- #7: build and release provenance recording this ADR's pins.

## Verification

`cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, `cargo build --locked`, `cargo deny check`, `cargo +1.88 check --locked --lib --bins`, and `cargo tree` output recorded in the #2 PR.

## Deferred measured choices

Parser optimization, node/string/output budgets, all capacity numbers, worker mode, and additional Tokio features: owned by #5/#18 per ADRs 0003, 0004, 0007, 0008.
