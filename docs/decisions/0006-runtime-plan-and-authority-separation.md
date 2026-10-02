# ADR 0006: Immutable RuntimePlan and authority separation

Status: Accepted (design). Implementation status: `RuntimePlan` type in #2; static JSON parsing, strict validation, loopback listener rule, CLI, and readiness implemented in #4 (see [configuration](../configuration.md)); core initialization stub until #5. Covers issue #3 section E.

## Context

Rereading config per request, or sharing a global mutable scanner behind one lock, creates drift and serializes all traffic. Mixing deployment, content, and resource settings lets a content-profile change weaken destination rules.

## Decision

At startup, build one validated immutable `RuntimePlan` containing: route-to-origin mapping, protocol contracts, core profile and policy, limits and deadlines, and shared transport clients. Do not reread or reparse config per request. Do not use a global mutable scanner or session behind a request-serializing mutex.

Three authorities are separate parts of the plan:

| Authority | Contents |
| --- | --- |
| Deployment authority | Listener, upstream origins, TLS rules, credential requirements |
| Content policy | Core profiles and actions |
| Resource policy | Limits, deadlines, capacities |

- A content-policy change cannot alter upstream, listener, TLS, or credential requirements.
- Clients cannot select policies through payload or header values. Any selection uses operator-defined route bindings.
- Per-request state and provider credentials stay out of shared client default headers.
- Use only supported public core initialization APIs. `Policy::compile()` is not an assumed existing dependency. #5 verifies what the exact pin offers.
- Configuration is static and restart-activated. No hot reload.

## Owner

`config` builds the plan. #4 owns validation and the CLI. Maintainer approves new plan fields.

## Invariants

1. The `RuntimePlan` is immutable after startup and shared by reference.
2. Request-local mutable state (session, placeholders) is created per request and never shared.
3. Credentials are never stored in a client default header.
4. Invalid authority/policy combinations fail before traffic is accepted.

## Failure behavior

Invalid config: process refuses to start or reports not ready; diagnostics reveal no config secrets. If core initialization fails, readiness is false and proxy routes reject.

## Implementation handoff

- #2: `RuntimePlan` type and module placement.
- #4: validation, unknown-field rejection, loopback default, readiness tied to plan plus core initialization. Test that repeated requests do not trigger config parsing or client initialization.
- #5: core initialization and reuse capabilities.
- #23-#25: route and credential handling uses the plan.

## Verification

#4 tests listed in its issue; review that no client default header carries a credential.

## Deferred measured choices

Core session/state reuse strategy (depends on #5 findings).
