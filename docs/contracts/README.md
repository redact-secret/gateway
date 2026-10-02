# Contracts

Normative rules for interfaces and behavior. Each contract has a status line that says whether it is a planned design or implemented. Rationale lives in [`docs/decisions/`](../decisions/README.md).

- [request-state](request-state.md)
- [core-completeness](core-completeness.md)
- [field-classification](field-classification.md)
- [chat-completions-request](chat-completions-request.md)
- [resource-limits](resource-limits.md)
- [errors-and-telemetry](errors-and-telemetry.md)
- [upstream-destinations](upstream-destinations.md)
- [headers-and-credentials](headers-and-credentials.md)

Credential and upstream trust is recorded in [ADR 0009](../decisions/0009-credential-and-upstream-trust-model.md); destination and client policy in [ADR 0013](../decisions/0013-fixed-https-destinations-and-outbound-authority.md); Chat Completions admission and limits in [ADR 0014](../decisions/0014-chat-completions-admission.md); header allowlists and request-local credentials in [ADR 0016](../decisions/0016-header-allowlists-and-request-local-credentials.md); JSON forwarding and cancellation in [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md); the SSE termination contract and stream bounds in [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md).

Alpha 1 qualification evidence (the threat-control map, stack pins, residual risks) is in [`docs/qualification/`](../qualification/alpha1-threat-control-map.md); the head guard and one-request-per-connection rule are in [ADR 0019](../decisions/0019-request-head-guard-and-one-request-per-connection.md).

A contract-changing PR updates the contract and its ADR together.
