# ADR 0031: Responses stateless text field and state contract

Status: Accepted (design), by maintainer delegation to the #82 author within the Beta 1 decomposition authorized for epic #13. Implementation status: **request subset, route, fixed destination and caller-auth boundary implemented (#83 to #86)**; relay lifecycle (#87) and qualification (#88) planned. This ADR and its contract freeze the request subset. Date: 2026-10-03. Builds on [ADR 0005](0005-protocol-expansion-and-central-enforcement.md), [ADR 0015](0015-core-inspection-and-request-transformation.md), [ADR 0025](0025-alpha2-field-contract.md). Contract: [responses-request](../contracts/responses-request.md). Issue: #82 (epic #13). Coordinates with #61 (caller-token authority); unblocks #62 (schema freeze), #83 to #88.

## Context

Beta 1 adds `POST /v1/responses`. Responses differs from Chat Completions in the ways that matter to a boundary that must see every text byte: input is a string or a heterogeneous item list, assistant output objects can be replayed as input, provider state can be referenced instead of sent (`previous_response_id`, `conversation`, `prompt`, `item_reference`), `store` defaults to true when omitted, reasoning items carry encrypted opaque content, and tools and structured outputs have flat shapes of their own. The pinned SDK request types (Node.js `openai` 7.27.0, Python `openai` 3.24.0) expose all of these, and a default SDK call omits `store`.

## Decisions

| # | Decision | Why |
| --- | --- | --- |
| R1 | The subset is stateless inspectable text: `instructions`, string or text-message `input`, app-submitted `function_call` and `function_call_output` items, function tools, `tool_choice`, `text.format` (`text`, `json_object`, flat `json_schema`), `metadata`, and a short list of numeric and boolean controls. Everything else is rejected at every depth. | Each accepted position has a text, label, or structural class and secret-placement tests. Anything the gateway cannot inspect cannot be admitted. |
| R2 | **`store` must be present and exactly `false`.** Omission, `null`, and `true` are rejected (`422 unsupported_input`). The gateway never normalizes omission to `false`. | Normalizing would make the forwarded body differ from what the caller sent in a field whose default is provider-side persistence, and a caller that forgot `store` would be told nothing. Rejecting makes the choice explicit in the application code; both pinned SDKs can send it (`store=False`, `store: false`). The cost is one required argument. If a later ADR adds documented normalization, it must also define the serialization and a visible signal to the caller. |
| R3 | `store:false` is not a retention guarantee, and the contract, README, and examples say so. | Provider retention for abuse monitoring or legal reasons and zero-data-retention arrangements are outside what the gateway can observe or enforce. |
| R4 | Stored and referenced state is rejected: `previous_response_id`, `conversation`, `prompt`, `item_reference`, `include`, `background`, `truncation`, `context_management`. Multi-turn use is the application resending text history. | A reference makes the provider inject content the gateway never inspected. |
| R5 | Opaque and non-text forms are rejected: `reasoning` items and controls, `include` (so no `reasoning.encrypted_content`), image, file, and audio parts, remote and hosted tools, and every hosted-tool item. | Encrypted or binary content cannot go through the core. |
| R6 | A rejected field is rejected whatever its value (`null`, `false`, `[]`, `{}`, a string). `null` is rejected on accepted fields too, except `tools[].parameters` and `tools[].strict`, where the pinned SDK types make the key required and nullable. | An accepted "empty" spelling is an unreviewed shortcut. The two exceptions are needed so a typed client can describe a no-argument function. |
| R7 | Provider output objects are not valid input. Items accept only the listed keys; `id`, `status`, `annotations`, `logprobs`, `caller`, `namespace`, `async` are rejected. Assistant replay is the easy message form with string content (plus optional `phase`). | Output objects carry provider handles and citation data (file ids, URLs). An application rebuilds turns from text. SDK loops that replay `response.output` unchanged are refused; documented as a residual risk. |
| R8 | Function calls reuse the Alpha 2 rules: `call_id` is a LINK label (unique across calls), `name` a NAME label, `arguments` the only decoded JSON string (strict, object root, key labels, text leaves, derived budgets, compact re-encoding). Outputs must follow their call (not necessarily adjacent), at most once; unanswered calls are not enforced. `output` is plain text or `input_text` parts and is never parsed. | Same reasons as ADR 0025 D3, D4, D9. Adjacency is not required because Responses clients normally append all outputs after all parallel calls. |
| R9 | Structural strings are labels (charset, length, detect-only scan, never rewritten); free text is redacted in place; no structural field carries free text. Label findings reject. Schema subset, `tool_choice` set, and metadata rules are the Chat contract's, re-expressed in the flat Responses shapes (`{"type":"function","name":N}`; `text.format` with `name` and `schema` at the same level). | Reuses reviewed principles without copying Chat field shapes. |
| R10 | Slot order is `instructions`, `input` (items in order), `tools`, `tool_choice`, `text.format`, `metadata`. Placeholder numbering is request-wide in that order, starts at 1 per request, and is not consumed by label slots. Revalidation after mutation is the Chat step. | Deterministic traversal; no cross-request scope. |
| R11 | No new numeric limit is introduced. Item count uses `max_messages`; body, node, depth, derived budgets, and output bound are the existing ones. Part, tool, property, enum, and `anyOf` counts equal the Chat values. | Finite per-item and request-wide budgets without a second operator surface before #62 freezes the schema. #84 and #85 may only tighten, never add a key, without amending this ADR. |
| R12 | Unknown and newly added fields are default-rejected. | The compatibility tradeoff is accepted and documented: newer SDK fields fail until reviewed. |

