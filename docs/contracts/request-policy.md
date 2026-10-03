# Contract: request-level block/redact policy

Status: implemented for the Alpha 1 text subset; applies unchanged to the Alpha 2 slots as #53 to #55 land them. Decision: [ADR 0026](../decisions/0026-request-policy-over-pinned-core.md). Verified against `redact-secret =0.1.0-beta.12`. Related: [core-completeness](core-completeness.md), [field-classification](field-classification.md), [ADR 0025](../decisions/0025-alpha2-field-contract.md), [ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md).

## What the policy is

The only actions are the four the pinned core attaches to a finding through `DefaultPolicy` (`Redact`, `Block`, `Warn`, `Allow`). The gateway adds no detector, no policy language, no per-type override, and no bypass. The operator's whole policy surface is the static `content` section:

| Key | Values | Default | Effect |
| --- | --- | --- | --- |
| `profile` | `common`, `full` (core names, exact case) | required | Which built-in detectors the core runs. `common` is narrower: it does not detect a GitHub-style token that `full` redacts. |
| `pii` | core selectors, at most 32 | none | Optional PII families. Detection is the core's, and it is context-gated (a bare address with no label is not a finding). The pin has no Korean national selector (`pii:kr` is refused). |
| `on_warn` | `reject`, `forward` | `reject` | What a `Warn` finding does (below). |
| `max_findings` | 1 to 50000 | 1024 | Request-wide finding bound; one more finding rejects the request. |

Nothing in `content` can select a destination, credential, listener, route, or TLS rule. Those live in `deployment`, and the tests assert the `deployment` and `resources` sections of the `RuntimePlan` are identical under every supported `content` combination.

## Action and result table

"Slot" below is a text position from the field matrix. A `redact` slot is rewritten in place; a `detect-only` slot (labels, identifiers, `model`, keys) is never rewritten (ADR 0025).

| Core outcome | Gateway behavior | Public result |
| --- | --- | --- |
| `Redact` finding, redact slot | Replaced with `<SECRET_n>`, `n` numbered request-wide from 1 | forwarded after full success |
| `Block` finding (for example `private_key`), any slot | Whole request rejected, no body produced, in both `on_warn` modes | `422 unsupported_input` |
| `Warn` finding, `on_warn = reject` | Whole request rejected | `422 unsupported_input` |
| `Warn` finding, `on_warn = forward`, redact slot | Text kept unchanged; counted in `warned_findings`; every other check still applies | forwarded after full success |
| `Allow` finding | Text kept, counted as unredacted. `DefaultPolicy` never returns it; mapped so a future change cannot read as redaction | forwarded after full success |
| Any finding, detect-only slot | Rejected whatever its action and whatever `on_warn` says | `422 unsupported_input` |
| No findings | Fresh serialized document | forwarded after full success |
| Request-wide input bytes or finding count over the bound, or any core limit code | Rejected; the core produced no text, nothing is truncated | `limit_exceeded` |
| Redacted output not valid, over the output bound, or empty | Rejected; never truncated, never replaced by the original body | `limit_exceeded` / `unsupported_input` |
| Any other core `Err` (detector, policy, placeholder, candidate), worker panic, discarded job, shutdown | Rejected | `incomplete_inspection` |
| Inspection queue or permits exhausted | Rejected before any core work | `overload` |
| Visited text count differs from the request's text count | Rejected | `incomplete_inspection` |

## Precedence

1. Slots are visited in one deterministic order. The first slot that fails decides the error class, and the scope is then poisoned: no later success can mint the completeness proof.
2. Within one leaf, `Block` outranks a rejected `Warn`. Across leaves a `Block` anywhere blocks the whole request; with `forward`, an earlier tolerated warning never masks a later `Block`. `Block` and a rejected `Warn` share one public code, so a client cannot observe which came first.
3. Redaction applies only when every slot completed. A request whose last slot fails returns nothing, whatever earlier slots produced.
4. `on_warn = forward` relaxes exactly one outcome: a `Warn` finding in a redact slot. It does not relax limits, errors, detect-only slots, `Block`, output revalidation, or completeness. There is no incomplete-scan fallback in either mode.

## When redaction cannot preserve provider semantics

The rewritten typed request is revalidated (`ChatRequest::revalidate`) and serialized afresh under the output bound before approval. If a replacement breaks a bound or a derived shape, the request is rejected. The gateway never truncates, never sends the original bytes, and never forwards a partly redacted body. Placeholder text can be longer than the secret it replaces (for example a short address), which is why the bound is checked on the output, not assumed from the input.

## State and ownership

- `ContentPolicy` is parsed once into the immutable `RuntimePlan`; the startup parse also builds one throwaway core registry for the exact profile and PII selection, so an activation the pin cannot build stops the process before it listens.
- `InspectorSpec` (profile, PII selection, limits, warn mode) is `Send + Sync` and shared. `DetectorRegistry` is `!Send + !Sync` in the pin, so each worker thread builds its own from the spec. There is no shared registry and no global lock.
- All request state (placeholder numbering, cumulative bytes and findings, the poisoned flag) lives in a per-request `RequestScope`. Two requests never share numbering. A client-supplied `<SECRET_1>` is plain text; numbering is not a reversible mapping and nothing trusts it.

## Startup rejection

These stop the process with a fixed class and a configuration path, never the offending value: an unknown `profile`; any PII selector the pinned core rejects (`pii:kr` and every national code the pin lacks, malformed selectors, unknown families); more than 32 selectors; `on_warn` other than `reject` or `forward`; `max_findings` outside 1 to 50000; any unknown key under `content` (so `on_block`, `actions`, `policy`, `bypass`, `detectors`, `route`, `upstream`, `authorization` are all refused); an activation the core cannot build.

## Defaults and compatibility

No default changed. `reject` on `Warn` and a 1024 finding bound remain. Two things are new and stricter: the startup parse now builds the core registry once (an unbuildable profile and PII combination fails at startup, where before it failed at worker start), and the action mapping is a single named function (`core_bridge::disposition`) with an exhaustive test. Existing valid configurations behave as before.

## Core contract debts and what each limits

| Core issue | Gateway behavior it limits | Not promised |
| --- | --- | --- |
| [#1177](https://github.com/redact-secret/redact-secret/issues/1177) no cancellation or time bound | A started scan cannot be interrupted, so capacity is held until it really ends (ADR 0004) and a client disconnect does not free CPU early. Request time is bounded only by the input and finding bounds. | Immediate cancellation or a per-request deadline on inspection. |
| [#1178](https://github.com/redact-secret/redact-secret/issues/1178) no `Send + Sync` handle | One registry per worker thread, built from the shared spec; no shared registry. | A shared pool of registries. |
| [#1179](https://github.com/redact-secret/redact-secret/issues/1179) `Ok` completeness not frozen in the contract | Gateway relies on `Ok` meaning a complete scan (verified for beta.12); a pin bump must re-run the action and completeness tests before it is accepted. | Safety across an unreviewed pin change. |
| [#1180](https://github.com/redact-secret/redact-secret/issues/1180) no request-scoped numbering | The gateway offsets numbering itself in `RequestScope`, so numbering is request-wide only because the gateway does it. | Whole-request numbering done by the core. |

None of these blocks the behavior in this contract; each is a documented limit, not a satisfied or waived requirement.
