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
| [0010](0010-release-prerequisites-license-and-reporting.md) | License and private security reporting | **Decided** (MIT; private reporting enabled) | maintainer |
| [0011](0011-dependency-and-toolchain-selection.md) | Toolchain, dependency, and core pin selection | Accepted (design); implemented (scaffold) | #2, #4, #5, #6, #7 |
| [0012](0012-release-candidate-artifact-build.md) | Release-candidate artifact build (skeleton in #7; Alpha 1 candidate in #22) | Accepted (design); implemented (scaffold; Alpha 1 candidate smoke checks and manifest v2 in #22) | #7, #22 |
| [0013](0013-fixed-https-destinations-and-outbound-authority.md) | Fixed HTTPS destinations and outbound authority | Accepted (design); implemented (destination, client, address policy); body forwarding implemented in #20 (ADR 0017) | #23 |
| [0014](0014-chat-completions-admission.md) | Chat Completions request admission and provisional limits | Accepted; implemented (admission, strict budgeted parse, field matrix); forwarding implemented in #20 (ADR 0017) | #18 |
| [0015](0015-core-inspection-and-request-transformation.md) | Core inspection and validated request transformation | Accepted; implemented (inspection, Block/Warn policy, fresh bounded serialization, approval); forwarding implemented in #20 (ADR 0017) | #19 |
| [0016](0016-header-allowlists-and-request-local-credentials.md) | Header allowlists and request-local provider credentials | Accepted; implemented (inbound vetting, credential type, wire headers, JSON and SSE response relay) | #24 |
| [0017](0017-json-forwarding-deadlines-and-cancellation.md) | Ordinary JSON forwarding, deadlines, response bounds, and cancellation | Accepted; implemented (JSON relay, deadlines, bounds, cancellation, drain); SSE relay in ADR 0018; SDK qualification done in #22 (ADR 0020); numbers provisional | #20 |
| [0018](0018-sse-relay-termination-and-stream-bounds.md) | SSE relay, stream termination contract, and stream bounds | Accepted; implemented (incremental relay, stream permit, idle/lifetime/buffer/write-stall bounds, termination contract, cancellation, stream telemetry); SDK qualification done in #22 (ADR 0020), which corrected the SDK-behavior text; numbers provisional | #21 |
| [0019](0019-request-head-guard-and-one-request-per-connection.md) | Request-head guard and one request per connection | Accepted; implemented (framing-ambiguity guard, head deadline, `Connection: close` on every response); connection-count limits planned (#10) | #25 |
| [0021](0021-framing-ambiguity-parser-level-investigation.md) | Framing ambiguity at the parser level: investigation and a local 400 | Accepted; implemented (the pinned stack cannot expose the ambiguity; the head guard now answers `400` / `431` itself) | #43 |
| [0020](0020-sdk-qualification-test-build.md) | SDK qualification through a separate non-release test build | Accepted by the maintainer; implemented (generated crate with a loopback fake provider, dedicated workflow, eight-file allowlist, seam-absence proof on candidate binaries); supersedes the open item in ADR 0017 | #22 |
| [0022](0022-connection-bound-at-accept.md) | Connection bound enforced at accept time | Accepted; implemented (`max_connections`, immediate close over the bound, slot tied to the connection IO, health not exempt, no per-peer bound); default provisional with a recorded loaded-host measurement | #40 |
| [0023](0023-header-size-measurement-and-size-classes.md) | Request-head size measurement and the size classes | Accepted; implemented (pinned-SDK header measurement, caps confirmed, field-count and head bounds answered by the head guard, one outcome per size class) |
| [0024](0024-connection-reuse-measurement-and-decision.md) | Connection reuse on the local and provider legs: measured, both stay disabled | Accepted; measured, decision recorded (no behavior change); figures provisional on a not-quiet host; criteria and required tests listed for any future re-enable | #42 |
| [0025](0025-alpha2-field-contract.md) | Alpha 2 recursive Chat Completions field contract, block-versus-redact rules, and module boundaries | Accepted (design); contract frozen, tool history implemented (#53), other planned fields rejected until #54 and #55; module split, slot modes, and matrix tests implemented | #52 (then #53, #54, #55, #56) |
| [0026](0026-request-policy-over-pinned-core.md) | Request block/redact policy over the pinned core: the core action set only, four operator keys, startup activation check | Accepted; implemented | #56 |
| [0027](0027-write-budget-and-bounded-response-frames.md) | Cumulative write budget and bounded response frames | Accepted; implemented (#59) | #59 |
| [0028](0028-aggregate-load-qualification.md) | Aggregate load qualification: measurement aids, findings, no change to defaults | Accepted; measured, decision recorded (figures provisional) | #58 |
| [0029](0029-schema-title-keyword.md) | Admit the `title` schema keyword as inspected free text | Accepted | #57 |
| [0030](0030-local-caller-auth-listener-health.md) | Local caller token, listener and health authority: header, syntax, comparison, secret references, ordering, listener combinations | Accepted (design); contract frozen, planned (not implemented) | #61 (then #63, #86, #62, #64, #65) |
| [0031](0031-responses-stateless-text-contract.md) | Responses stateless text field and state contract: mandatory `store:false`, closed state and opaque forms, flat item and tool shapes | Accepted (design); planned, contract frozen, no code | #82 (then #83 to #88) |

## Contracts

| Contract | Covers |
| --- | --- |
| [request-state](../contracts/request-state.md) | ReceivedRequest, ValidatedRequest, SanitizedRequest; sealed final type |
| [core-completeness](../contracts/core-completeness.md) | What counts as complete core inspection; blocker handling |
| [request-policy](../contracts/request-policy.md) | Request-level action and result table over the pinned core; precedence; startup rejection; core debts #1177 to #1180 |
| [field-classification](../contracts/field-classification.md) | Initial classification rules and rejected forms |
| [chat-completions-request](../contracts/chat-completions-request.md) | Endpoint field matrix for the Alpha 1 Chat Completions text subset (implemented) and the frozen Alpha 2 recursive subset (planned), admission rules |
| [responses-request](../contracts/responses-request.md) | Frozen field, item, and state matrix for the stateless Responses text subset (planned, #82) |
| [resource-limits](../contracts/resource-limits.md) | Resource-limit categories; provisional request limits and memory composition |
| [errors-and-telemetry](../contracts/errors-and-telemetry.md) | Safe error code taxonomy and telemetry exclusions |
| [upstream-destinations](../contracts/upstream-destinations.md) | Fixed HTTPS destinations, client hardening, address policy, test-seam isolation |
| [headers-and-credentials](../contracts/headers-and-credentials.md) | Header allowlists, request-local provider credential, framing, response headers, reserved local authority |
| [local-caller-auth](../contracts/local-caller-auth.md) | Planned (frozen design, #61): local caller token header, syntax, comparison, secret references, ordering, listener combinations, health authority |

## Template

Each ADR has: Status, Context, Decision, Owner, Invariants, Failure behavior, Implementation handoff, Verification, Deferred measured choices, Implementation status.

## Linkage to core

Architecture origin: [redact-secret/redact-secret#1001](https://github.com/redact-secret/redact-secret/issues/1001). These ADRs do not satisfy #1001's threat-model or prototype-measurement acceptance criteria. That acceptance is outstanding. It needs the #5 probe results and a reviewed threat model, and neither exists yet.