## Invariants

1. No request-body bytes upstream until complete bounded parsing, inspection (label scans included), transformation, and revalidation succeed. No raw fallback.
2. A label is never rewritten. Rejection never strips.
3. Every accepted field is classified or rejected; every text and label slot has a secret-placement test; no structural field is a text bypass.
4. Only `function_call.arguments` is parsed as JSON.
5. `store:false` is always written upstream and always came from the caller.
6. Errors, `Debug`, logs, and telemetry carry no field name, value, or finding.
7. Detection stays core-owned; response content remains unredacted relay.

## Failure behavior

As [ADR 0025](0025-alpha2-field-contract.md): `422 unsupported_input` for unsupported shapes, rejected fields, correlation errors, and label findings; `400 malformed_input` for duplicate keys and malformed arguments; `413 limit_exceeded` for budgets and bounds; `500 incomplete_inspection` for revalidation failure.

## Implementation handoff

- #83: typed dispatch for a second request kind (private enum, no plugin interface); the route stays unrouted until #86.
- #84: `instructions`, `input` string and message items, `store` rule, slot order for those positions, and their secret-placement tests.
- #85: function-call items, outputs, tools, `tool_choice`, `text.format`, metadata, and their tests.
- #86: fixed destination, route id, and shared caller-auth boundary (#61, #63). #82 adds no config key; the route and its destination are defined there.
- #87: JSON and SSE relay lifecycle with Responses terminal events. #88: qualification of every accepted and rejected form with the pinned SDKs and the Beta 1 endpoint matrix.
- Internal names for the owning issues: module `protocol::responses` (parallel to `protocol::chat`), route id `openai.responses` (parallel to `openai.chat_completions`). A rename is an amendment.

## Verification

`tests/responses_contract.rs` pins that the contract's examples are valid JSON and obey the `store` rule, that every rejected top-level field family in R4 to R6 is listed, and that the route is still unrouted (flipped by #86). Positive and negative behavior tests belong to #84 to #88.

## Deferred measured choices

All numeric bounds are the provisional Chat values (ADR 0008). Exact decimal preservation follows ADR 0025 D10.

## Consequences and limits

Requiring `store:false` makes the first-call experience stricter than the provider's. Rejecting provider output objects and reasoning items means tool loops with reasoning models that need encrypted reasoning replay are outside this subset. Rejecting `user`, `safety_identifier`, and prompt-cache fields removes some provider optimizations; each can be admitted later with a label or text rule.

## Implementation status

Contract frozen by #82. Implemented so far: the #83 dispatch, the #84 text subset and the #85 function-call, tool, structured-output and metadata subset (see the implementation notes). The route, fixed destination and caller-auth boundary are implemented (#86); relay lifecycle (#87) and qualification (#88) are planned.

## Implementation note (#83)

