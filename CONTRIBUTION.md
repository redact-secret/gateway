# Contribution guide

Thank you for contributing to RedactSecret Gateway. This project is initially in architecture and scaffolding development. Read README.md, ARCHITECTURE.md, CONVENTIONS.md, and SECURITY.md before proposing implementation.

This file intentionally uses the requested name `CONTRIBUTION.md`. README links here explicitly; it is not a claim that GitHub will auto-detect the usual `CONTRIBUTING.md` filename.

## Choose work within the current scope

Use the Alpha 1 scaffolding epic and its linked tasks for the first implementation. Other epics are planning placeholders and do not yet have implementation sub-issues. Discuss scope before decomposing or implementing them.

Gateway owns transport and protocol handling. Detector improvements belong in `redact-secret/redact-secret`. In-process logging/SDK integration belongs in adapters. Restore authority belongs in Vault. Comparative evaluation belongs in the relevant benchmark/evaluation repository. Cross-repository changes need explicit links and ownership; do not duplicate detection logic to avoid a core dependency change.

## Report ordinary bugs and integration gaps

Include the gateway commit/version, exact core pin, operating system/architecture, SDK version, configuration schema version, endpoint and supported payload form, synthetic reproduction, expected behavior, actual safe error code, and relevant timing/limit conditions.

Do not paste real prompts, API keys, identifying personal data, provider error payloads, or raw traces. Replace them with synthetic values. For a possible vulnerability, follow SECURITY.md instead of filing a public issue.

## Development setup

