---
name: dependency-audit
description: Scan this repository's Rust (Cargo) and container dependencies for known vulnerabilities, license and source problems, and core-pin drift with cargo-audit, cargo-deny, and OSV-Scanner, separating what ships in the binary or image from dev-only tooling. Use when asked to audit dependencies, before a release, or for "dependency-audit", "/dependency-audit". Report-only.
---

# dependency-audit

Answer one question: does any dependency we ship or build with have a known vulnerability, an unreviewed source, or a drifted pin?

## Orient first

The repo is in scaffolding. Check that `Cargo.toml`, `Cargo.lock`, and any `Dockerfile` exist. If there is no `Cargo.lock` yet, say so and review only what is declared (toolchain file, ADR, planned stack in `ARCHITECTURE.md`). Never run an audit against a lock you generated just for the audit.

## Run

1. `cargo fetch --locked` from a clean tree. A lock that needs updating is a finding.
2. `cargo audit --json` (RustSec). Record the tool version and advisory DB date.
3. `cargo deny check` if `deny.toml` exists (advisories, bans, licenses, sources). Otherwise report that it is missing.
4. `osv-scanner scan source -L Cargo.lock --format json`. Add any other lockfile found (for example under `examples/`). Record the version and scan time.
5. `cargo tree --workspace -e normal,build --locked` for the shipped graph, and `cargo tree -e features -i <crate>` for any hit, to see which features pull it in.
6. Core pin: confirm the `redact-secret` core entry in `Cargo.toml` and `Cargo.lock` is an exact version or exact commit, matches the pin recorded in the release notes or ADR, and is not a floating git branch (`CONVENTIONS.md` § Names and versioning).
7. Container: if a `Dockerfile` exists, confirm the base image is pinned by digest and run `osv-scanner scan image <image>` or `trivy image` on the built candidate when available.

## Classify every hit

- **Shipped (binary)**: normal and build dependencies of the `redact-secret-gateway` binary, including transitive ones, per `cargo tree`. Read the manifests; do not trust a remembered list. The preferred stack is Tokio, Axum, and Reqwest plus a JSON implementation and the core, and the TLS, HTTP/2, and parsing crates beneath them are the highest-value surface.
- **Shipped (image)**: base-image OS packages and anything copied into the final stage.
- **Build/test only**: dev-dependencies, proptest or fuzzing tools, fake-upstream test crates, CI tooling. Report separately with lower priority.
- **Core**: an advisory in the pinned core or its dependencies is shipped surface, and the fix belongs upstream in `redact-secret/redact-secret`. Note it as a cross-repository blocker.
- **Accepted**: advisories ignored in `deny.toml` or `audit.toml`. List them with their stated reason, and flag any entry with no reason, a passed expiry, or a fix that has since shipped.
- **Reachable?** State whether the vulnerable function or feature is used by our code or enabled features. Say "not assessed" when unsure; never guess "not reachable".
- **Source and license**: any git or path dependency, alternate registry, or license outside the allow-list. `CONVENTIONS.md` requires minimal features and audited normal and build dependencies, and the project license is not yet selected, so flag copyleft or unknown licenses for the maintainer.

## Output

| Class | Crate@version | Advisory (RUSTSEC/GHSA/CVE) | Severity | Fixed in | Reachable | Action |
| --- | --- | --- | --- | --- | --- | --- |

Then the core-pin check, the `cargo deny` result, the image result if run, and the tool versions. Verdict: `no known vulnerabilities in shipped dependencies` or the counts.

## Rules

- Do not upgrade anything or edit `Cargo.lock`. A core or dependency upgrade is an explicit qualification change that records exact pins and reruns boundary checks (`CONTRIBUTION.md`), so propose it instead.
- Never paste tokens. If a registry call needs auth, stop and say so.
- A tool that is not installed is reported as not run, never as a pass.
