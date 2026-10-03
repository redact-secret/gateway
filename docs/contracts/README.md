# Contracts

Normative rules for interfaces and behavior. Each contract has a status line that says whether it is a planned design or implemented. Rationale lives in [`docs/decisions/`](../decisions/README.md).

- [request-state](request-state.md)
- [core-completeness](core-completeness.md)
- [request-policy](request-policy.md)
- [field-classification](field-classification.md)
- [chat-completions-request](chat-completions-request.md)
- [responses-request](responses-request.md)
- [resource-limits](resource-limits.md)
- [errors-and-telemetry](errors-and-telemetry.md)
- [upstream-destinations](upstream-destinations.md)
- [headers-and-credentials](headers-and-credentials.md)
- [local-caller-auth](local-caller-auth.md) (implemented for Chat Completions, #63)
- [config-upgrade-rollback](config-upgrade-rollback.md) (restart activation, migration and rollback pairs)
- [config-schema](config-schema.md) (frozen, implemented; schema file `docs/schema/gateway-config.v1.schema.json`)
- [request-lifecycle](request-lifecycle.md)

Credential and upstream trust is recorded in [ADR 0009](../decisions/0009-credential-and-upstream-trust-model.md); destination and client policy in [ADR 0013](../decisions/0013-fixed-https-destinations-and-outbound-authority.md); Chat Completions admission and limits in [ADR 0014](../decisions/0014-chat-completions-admission.md); header allowlists and request-local credentials in [ADR 0016](../decisions/0016-header-allowlists-and-request-local-credentials.md); JSON forwarding and cancellation in [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md); the SSE termination contract and stream bounds in [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md).

The stage-by-stage cancellation, deadline, cleanup-owner, and shutdown contract is [request-lifecycle](request-lifecycle.md) ([ADR 0027](../decisions/0027-write-budget-and-bounded-response-frames.md)).

The Alpha 2 recursive Chat Completions field contract (tool history, tool definitions, schemas, metadata; all implemented, #53 to #55) and its module ownership are in [ADR 0025](../decisions/0025-alpha2-field-contract.md).

The Beta 1 stateless Responses text subset (`POST /v1/responses`; frozen design; the #84 text subset is implemented behind an unrouted path, the rest lands in #85 to #88) is the [responses-request](responses-request.md) contract and [ADR 0031](../decisions/0031-responses-stateless-text-contract.md): `store:false` is mandatory, stored and opaque state is rejected, and this is not generic OpenAI compatibility.

Alpha 1 qualification evidence (the threat-control map, stack pins, residual risks) is in [`docs/qualification/`](../qualification/alpha1-threat-control-map.md); the head guard and one-request-per-connection rule are in [ADR 0019](../decisions/0019-request-head-guard-and-one-request-per-connection.md).

The local caller token, the listener combinations it adds, and the health/probe authority split are specified in [local-caller-auth](local-caller-auth.md) ([ADR 0030](../decisions/0030-local-caller-auth-listener-health.md), #61) and enforced by #63; #86 reuses the same boundary for the Responses route.

A contract-changing PR updates the contract and its ADR together.