The pinned Rust toolchain (`rust-toolchain.toml`), crate manifest, lockfile, and dependency ADR ([ADR 0011](docs/decisions/0011-dependency-and-toolchain-selection.md)) exist. Run these before every PR; all use the committed lockfile:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
cargo deny check     # needs cargo-deny
```

CI (`.github/workflows/ci.yml`) runs the same commands plus a `--version` startup check; do not claim a check passed without running it or seeing CI pass. `cargo test` includes `trybuild` compile-fail tests whose expected diagnostics live in `tests/ui/*.stderr`; after an intentional change, regenerate with `TRYBUILD=overwrite cargo test --locked --test api_boundary` and review the diff. Production code must not panic on untrusted input: unwrap, expect, panic, and indexing are denied by lint outside tests, and unsafe code is forbidden.

Normal tests use a fake upstream and synthetic inputs. Optional live provider tests require a separate explicit procedure and must never become an implicit requirement for contributors or ordinary CI.

## Test harness and CI

Test support lives in `tests/support/` (test-only; never compiled into the binary). Include it from an integration test with `mod support;`.

| Module | Use |
| --- | --- |
| `support::fake_upstream` | Loopback fake provider. `FakeUpstream::start(Behavior)`; records connections, headers, and body bytes (partial bodies too). `assert_nothing_sent()` / `check_nothing_sent()` is the zero-upstream-body assertion for rejected requests. Behaviors: `Json`, `Slow`, `DisconnectBeforeResponse`, `DisconnectMidBody`, `Malformed`, `Sse` (fragments with delays, chunked or close-delimited, with or without clean finish), `SseEndless` (chunked, writes until the peer closes; `streamed_bytes()` and `peer_closed()` / `wait_for_peer_closed()` observe backpressure and upstream cancellation); `enqueue` scripts per-call behavior. |
| `support::leak` | `Markers::standard()` plants synthetic canaries (`BODY_MARKER`, `HEADER_MARKER`, `KEY_MARKER`, `ARG_MARKER`); `assert_clean*` fails if any appears in gateway-generated text. Failure messages carry the marker label and offset, never the value. |
| `support::permits` | `Capacity` (free-capacity probes, `assert_all_free`), `start_job` (non-interruptible reference worker owning its permits), `OrderTracker` (fixed acquisition order). |
| `support::parser_cases` | Shared accept/reject table; `run_conformance(parser)` must be empty for every request-body parser. Wire any optimized parser in `tests/parser_conformance.rs`. |
| `support::workloads` | Synthetic payload shapes and timing/RSS measurement used by `examples/perf_workloads.rs`. |

Harness tests: `tests/harness_fake_upstream.rs` (harness self-tests), `tests/no_forward_skeleton.rs`, `tests/permits_capacity.rs`, `tests/permits_cancellation.rs`, `tests/parser_conformance.rs`, `tests/perf_scaffold.rs`, `tests/inspection_transform.rs` and `tests/inspection_route.rs` (core inspection and transformation, #19), plus the compile-fail API boundary in `tests/api_boundary.rs`. End-to-end forwarding tests (#20) live in `src/transport/tests/forward_tests.rs` and end-to-end SSE relay tests (#21) in `src/transport/tests/stream_tests.rs`, not under `tests/`: the loopback fake upstream is reachable only through `#[cfg(test)]` items, and `tests/destination_policy.rs` forbids any feature, flag, or environment path to it, so a test that forwards must be a crate-internal unit test that includes `tests/support/*.rs` through `#[path]`. Integration tests under `tests/` use configs with no `deployment.upstream`, so they can never reach a provider. They cover only what the skeleton implements; full route and stream behavior is qualified in #18-#25. Each negative control (a deliberately wrong worker, parser, or leak) proves its assertion can fail.

Performance workloads are a measurement tool (ADR 0008): `cargo run --locked --release --example perf_workloads` prints JSON lines with coarse p50/p95/p99 timings and resident memory per phase, with no payloads or credential labels. Numbers from it are not performance claims and may not become defaults without recorded pins.

Candidate artifacts: `.github/workflows/artifacts.yml` (`workflow_dispatch`, and pull requests touching build files) builds the skeleton once per target, smoke-tests the exact bytes on their platform, builds the image from the Linux binary, and uploads a manifest with `SHA256SUMS`. It publishes nothing and has no secrets or registry credentials. Local helpers live in `scripts/`; see [docs/artifacts.md](docs/artifacts.md).

CI (`CI passed` aggregates `Format`, `Lint`, `Test`, `Build and startup`, `Dependency policy`) uses no secrets and no provider credentials, and makes no provider calls. Workflow rules: every `uses:` pinned to a full commit SHA, `permissions: {}` at top level with per-job `contents: read`, `persist-credentials: false`, no event data inside `run:`, toolchain from `rust-toolchain.toml`. Live-provider qualification, when it exists, is a separate manual, cost-bounded, environment-gated workflow. Extension point for #4 (config validation, health) is marked `TODO(#4)` in the workflow.

## Pull request requirements

1. Link the parent epic and implementation issue, and state the concrete resulting behavior.
2. Identify changes to supported fields/routes, rejection behavior, credentials, limits, configuration, or streaming semantics.
3. Include meaningful tests: especially negative tests proving rejected requests transmit no upstream body.
4. Update related contracts, examples, and ADRs. Clearly distinguish planned from implemented behavior.
5. Record validation performed and any remaining blocker. Do not claim a test passed without execution evidence.

Protocol changes must classify new fields and input forms. Dependency/core upgrades must record exact pins and rerun affected boundary and SDK compatibility checks. Artifact changes require execution smoke tests of the actual candidate binaries/images.

## Review principles

Maintainers review correctness at the network boundary, deterministic core reuse, resource ownership, cancellation, sensitive-data handling, supported SDK behavior, and maintainability. Passing a happy-path test is insufficient for a boundary change.

Keep changes focused. Do not add a generic proxy, multi-tenant service, provider key store, dashboard, public plugin API, or reversible mode inside an unrelated issue. These require their own approved architecture and qualification scope.

## Agent-assisted contributions

Agents follow the same requirements as human contributors. Read the five baseline documents and the complete issue before editing. Preserve repository scope, use synthetic fixtures, and record concrete validation. A future placeholder does not authorize task decomposition, unrelated code changes, release publication, or private-data collection.

## Licensing

The repository is licensed under the MIT License (see `LICENSE`). By submitting a contribution you agree it is provided under the same license.
