# Conventions

These conventions govern implementation in `redact-secret/gateway`. Proposed commands and artifacts must not be presented as released until they exist and are qualified.

## Names and versioning

- Product: RedactSecret Gateway. Repository: `redact-secret/gateway`. Executable: `redact-secret-gateway` (unpublished). The separate non-release SDK test build `redact-secret-gateway-qualification` is never an artifact ([ADR 0020](docs/decisions/0020-sdk-qualification-test-build.md)).
- Initial milestones are exactly `Alpha 1`, `Alpha 2`, `Beta 1`, `Beta 2`, and `Beta 3`.
- Intended release tags follow `v0.1.0-alpha.1`, `v0.1.0-alpha.2`, and `v0.1.0-beta.N`; stable is `v0.1.0` after qualification. Additional prereleases may be required; milestone completion is not automatic release approval.
- Gateway versioning is independent of core. Record the exact core version and source identity per release. Do not use floating git dependencies or automatically follow core beta releases.
- Keep Cargo.lock committed for the binary. Publish no internal crate as a separate API without an ADR.

## Dependency boundaries

Detection belongs in core. Gateway code must not recreate provider token regexes, scoring, PII logic, or detector registries. Protocol field selection, HTTP handling, static routing, resource limits, and deployment belong in Gateway. Core receives no Gateway/network dependency.

Use Rust and the selected asynchronous transport stack. Pin the toolchain and compatible dependency versions after the scaffolding ADR; do not assume the gateway must inherit core's workspace or exact MSRV. Minimize enabled dependency features and audit normal/build dependencies. Toolchain, versions, features, and the core pin are recorded in [ADR 0011](docs/decisions/0011-dependency-and-toolchain-selection.md). Panic and error conventions: `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, slice indexing, and unchecked arithmetic are denied by clippy in production code (tests may use them); fallible code returns small typed errors that carry fixed safe codes only. Unsafe Rust code is prohibited in this repository: enforce it with `#![forbid(unsafe_code)]` in every crate root (or `unsafe_code = "forbid"` under `[workspace.lints.rust]`) and do not add `#[allow(unsafe_code)]`. Dependencies may contain unsafe code; audit them under the dependency rules above.

## Configuration

Static, versioned configuration is the initial contract. Reject unknown configuration fields and invalid values. Validate configuration before accepting traffic. Configuration changes take effect through validated startup/restart; hot reload is out of scope unless separately designed.

Never embed real credentials in configuration examples. Separate local gateway authentication from provider authentication. Environment handling must be explicit; do not inherit HTTP proxy routing or secret values into diagnostics. Bind loopback by default. Unsupported external exposure must not be documented as supported deployment.

Numerical defaults need measured justification and boundary tests. A finite per-request limit is insufficient without a global concurrency/memory budget. No unlimited queue, detached task, stream buffer, or body collection is allowed.

## HTTP and protocol behavior

Receive complete bounded requests, classify fields, inspect decoded text, validate transformed output, then forward. Reject unsupported inputs rather than forwarding them unchanged. Unknown nested fields require the same classification discipline as top-level fields.

Recompute outbound lengths after transformation. Define header allowlists and hop-by-hop behavior explicitly. Disable redirects and gateway retries. Provider headers and local caller tokens follow separate handling rules (qualification fixtures use only the synthetic `local_token` and `local_token_decoy` in `qualification/synthetic.json`; the fake provider reports a `local_token_seen` flag and the harness fails if either value appears in a header, target, body, error, log or evidence file) (`transport::local_auth` owns the token: no `Clone`, `Display`, `Serialize` or equality operator, a redacted `Debug`, constant-time comparison, never in request state). Do not log requests before sanitization or assume sanitization makes body logging safe.

Gateway-generated errors use stable safe codes; preserve no offending text in error messages. Once response delivery begins, failure follows the documented termination contract. Do not fabricate provider SSE completion events to disguise an interrupted response.

## Code, tests, and changes

Prefer explicit states, typed failures, small modules, and reviewable ownership. Avoid panics and unchecked operations on untrusted input. Keep CPU inspection scheduling bounded and separate from transport assumptions. Any asynchronous worker or task must have a cancellation and cleanup owner.

Structural rules from the ADRs (see `docs/decisions/`): only `transport` holds HTTP clients and credentials, and it accepts only the sealed `SanitizedRequest` that only `boundary` can construct; protocol modules never send requests; capacity permits are owned by the resource they guard (a started synchronous job keeps its CPU and memory permits until it really finishes, never the HTTP future); build an immutable `RuntimePlan` at startup and keep per-request state and credentials request-local; no global mutable scanner behind a request-serializing lock; error and diagnostic types never own bodies or credentials; do not assume core APIs (`Policy::compile()`, cooperative cancellation) that the pinned core has not been verified to provide. Do not state a performance, zero-copy, or fast-path claim without measurements recorded per ADR 0008.

Fixtures contain synthetic credentials and invented PII only. Use fake upstreams for rejection and transport tests. Real provider credentials are never needed for ordinary CI. Optional live qualification runs are explicit, cost-bounded, and do not publish payloads or credentials.

Scaffolding establishes real commands for formatting, linting, tests, locked builds, artifact smoke checks, and configuration validation. Until implemented, documentation must label commands as proposed rather than runnable.

Use focused commits such as `feat(config): validate static routes` or `fix(transport): cancel upstream on disconnect`. PRs state resulting behavior, affected contracts, tests, core/dependency pins, and known limitations. Contract-changing work updates documentation and an ADR in the same PR.

## Issues and planning

Implementation issues have a parent epic, intended milestone, concrete deliverables, non-goals, dependencies, and evidence-based acceptance criteria. Use task lists and reciprocal links when native sub-issue tooling is unavailable; do not claim native hierarchy exists unless verified.

The scaffolding epic is the only epic initially decomposed into implementation sub-issues. Other epics are detailed placeholders until separately authorized for decomposition. A placeholder documents scope and gates, not permission to implement its entire future scope immediately.

Preserve Alpha 1 MVP delivery separately from skeleton completion. A compiling executable with health endpoints is not a working sanitizing proxy.

## Release integrity

Build once from the candidate commit and qualify the exact artifacts. Record checksums, source commit, toolchain, core pin, dependency lock, configuration version, SDK compatibility, and supported platforms. Do not assert signing, SBOM, provenance, or security review exists before those checks are implemented.

Document upgrade and rollback behavior. Never silently reinterpret configuration or weaken rejection behavior to preserve compatibility. License selection is an explicit maintainer decision before distribution. As of 2026-10-02 the license is MIT (maintainer decision) and GitHub private vulnerability reporting is enabled and verified through the GitHub API (see [ADR 0010](docs/decisions/0010-release-prerequisites-license-and-reporting.md)).

## Beta 2 deployment evidence

Use the fixed-key loopback operations export of ADR 0036, preserving the existing
telemetry exclusions. Native ARM64 execution is required; emulation is supplemental.
Record OCI manifest/index digests separately from Docker image config IDs. Do not
claim basic same-Pod NetworkPolicy enforces Gateway traversal. Record all quota,
load, soak and recovery outcomes before converting provisional resource guidance
into a support claim; only #15 owns final release reconciliation.
