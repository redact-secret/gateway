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

CI does not exist yet (#6). Do not claim a check passed without running it. `cargo test` includes `trybuild` compile-fail tests whose expected diagnostics live in `tests/ui/*.stderr`; after an intentional change, regenerate with `TRYBUILD=overwrite cargo test --locked --test api_boundary` and review the diff. Production code must not panic on untrusted input: unwrap, expect, panic, and indexing are denied by lint outside tests, and unsafe code is forbidden.

Normal tests use a fake upstream and synthetic inputs. Optional live provider tests require a separate explicit procedure and must never become an implicit requirement for contributors or ordinary CI.

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

The repository must select and add a license before distributing artifacts. Until then, do not assume the gateway inherits a dependency's license. Contributors should confirm the repository licensing decision before submitting code intended for public distribution.