The typed dispatch is in place: `protocol::RequestBody::{Chat, Responses}`, `protocol::Protocol::ResponsesText` (canonical route id `openai.responses`), and `boundary::ProtocolRoute` binding a protocol to its route at approval. `protocol::responses::ResponsesRequest` is a skeleton that carries only `model`, `instructions` and string `input` so the shared inspection path is exercised; the classifier (#84), remaining slots (#85), route and destination (#86) and relay (#87) are not implemented, and `POST /v1/responses` is still unrouted.

## Implementation note (#84)

`protocol::validate_with` now parses `Protocol::ResponsesText` bodies (one strict budgeted parse, then `protocol::responses::classify`). Implemented: `model`, required `store:false`, `instructions`, `input` as a string or text message items (`role` of `user`, `assistant`, `system`, `developer`; string content, or `input_text` parts for non-assistant roles; assistant `phase`), and the controls `stream`, `stream_options.include_obfuscation`, `temperature`, `top_p`, `max_output_tokens`. Every other field, item type and part type, including everything #85 owns (`function_call` and `function_call_output` items, `tools`, `tool_choice`, `parallel_tool_calls`, `text`, `metadata`), is `422 unsupported_input` until its parser lands. Slot order is `instructions`, then `input`; the outbound document is serialized fresh with `store:false` always written. `POST /v1/responses` is still unrouted (#86); the behavior is tested at the protocol and boundary layers (`tests/responses_text.rs`). Replaying conversation history is explicit re-submission of supported text items; there is no previous-response reference.

### Implementation notes (#85)

`protocol::responses` now accepts the `function_call` and `function_call_output` input items, function `tools` (flat shape, `parameters` and `strict` required keys that may be `null`), `tool_choice`, `parallel_tool_calls`, `text` (`format` of `text`, `json_object` or the flat `json_schema`, and `verbosity`) and `metadata`. Reuse, not duplication: `function_call.arguments` goes through the Chat `ToolCall` (strict duplicate-key-rejecting parse, decoded tree, label digest, compact re-encoding); tool and format schemas use the Chat schema subset (`protocol::chat::schema`); metadata is the Chat `Metadata`. A single `Derived` counter is created per `classify` call, so arguments, tool schemas, format schemas and metadata share one request-wide node and byte budget regardless of JSON key order. Correlation (unique `call_id` across calls, an output after its call, each call answered once) is checked at parse time and again at revalidation. New slot variants are `CallId`, `CallName`, `CallArgumentKey` (detect-only), `CallArgumentText`, `Output` (redact), `OutputCallId`, `ToolName`, `ToolSchemaLabel`, `ToolChoiceName`, `FormatName`, `FormatSchemaLabel`, `MetadataKey` (detect-only) and `ToolDescription`, `ToolSchemaText`, `FormatDescription`, `FormatSchemaText`, `MetadataValue` (redact). No new error outcome and no new operator key. Tests: `tests/responses_tools.rs`. SDK helper limitations and safe replay conversion are in the contract.

### Implementation notes (#86)

`chat_route::EndpointRoute` is the single route handler; `ChatRoute` and `ResponsesRoute` are aliases of it, each bound at startup to one `Protocol` (`EndpointRoute::new` for Chat, `EndpointRoute::responses` for Responses, `for_protocol` for both). The protocol fixes the exact inbound path (`chat_route::path_for`: `/v1/chat/completions` or `/v1/responses`), the request matrix passed to `protocol::validate_with`, and the `ProtocolRoute` handed to `inspect_and_approve`, so a body of the other protocol is a local `422`/`400` and a `RouteMismatch` cannot be forged. The reviewed route table now holds `openai.responses` -> `POST https://api.openai.com/v1/responses` beside Chat on the same origin; the route id comes from `transport::destination::OPENAI_RESPONSES_ROUTE`, never a request. `Services::init` builds both routes over one admission, one inspection service, one transport, one metrics owner, and the plan's `local_auth`; shutdown cancels both. Local caller auth, head/target/framing admission, header vetting (local-auth headers are not on the outbound allowlist), upstream permit, no-retry and no-replay are the shared code, not a copy. Tests: `src/transport/tests/responses_route_tests.rs` (two fake providers prove one send to the Responses origin only, credential isolation, no local authority upstream, zero bytes on every rejection), `tests/responses_route.rs`, and the flipped `tests/responses_contract.rs` and `tests/config_schema.rs` route-table checks. The Responses relay semantics and stream terminal events are #87.
