# ADR 0010: License and private security reporting (release prerequisites)

Status: **Partially decided.** The license is MIT (maintainer decision). Private security reporting was enabled by the maintainer (GitHub API `enabled: true`); an end-to-end test report is still to be recorded.

## Context

The baseline documents require an explicit license selection and a verified private security-reporting channel before any alpha artifact is distributed.

## Verified state (2026-10-02)

Checked with the GitHub API for `redact-secret/gateway`:

- Repository license metadata was `null` and no `LICENSE` file existed when this ADR was written; the maintainer has since selected MIT and the `LICENSE` file was added.
- Private vulnerability reporting: `enabled: false`.
- The repository is public. Secret scanning, push protection, and Dependabot security updates report `disabled`.

## Decision

License: **MIT**, selected by the maintainer. Private reporting: enabled; test-report verification pending and still a release gate:

| Item | Needed | Decider |
| --- | --- | --- |
| Repository license | Decided: MIT. `LICENSE` file and `Cargo.toml` `license = "MIT"` added. Copyright holder line in `LICENSE` should be confirmed by the maintainer | Maintainer |
| Private reporting | Decided/enabled (API shows `enabled: true`). Remaining: send a test report and confirm it reaches the maintainer. Original requirement: maintainer enables GitHub private vulnerability reporting (or names an established private channel), then someone verifies it works and updates SECURITY.md | Maintainer |

No contact address, response SLA, or disclosure deadline is invented. SECURITY.md keeps its current wording that does not claim a channel exists.

## Owner

Maintainer. Verification of the channel by a reviewer after enabling.

## Invariants

1. No artifact is distributed until a test report has been verified end to end.
2. Documents never state that reporting is enabled until the API shows it enabled and it has been tested.

## Failure behavior

If the test-report verification is not recorded at release time, release is blocked and recorded as blocked with the owner, not waived.

## Implementation handoff

- #7 (release manifest and artifact scaffolding): must refuse to label anything as distributable while this is open.
- Epic #1 completion evidence: "license decision and private reporting readiness reconciled".

## Verification

`gh api repos/redact-secret/gateway/private-vulnerability-reporting` returns `enabled: true`; a test report reaches the maintainer; license metadata is non-null and matches the file.

## Deferred measured choices

None.
