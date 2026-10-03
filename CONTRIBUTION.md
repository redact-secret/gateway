# Contribution guide

Thank you for contributing to RedactSecret Gateway. This project is initially in architecture and scaffolding development. Read README.md, ARCHITECTURE.md, CONVENTIONS.md, and SECURITY.md before proposing implementation.

This file intentionally uses the requested name `CONTRIBUTION.md`. README links here explicitly; it is not a claim that GitHub will auto-detect the usual `CONTRIBUTING.md` filename.

## Choose work within the current scope

Alpha 1, Alpha 2 and the Beta 1 Responses and configuration epics are implemented and qualified against the fake-upstream build (see the qualification reports). Beta 2 is implemented and qualified for the native sidecar environments in its [report](docs/qualification/beta2-qualification-report.md). Beta 3 owns final stable-candidate review and release gates; discuss new scope before decomposing it.

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

Connection bound (#40, [ADR 0022](docs/decisions/0022-connection-bound-at-accept.md)): `tests/connection_limits.rs` drives the production `bind`/`serve` stack over real loopback sockets (silent, slow-head, and abandoned connections near and across `max_connections`, slot return, health at the bound, shutdown with many open connections); gate on events (a server close, a probe answered) and bound every wait, never on a fixed sleep. `cargo build --locked --release && cargo run --locked --release --example conn_cost` measures descriptors and resident memory per idle and fat-head connection; record the host load with any number you quote.

Adversarial qualification suite (#25): `src/transport/tests/attack_tests.rs` (served stack with the production connection handling, a fake provider, hostile targets, headers, framing, redirects, slow heads, floods, and deterministic seeded mutation runs; extend the tables there, and keep the seed and case counts committed so failures reproduce), `tests/attack_surface.rs` (real binary, hostile environment, no upstream), `tests/diagnostic_surface.rs` (every error type renders only fixed safe text), and the compile-fail cases in `tests/ui/`. The threat-to-test map, tested stack pins, and residual risks are in [docs/qualification/alpha1-threat-control-map.md](docs/qualification/alpha1-threat-control-map.md); a change that adds a threat control, a supported request form, or a new error type must update the map in the same PR.

Exact-binary configuration and local-auth lifecycle (#65): `sh scripts/smoke-local-auth.sh <binary> <dir>` (also the tail of `scripts/smoke-binary.sh`) validates the shipped configs and walks startup, readiness, restart activation, rollback and refusals; it never sends a provider `Authorization`, so no request can leave the machine. The Beta 1 report is `docs/qualification/beta1-qualification-report.md`; a change to the local-auth contract, the dual-credential examples, or the supported listener combinations updates it, and #88 extends it with Responses.

SDK qualification (#22, [ADR 0020](docs/decisions/0020-sdk-qualification-test-build.md)): `sh qualification/build.sh` generates and builds the separate non-release crate `redact-secret-gateway-qualification` (a copy of `src/` plus `qualification/seam.patch` and `qualification/overlay/*`; the shipped package is never touched); `sh qualification/run-suites.sh` starts the scripted fake provider (`qualification/fake-provider/server.mjs`, Node built-ins only) and ten gateway instances (including `authfile` and `authenv`, which enforce `deployment.local_auth` with synthetic tokens, #65), then runs the pinned Node/TypeScript suite (`qualification/sdk/node`, `node:test`; the Responses suites are `responses-*.test.ts`, driven by `qualification/responses-cases.json` and the Responses scenarios in `qualification/fake-provider/responses.mjs`, #87 and #88), the pinned Python suite (`qualification/sdk/python`, `unittest`), and the README examples (`qualification/run-examples.sh`). Setup once: `npm ci --ignore-scripts` in `qualification/sdk/node` and `examples/node` (Node 24 or newer), and `python3 -m venv .venv && .venv/bin/pip install --require-hashes --no-deps -r requirements.txt` in `qualification/sdk/python` and `examples/python`. The workflow is `.github/workflows/qualification.yml`; it uploads only text evidence and never the test binary. Only the ten files in `QUALIFICATION_SEAM_ALLOWLIST` (`tests/destination_policy.rs`) may implement or invoke the seam; any other file that does fails `cargo test`. When the seam patch stops applying after a change to `server.rs`, `transport.rs`, `destination.rs`, or `resolver.rs`, regenerate it deliberately and review what the qualification build changes. Synthetic stage-timing and peak-memory runs: `QUAL_COMMAND='node qualification/perf/run.mjs' sh qualification/run-suites.sh --binary qualification/target/release/redact-secret-gateway-qualification --suites none` after `sh qualification/build.sh --release`; results are labelled provisional unless the host is quiet.

Performance workloads are a measurement tool (ADR 0008): `cargo run --locked --release --example perf_workloads` prints JSON lines with coarse p50/p95/p99 timings and resident memory per phase, with no payloads or credential labels. Numbers from it are not performance claims and may not become defaults without recorded pins.

Candidate artifacts: `.github/workflows/artifacts.yml` (`workflow_dispatch`, and pull requests touching build files) builds the Alpha 1 candidate once per target, smoke-tests the exact bytes on their platform (startup, config, health, proxy-route rejections that never forward, seam-absence check; the image also non-root and the Compose example), builds the image from the Linux binary, and uploads a manifest with `SHA256SUMS`. It publishes nothing and has no secrets or registry credentials. Local helpers live in `scripts/`; see [docs/artifacts.md](docs/artifacts.md).

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

## Beta 2 qualification

`python3 scripts/test-oci-bundle.py` checks OCI platform/blob invariants. Native
Linux ARM64 candidate build/smoke uses an actual ARM64 runner; image assembly
checks the ELF machine field before copying exact tested bytes. A local OCI
layout/archive combines the native-tested variants without registry publication.
The pull-request/manually dispatched `.github/workflows/beta2.yml` builds exact candidate
startup evidence separately from a non-release workload image and runs
`qualification/sidecar/run.py` on native Linux amd64/arm64. Only safe aggregate
text evidence is uploaded. See the [Beta 2 register](docs/qualification/beta2-qualification-report.md)
for executed datasets, reproduction and remaining Beta 3 release gates; a passing build alone does not qualify deployment.
