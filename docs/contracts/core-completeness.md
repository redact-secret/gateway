# Contract: core completeness

Status: approved design contract; verified against `redact-secret =0.1.0-beta.12` by the #5 probe ([report](../probes/core-bridge-probe.md)). Related: [ADR 0004](../decisions/0004-cancellation-and-synchronous-core-work.md).

## Rule

A `SanitizedRequest` may be created only when core processing of every inspected field is known to be complete. Any signal that inspection was not complete rejects the request with `incomplete_inspection`. Silence from the core API is not success.

## Conditions that must reject

- Truncation of any inspected text.
- Finding count at or over a maximum, where the API indicates findings were dropped.
- Detector failure or error.
- Any partial result.
- Unsupported or malformed core profile or policy at runtime.
- Core call that did not finish before a deadline or cancellation (the result is discarded; see ADR 0004).

## What the gateway must not do

- Assume a capability the pinned core does not expose. In particular, `Policy::compile()` and cooperative cancellation are not assumed to exist.
- Treat "no findings returned" as proof of completeness unless the API says so.
- Reimplement detection to fill a gap.

## Blocker handling

If the pinned public core API cannot distinguish complete from incomplete inspection, record a cross-repository contract blocker (issue in `redact-secret/redact-secret`, linked from #5 and from #1001). Gateway keeps failing closed on the affected path. The blocker is explicit and not treated as satisfied.

## Required evidence (#5)

For the exact core pin and profile: synthetic tests for success and every exposed incomplete or failure outcome; documented state isolation, Unicode handling, and pre-redacted input behavior. The probe records the pin, profile, and measurement method.

## Verified for the pinned core (beta.12)

- The `Result` of a whole-input call is the completeness signal. `Ok` means every registered detector inspected the whole input and the policy and formatter succeeded. Every limit, detector, candidate, policy, placeholder, and findings failure is a distinct `Err`. There is no truncation flag, partial result, or per-detector status, so no completeness blocker exists for this pin.
- The mapping from core codes to gateway failures is `core_bridge::map_core_error`; any code it does not name, including one a later core adds, is `incomplete_inspection`.
- A `CompleteInspection` can be minted only through `RequestScope::finish`, and only if every leaf in the request returned `Ok`. A failing leaf (limit, `Block`, any core error) poisons the scope.
- Input and finding limits are applied by the core per call; the gateway applies them across the whole request by summing in `RequestScope`. A finding-limit failure yields no redacted text, so it can only reject.
- `Ok` does not mean nothing sensitive remains: the default policy leaves `Warn` findings in the text. They are counted in `InspectionSummary::unredacted_findings`; whether they reject is a policy decision for #19.
- The core has no cancellation, deadline, or work budget, and `DetectorRegistry` is `!Send + !Sync`. Capacity is held until real completion (ADR 0004); see the report for the offload evidence and the draft core-side issues.

## Status

Probe run for beta.12; bridge and offload candidate implemented as a prototype (`core_bridge`, `core_bridge::pool`). Not yet wired to a route: #18/#19 consume `Inspector`, `RequestScope`, and `CompleteInspection`. Core #1001 prototype and threat-model acceptance is still outstanding, and the draft core-side issues in the report are not yet filed.
