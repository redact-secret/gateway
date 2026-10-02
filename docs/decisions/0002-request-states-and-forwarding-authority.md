# ADR 0002: Request states and forwarding authority

Status: Accepted (design). Implementation status: types and transport signature implemented in #2 (scaffold; no route). Runtime no-forward tests: #6/#19, and for the wired forwarding path #20 (`src/transport/tests/forward_tests.rs`; `Upstream::forward` now also takes the request-local `VettedHeaders` and an `UpstreamPermit`, but still no body type other than `SanitizedRequest`). Covers issue #3 section A. Normative detail: [request-state contract](../contracts/request-state.md).

## Context

The key security property is that no request-body bytes leave before complete inspection and validated output. A convention ("remember to scan first") is weak. A structural safeguard makes the unsafe path hard to write.

## Decision

Requests move through three types:

`ReceivedRequest` -> `ValidatedRequest` -> `SanitizedRequest`

- Transport accepts only `SanitizedRequest`. It never accepts a raw HTTP request, the original buffer, or a generic JSON value.
- `SanitizedRequest` is sealed. Its constructor is private to the `boundary` module. Fields are private. It has no `Deserialize`, `Default`, `Clone`-with-mutation, or setter API that could forge or alter the approved body.
- `boundary` may create it only after: recursive supported-field classification, complete core processing (see the core-completeness contract), structural and output-size validation, and association with an approved route/authority plan.
- Transport derives wire headers and framing from the vetted route plan plus the immutable body. There is no raw-body fallback.

This is a structural safeguard. It does not prove detector coverage and does not eliminate every security bug.

## Owner

`boundary` module owns construction. `transport` module owns consumption. Maintainer approves any API that widens construction.

## Invariants

1. Only `boundary` can produce `SanitizedRequest`.
2. `SanitizedRequest` has no public mutation, no public constructor, and no general deserialization.
3. `ReceivedRequest` and `ValidatedRequest` have no path to a transport call.
4. The transport API has no overload accepting bytes, `http::Request`, or `serde_json::Value`-like generic values.
5. Outbound `Content-Length` is recomputed from the sealed body. Client framing headers are never copied.

## Failure behavior

Any failed stage yields a gateway-owned safe error (see [errors-and-telemetry](../contracts/errors-and-telemetry.md)) and zero upstream body bytes. A failure after transmission starts is a transport failure, never a replay of the original payload.

## Implementation handoff

- #2: define the three types, module privacy, and the transport signature taking only `SanitizedRequest`.
- #6: compile-fail / API-boundary test (for example `trybuild` or an equivalent private-crate-boundary test; tool chosen in #6) plus runtime no-forward tests against the fake upstream.
- #19, #20: runtime no-forward negative tests for the MVP route.

## Verification

- Compile-fail test: a `ReceivedRequest`, `ValidatedRequest`, raw bytes, or generic JSON passed to the transport entry point fails to compile. Constructing `SanitizedRequest` outside `boundary` fails to compile.
- Runtime: every rejection path leaves the fake upstream with zero recorded body bytes.

## Deferred measured choices

None. Names are fixed for scaffolding by the ADR index.
