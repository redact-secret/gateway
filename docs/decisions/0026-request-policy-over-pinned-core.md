# ADR 0026: Request block/redact policy over the pinned core

Status: Accepted (design and implementation), by maintainer delegation to the #56 author. Date: 2026-10-03. Builds on [ADR 0004](0004-cancellation-and-synchronous-core-work.md), [ADR 0006](0006-runtime-plan-and-authority-separation.md), [ADR 0015](0015-core-inspection-and-request-transformation.md), [ADR 0025](0025-alpha2-field-contract.md). Contract: [request-policy](../contracts/request-policy.md). Issue: #56 (epic #9).

## Context

The Alpha 1 bridge already used the core's `DefaultPolicy`, `on_warn` and a finding bound. #56 asks for the supported request-level block/redact contract over the exact pin, compiled into the startup plan, with every complete and incomplete outcome mapped and tested.

## Decision

1. **The action set is the core's.** `Redact`, `Block`, `Warn`, `Allow` from `DefaultPolicy`. No `Policy::compile()` is assumed (the pin has none), no custom `Policy` is supplied, no per-type override exists. The single mapping is `core_bridge::disposition`.
2. **Operator surface stays four keys** (`profile`, `pii`, `on_warn`, `max_findings`). Any other key under `content` is refused at startup. No `on_block`, no bypass, no profile-controlled routing or authentication.
3. **Block always blocks; `on_warn` relaxes only `Warn` in a redact slot.** It never relaxes limits, errors, detect-only slots, revalidation, or completeness.
4. **Startup validates the activation.** The config parse builds one core registry for the profile and PII selection and refuses the process if the pin cannot.
5. **Ownership is unchanged.** Immutable `Send + Sync` `InspectorSpec` shared; one `!Send` registry per worker; request state in a per-request `RequestScope`.
6. **Redaction that cannot preserve the request is rejection**, never truncation or raw fallback.

## Why not richer policy

A per-type or per-class action table would be an independent policy language over detections the core owns, and would need the gateway to name core finding types that can change between pins. The issue's non-goals exclude it. If the operator needs a different action for a class, that is a core feature request.

## Invariants

No upstream byte until parse, complete inspection, transformation and revalidation succeeded. No raw fallback. Unknown actions, selectors and keys fail closed. Detection stays core-owned and this does not promise detection of every secret (`common` is narrower than `full`; PII is context-gated; the pin has no Korean national selector).

## Failure behavior

The table in the contract. All rejections use existing safe codes; no new code is introduced.

## Implementation handoff

`src/core_bridge.rs` (`FindingDisposition`, `disposition`, `validate_activation`, block-before-warn precedence), `src/config.rs` (`parse_content` validates activation), `tests/policy_actions.rs`. #53 to #55 reuse the visit order and poisoning unchanged.

## Verification

`tests/policy_actions.rs` (action table, precedence, PII and profile combinations with pinned-core results, startup rejection, authority-section equality, many findings, output growth, pre-redacted input, escapes). Existing `tests/inspection_*.rs` untouched.

## Deferred

A published core cost bound and cancellation (core #1177) would let a deadline be added; a core multi-leaf numbering helper (#1180) would remove the gateway offset. Pin bumps must re-run the action and completeness tests (#1179).
