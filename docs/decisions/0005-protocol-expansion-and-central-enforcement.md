# ADR 0005: Protocol expansion and centralized enforcement

Status: Accepted (design). Implementation status: planned. Covers issue #3 section D.

## Context

Later endpoints (Responses API, Anthropic) must not each re-implement admission, inspection approval, or forwarding. Duplicated checks drift.

## Decision

Private binary layout (one crate, no public plugin API, no dynamic loader):

| Module | Owns |
| --- | --- |
| `config` | Static configuration parsing and validation into the RuntimePlan |
| `admission` (new) | Capacity, permits, deadlines, overload policy (ADR 0003) |
| `protocol` | Endpoint-specific parsing, field classification, output rules. Internal enums per provider/endpoint |
| `boundary` | Orchestrates inspection; the only creator of `SanitizedRequest` |
| `core_bridge` | Pinned core semantics, completion interpretation, worker execution |
| `transport` | Routing, TLS, headers, credentials, transmission, response relay |
| `health` | Liveness and readiness |
| `telemetry` | Safe, bounded observability |

Authority direction: `protocol` -> `boundary` / `core_bridge` -> `transport`. Protocol modules classify; boundary approves; transport sends.

- Protocol modules do not instantiate HTTP clients or send requests.
- New providers reuse central admission and forwarding checks.
- Use private modules and internal enums initially.

## Owner

Maintainer approves module-boundary changes. #2 scaffolds the modules.

## Invariants

1. Only `transport` holds HTTP clients and credentials.
2. Only `boundary` constructs `SanitizedRequest`.
3. A protocol module cannot reach `transport` without going through `boundary`.
4. Adding a protocol does not add a second forwarding path.

## Failure behavior

A protocol module that cannot classify a field returns an unsupported-input rejection. Nothing falls through to pass-through.

## Implementation handoff

- #2: module skeleton and dependency/module review (no protocol -> transport import; no HTTP client outside transport).
- #18, #19: first protocol (Chat Completions text subset) as an internal enum variant.
- Later epics: Responses API and any new provider need a separate ADR.

## Verification

Module and dependency review in #2; a check (lint rule, grep-based CI check, or visibility test, chosen in #6) that `reqwest`/client construction appears only in `transport`.

## Deferred measured choices

None.
