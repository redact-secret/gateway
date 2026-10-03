# Contract: request states

Status: implemented (the sealed request types from Alpha 1; the closed `Chat`/`Responses` dispatch, #83). Rationale: [ADR 0002](../decisions/0002-request-states-and-forwarding-authority.md).

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

Implemented in #2 as a scaffold: the three types, the sealed constructor, and `transport::Upstream::forward(SanitizedRequest)`, with `trybuild` compile-fail tests (`tests/api_boundary.rs`). Runtime no-forward tests use the fake upstream (#6).

#18 adds the first two states on a real route: `admission::ReceivedRequest` is produced by `Admission::begin_body_receipt` (reservation before collection) and `protocol::validate_with` turns it into a `ValidatedRequest` holding the typed `protocol::chat::ChatRequest`; `chat_route::Admitted` pairs that with the operator-defined `RouteId`. The route then ends in a local `501 not_implemented`: nothing constructs a `SanitizedRequest` and no request reaches upstream. Core inspection, `boundary::approve`, and forwarding were left to #19/#20.

#19 adds `boundary::Inspection::inspect_and_approve(ValidatedRequest, RouteId) -> Result<SanitizedRequest, BoundaryError>`, the only path to a `SanitizedRequest`: inspect on the worker pool (the job owns the permit and memory), serialize a fresh bounded document, mint `CompleteInspection` through `RequestScope::finish`, then `boundary::approve`. `ChatRoute::handle` runs it and then still answers `501 not_implemented` (the approved value is dropped locally); #20 replaces that last step with transport. See [ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md).

#20 wires it: `ChatRoute` passes the `SanitizedRequest`, the request-local `VettedHeaders`, and an independently acquired `UpstreamPermit` to `Upstream::forward(SanitizedRequest, VettedHeaders, UpstreamPermit)`, the only entry point; no body type other than the sealed request is accepted and no fallback exists. The sealed request (and its memory reservation) outlives the exchange; the permit outlives the buffered response body. #21 adds the sibling `Upstream::forward_stream(SanitizedRequest, VettedHeaders, UpstreamPermit, StreamPermit)`: the same sealed request and request-local credential, plus an independently acquired stream permit; there is no bytes or JSON overload and no fallback. The request (and its memory reservation) ends with the provider's response headers; the upstream and stream permits belong to the response body until the stream ends. See [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md) and [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md).

#83 (epic #13) makes the typed request a closed internal enum. `ValidatedRequest` holds a `protocol::RequestBody` (`Chat` or `Responses`), and `boundary` reaches slot traversal (per-slot `SlotMode`), revalidation and bounded serialization only through its methods, so both endpoints share one inspection and approval implementation with no raw fallback. `inspect_and_approve` and `approve` take a `boundary::ProtocolRoute` (a `protocol::Protocol` paired with the operator-defined `RouteId`, built where the route is built from the startup plan, never from the request). A validated request of the other protocol is refused with `BoundaryError::RouteMismatch` (`500 incomplete_inspection`) before a permit or worker is spent, so a Responses payload cannot be sealed for the Chat route or the reverse; the `SanitizedRequest` records the `Protocol` it was sealed under. `protocol::responses::ResponsesRequest` was a minimal skeleton at #83; the Responses classifier (#84, #85), the route (#86) and the relay (#87) have since landed on this same dispatch and the whole subset is qualified (#88). The enum is private to the crate's design, not a plugin point.
