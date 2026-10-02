# ADR 0010: License and private security reporting (release prerequisites)

Status: **Open. Needs a maintainer decision.** This ADR does not select a license and does not claim a reporting channel is enabled.

## Context

The baseline documents require an explicit license selection and a verified private security-reporting channel before any alpha artifact is distributed.

## Verified state (2026-10-02)

Checked with the GitHub API for `redact-secret/gateway`:

- Repository license metadata: `null`. No license is detected and no `LICENSE` file exists.
- Private vulnerability reporting: `enabled: false`.
- The repository is public. Secret scanning, push protection, and Dependabot security updates report `disabled`.

## Decision

None yet. Both items are explicit release blockers:

| Item | Needed | Decider |
| --- | --- | --- |
| Repository license | Maintainer picks a license and adds the license file. Do not copy core's license by assumption | Maintainer |
| Private reporting | Maintainer enables GitHub private vulnerability reporting (or names an established private channel), then someone verifies it works and updates SECURITY.md | Maintainer |

No contact address, response SLA, or disclosure deadline is invented. SECURITY.md keeps its current wording that does not claim a channel exists.

## Owner

Maintainer. Verification of the channel by a reviewer after enabling.

## Invariants

1. No artifact is distributed while either item is open.
2. Documents never state that reporting is enabled until the API shows it enabled and it has been tested.

## Failure behavior

If either item is open at release time, release is blocked and recorded as blocked with the owner, not waived.

## Implementation handoff

- #7 (release manifest and artifact scaffolding): must refuse to label anything as distributable while this is open.
- Epic #1 completion evidence: "license decision and private reporting readiness reconciled".

## Verification

`gh api repos/redact-secret/gateway/private-vulnerability-reporting` returns `enabled: true`; a test report reaches the maintainer; license metadata is non-null and matches the file.

## Deferred measured choices

None.
