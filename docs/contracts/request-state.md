# Contract: request states

Status: approved design contract; implementation planned (#2). Rationale: [ADR 0002](../decisions/0002-request-states-and-forwarding-authority.md).

## States and types

| Type | Meaning | Created by | May reach transport |
| --- | --- | --- | --- |
| `ReceivedRequest` | Complete bounded body received on an admitted, exactly matched route. Nothing parsed or trusted | `admission` / receipt path | No |
| `ValidatedRequest` | Parsed (one structure), duplicate-key/UTF-8/structure checked, every field classified recursively for the route's protocol contract | `protocol` via `boundary` | No |
| `SanitizedRequest` | Sealed. Core processing complete, output structurally and size validated, bound to an approved route/authority plan | `boundary` only | Yes, the only accepted type |

Negative-path states (from ARCHITECTURE.md): `Reject` (before any upstream byte) and `Terminate` (after transport begins). Neither produces a replay.

## Rules

1. Transport accepts only `SanitizedRequest`. No overload takes bytes, `http::Request`, or generic JSON values.
2. `SanitizedRequest`: private constructor (within `boundary`), private fields, no `Deserialize`, no `Default`, no public mutation, no conversion from the other two types outside `boundary`.
3. Creation requires all of: recursive supported-field classification; complete core processing (see [core-completeness](core-completeness.md)); structural and output-size validation; association with an approved route/authority plan from the `RuntimePlan`.
4. Wire headers and framing come from the vetted plan plus the immutable body. Outbound length is recomputed. Client framing headers are not copied.
5. No raw-body fallback exists. A request that cannot reach `SanitizedRequest` is rejected.
6. A client header claiming "already scanned" has no effect.

## Limits of this contract

This is a structural safeguard. Types do not prove detector coverage and do not eliminate every security bug.

## Required tests (owners)

- Compile-fail / API-boundary test: unvalidated types cannot call forwarding; `SanitizedRequest` cannot be built or mutated outside `boundary` (#6).
- Runtime no-forward negative tests with a fake upstream: zero upstream body bytes on every rejection (#6, #19, #20).

## Implemented status

Implemented in #2 as a scaffold: the three types, the sealed constructor, and `transport::Upstream::forward(SanitizedRequest)`, with `trybuild` compile-fail tests (`tests/api_boundary.rs`). No route exists and no request can reach upstream. Field classification (#18/#19) and runtime no-forward tests (#6) are not implemented.
