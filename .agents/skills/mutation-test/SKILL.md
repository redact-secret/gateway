---
name: mutation-test
description: Measure whether the gateway's tests catch fail-closed regressions by mutating the security-critical Rust source with cargo-mutants and reporting surviving mutants. Use when asked to check test strength, after changing boundary, classification, limit, or transport code, or for "mutation-test", "/mutation-test src/boundary". Report-only; proposes tests for survivors.
---

# mutation-test

A fail-closed check that can be deleted without a test failing is not protected. Find those.

## Orient first

The repo is in scaffolding. Locate the real modules with `graft map` or `graft skeleton src/`; the layout in `ARCHITECTURE.md` (configuration, boundary orchestration, protocol/OpenAI, core integration, transport, health, safe telemetry) is planned, not guaranteed. If there is no `Cargo.toml` or no tests yet, report that and stop. Mutating code with no tests only produces noise.

## Setup

- Tool: `cargo-mutants` (`cargo install --locked cargo-mutants`, or `cargo mutants --version` if present). Do not add it as a project dependency. Record its version.
- Baseline must pass first: `cargo test --locked`. If it fails or is flaky, stop and report.
- Use the fake-upstream integration tests as the test suite. They are what prove "no upstream body on rejection", so a mutant surviving there is meaningful.
- Mutate only security-relevant source, or the file given as an argument:
  - request admission and body receipt (size, timeout, framing)
  - JSON validation and field classification (unknown-field rejection, duplicate keys, UTF-8)
  - core bridge (completion/truncation/failure checks, finding limits)
  - serialization and output limits, recomputed length
  - transport (header allowlist, hop-by-hop removal, destination selection, redirect and proxy settings, cancellation)
  - configuration validation (unknown fields, loopback default)
  - safe error mapping and telemetry (no payload in output)
- Skip generated code, `main`, and pure formatting.

## Run

`cargo mutants --locked --file <path> --jobs <n>` (one file or module per run; lower `--jobs` if the fake upstream binds fixed ports). Add `--in-diff <(git diff main...HEAD)` to limit to a change. Output lands in `mutants.out/`; keep it out of commits and confirm it is gitignored. Record the score from `mutants.out/outcomes.json`.

## Triage survivors

For each surviving mutant (`missed`), decide one of the following:
- **Gap.** It weakens a security check: a rejection branch turned into a pass, a `<` versus `<=` at a limit, an inverted completeness or truncation check, a skipped unknown-field or duplicate-key check, a header allowlist or hop-by-hop removal change, a redirect or proxy flag flip, a cancellation or cleanup path removed, forwarding before inspection completes, or an error or log path that includes payload text. Propose the exact test that kills it, at the boundary value.
- **Equivalent.** The behavior is unchanged. Say why in one line.
- **Non-security.** A message string, a comment, a metric label, a type-only branch, or dead code. List it without action.
- A `timeout` or `unviable` mutant is not a survivor; report the counts.

## Output

| Verdict | file:line | Mutation | Why it survived | Proposed test |
| --- | --- | --- | --- | --- |

End with the mutation score overall and for security-relevant files, the counts of caught, missed, timeout, and unviable, and the number of gaps.

## Rules

- cargo-mutants edits source in a scratch copy, but confirm `git status --short src tests Cargo.toml Cargo.lock` is unchanged when done, and never commit mutated source.
- Synthetic values only in any proposed test. Negative tests must assert the fake upstream received zero body bytes, not merely that an error status came back.
- Proposed tests belong in `tests/` beside the existing fake-upstream tests. Do not apply them unless asked.
