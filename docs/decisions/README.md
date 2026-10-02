# Architecture decision records

ADRs record decisions that fix an interface, ownership, or trust assumption. Contracts under [`docs/contracts/`](../contracts/) state the normative rules that the ADRs rely on.

## Status vocabulary

- **Accepted (design)**: approved by the maintainer as a design contract. This says nothing about implementation. Unless an ADR says otherwise, nothing it describes exists in code yet.
- **Open**: needs a maintainer decision or measured evidence before it can be accepted.
- **Planned vs implemented**: every ADR has an `Implementation status` line. Until the owning issue lands, that line says `planned`. Update it in the PR that implements the behavior.

Type, module, and permit names in these ADRs are the names downstream tasks (#2, #4, #5, #6, #18-#25) should use. They are internal and private. A rename is an ADR amendment, not a drive-by change.

## Index

| ADR | Title | Status | Primary implementation owners |
| --- | --- | --- | --- |
| [0001](0001-repository-and-ownership.md) | Repository, ownership, and independent versioning | Accepted (design) | #2 |
| [0002](0002-request-states-and-forwarding-authority.md) | Request states and forwarding authority (A) | Accepted (design) | #2, #6, #19, #20 |
| [0003](0003-resource-admission-and-lifetime.md) | Resource admission and lifetime (B) | Accepted (design); numbers deferred | #2, #6, #18 |
| [0004](0004-cancellation-and-synchronous-core-work.md) | Cancellation and synchronous core work (C) | Accepted (design); worker choice deferred | #5, #6, #18 |
| [0005](0005-protocol-expansion-and-central-enforcement.md) | Protocol expansion and centralized enforcement (D) | Accepted (design) | #2, #18, #19 |
| [0006](0006-runtime-plan-and-authority-separation.md) | Immutable RuntimePlan and authority separation (E) | Accepted (design) | #2, #4, #23-#25 |
| [0007](0007-parsing-copying-and-plaintext-lifetime.md) | Parsing, copying, and plaintext lifetime (F) | Accepted (design); parser choice deferred | #5, #6, #19 |
| [0008](0008-performance-measurement-gate.md) | Performance measurement gate (G) | Accepted (design); all numbers deferred | #5, #6 |
| [0009](0009-credential-and-upstream-trust-model.md) | Credential and upstream trust model | Accepted (design) | #4, #23-#25 |
| [0010](0010-release-prerequisites-license-and-reporting.md) | License and private security reporting | **Open: needs maintainer** | maintainer |
| [0011](0011-dependency-and-toolchain-selection.md) | Toolchain, dependency, and core pin selection | Accepted (design); implemented (scaffold) | #2, #4, #5, #6, #7 |

## Contracts

| Contract | Covers |
| --- | --- |
| [request-state](../contracts/request-state.md) | ReceivedRequest, ValidatedRequest, SanitizedRequest; sealed final type |
| [core-completeness](../contracts/core-completeness.md) | What counts as complete core inspection; blocker handling |
| [field-classification](../contracts/field-classification.md) | Initial classification rules and rejected forms |
| [resource-limits](../contracts/resource-limits.md) | Resource-limit categories with no numeric defaults |
| [errors-and-telemetry](../contracts/errors-and-telemetry.md) | Safe error code taxonomy and telemetry exclusions |

## Template

Each ADR has: Status, Context, Decision, Owner, Invariants, Failure behavior, Implementation handoff, Verification, Deferred measured choices, Implementation status.

## Linkage to core

Architecture origin: [redact-secret/redact-secret#1001](https://github.com/redact-secret/redact-secret/issues/1001). These ADRs do not satisfy #1001's threat-model or prototype-measurement acceptance criteria. That acceptance is outstanding. It needs the #5 probe results and a reviewed threat model, and neither exists yet.
