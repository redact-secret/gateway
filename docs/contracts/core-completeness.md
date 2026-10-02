# Contract: core completeness

Status: approved design contract; the exact core API is unverified. Probe owner: #5. Related: [ADR 0004](../decisions/0004-cancellation-and-synchronous-core-work.md).

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

## Status

Planned. No probe has run. Core #1001 prototype and threat-model acceptance is still outstanding.
