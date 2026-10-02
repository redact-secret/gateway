---
name: ci-hardening
description: Audit this repository's GitHub Actions workflows, release automation, container build, and repo rulesets for supply-chain weaknesses with zizmor and OpenSSF Scorecard, then propose exact patches. Use when asked to harden or review CI/CD, before changing a release or publish workflow, or for "ci-hardening", "/ci-hardening". Report-only unless asked to apply.
---

# ci-hardening

Find ways CI or release automation could be abused. Propose patches; apply only when asked.

## Orient first

The repo is in scaffolding. Run `ls .github/workflows .github/rulesets 2>/dev/null` before anything else. Audit what exists. If a workflow named below is absent, say "not present yet" and judge the planned design against `CONVENTIONS.md` § Release integrity and `SECURITY.md` § Security release gates instead of inventing findings. Do not describe signing, SBOM, or provenance as present until a workflow produces them.

## Run

- `uvx zizmor --format plain .github/workflows/` (or `pipx run zizmor`). Record the zizmor version.
- `scorecard --repo=github.com/redact-secret/gateway --format json`. This needs `GITHUB_AUTH_TOKEN`; skip it and say so if the token is unavailable. Compare with the latest Scorecard workflow run if one exists.
- Read every workflow in `.github/workflows/` yourself. The tools miss repo-specific intent.
- Read `CONVENTIONS.md` § Release integrity and `CONTRIBUTION.md` § Release roadmap for the intended flow, then check the workflows enforce it.

## Checks

| Check | Pass when |
| --- | --- |
| Action pinning | Every `uses:` is pinned to a full commit SHA with a version comment |
| Token permissions | Top-level `permissions: {}`; each job declares only what it needs (`contents: read` for build and test jobs) |
| Toolchain pinning | Rust toolchain comes from the pinned `rust-toolchain.toml`, not `stable` or `latest`; `cargo build`/`test` use `--locked`; `Cargo.lock` is committed |
| Core pin | The `redact-secret` core dependency is an exact version or exact commit, never a floating git branch; CI fails if the lock changes it unreviewed |
| Build once | Release artifacts are built once from the candidate commit and the same bytes are smoke-tested and published, not rebuilt per stage |
| Artifact integrity | Checksums are generated and published with each artifact; provenance, signing, and SBOM steps exist only if claimed, and use OIDC (`id-token: write`) on the publishing job only |
| Container image | Base image pinned by digest, non-root user, no secrets in layers or build args, registry push bound to a protected `environment`, tags from the release plan and never an unconditional `latest` |
| Injection | No `${{ github.event.* }}`, `github.head_ref`, PR titles, or branch names inside `run:`; values pass through `env:`. No `pull_request_target` that checks out PR code |
| Credentials | `persist-credentials: false` on checkout unless a later step pushes; no long-lived registry or crates token secret when OIDC is available; no real provider key anywhere in CI |
| Live provider tests | Ordinary CI uses only the fake upstream. Any live-provider job is manual, cost-bounded, environment-gated, and does not upload payloads or credentials |
| Rulesets | Live rulesets match any committed `.github/rulesets/*.json`: no bypass actors, no force-push or deletion, required status checks. Read via `gh api repos/redact-secret/gateway/rulesets` and diff against committed files |
| Artifacts and logs | Nothing secret-bearing uploaded or echoed; test output carries synthetic data only |
| Dependabot / Renovate | Covers github-actions, cargo, and docker ecosystems if a Dockerfile exists |

## Output

| Severity | Workflow:line or setting | Finding | Exploit path | Patch |
| --- | --- | --- | --- | --- |

Give each patch as a minimal diff. End with the Scorecard score, if run, and a one-line verdict.

## Rules

- Never print, create, or move secrets. Do not change repo settings, rulesets, or push unless asked.
- A finding in a release workflow needs a dry-run consideration: say whether the patch can be checked by a rehearsal or `workflow_dispatch` run before a real release.
