# Contracts

Normative rules for interfaces and behavior. Each contract has a status line that says whether it is a planned design or implemented. Rationale lives in [`docs/decisions/`](../decisions/README.md).

- [request-state](request-state.md)
- [core-completeness](core-completeness.md)
- [field-classification](field-classification.md)
- [chat-completions-request](chat-completions-request.md)
- [resource-limits](resource-limits.md)
- [errors-and-telemetry](errors-and-telemetry.md)
- [upstream-destinations](upstream-destinations.md)

Credential and upstream trust is recorded in [ADR 0009](../decisions/0009-credential-and-upstream-trust-model.md); destination and client policy in [ADR 0013](../decisions/0013-fixed-https-destinations-and-outbound-authority.md); Chat Completions admission and limits in [ADR 0014](../decisions/0014-chat-completions-admission.md).

A contract-changing PR updates the contract and its ADR together.
