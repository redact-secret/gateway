# Contract: `POST /v1/chat/completions` request matrix (Alpha 1 text subset)

Status: implemented for admission, parsing, and classification (#18) and for core inspection and transformation (#19, [ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md)). A request that passes everything below and is inspected and approved is forwarded once (#20, [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md)) and the provider's JSON response is relayed. `stream: true` follows the same road (admission, parsing, inspection, approval) and the provider's event stream is then relayed incrementally (#21, [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md)); every rejection sends no upstream byte, and a `stream: true` request from an HTTP/1.0 caller is rejected locally. Implementation: `src/chat_route.rs` (HTTP admission), `src/protocol/json.rs` (strict budgeted parse), `src/protocol/chat.rs` (this matrix). Rationale: [ADR 0014](../decisions/0014-chat-completions-admission.md), [ADR 0005](../decisions/0005-protocol-expansion-and-central-enforcement.md), [ADR 0007](../decisions/0007-parsing-copying-and-plaintext-lifetime.md). Parent rules: [field-classification](field-classification.md), [request-state](request-state.md). Limits: [resource-limits](resource-limits.md). Errors: [errors-and-telemetry](errors-and-telemetry.md).

This file and `protocol/chat.rs` must agree. The unit tests in `chat.rs` and `tests/chat_admission.rs` carry one case per row marked "rejected".

## Request admission (before any body byte is read)

Checked in this order; each failure is a fixed local response and reserves nothing.

| Check | Rule | Rejection |
| --- | --- | --- |
| Route | Exact path `/v1/chat/completions`. No trailing slash, no case folding, no percent-decoding. | `404 unsupported_input` |
| Method | `POST` only. Every other method on the route is refused with `Allow: POST`; `CONNECT` and other non-origin-form targets never match the route. | `405 unsupported_input` |
| Target | No query string (an empty `?` also counts), no absolute-form target, no `Upgrade`. | `400 unsupported_input` |
| `Content-Type` | Exactly one header: `application/json`, case-insensitive, optional `charset=utf-8` and nothing else. | `415 unsupported_input` |
| Compression | Any `Content-Encoding` (even `identity`) and any transfer coding other than a lone `chunked`. | `415 unsupported_input` |
| Framing | One all-digit `Content-Length` or `Transfer-Encoding: chunked`, not both (both is refused by a connection-level guard before the HTTP layer, answered with a local `400 malformed_input` and the connection closed; ADR 0019, ADR 0021); absent or zero length. Malformed framing the HTTP layer itself refuses is a bare `400` from the HTTP layer. | `400 malformed_input` |
| Size | Declared `Content-Length` over the effective maximum body (see limits). | `413 limit_exceeded` |

Then receipt capacity and a conservative memory reservation are taken (ADR 0003), the body is collected under a deadline and a hard byte bound, and the body is parsed. `Content-Length` is only ever an upper bound that collection enforces; it never decides how much memory is reserved beyond what the limits already cap.

## Parsing rules

One pass, one working structure ([ADR 0007](../decisions/0007-parsing-copying-and-plaintext-lifetime.md)):

- The body must be exactly one JSON document. Invalid UTF-8, a byte-order mark, trailing bytes, comments, non-finite numbers, and malformed escapes are rejected (`malformed_input`).
- Strings and object keys are decoded before any other check: `\"`, `é`, surrogate pairs, and literal Unicode (including Korean) all become the same decoded text. A lone or mismatched surrogate escape is rejected. Escaped keys decode first, so `"model"` is the key `model`.
- Duplicate keys at any depth are rejected after decoding, so `"model"` and `"model"` collide.
- Budgets are enforced while the tree is built: container depth, node count (values plus keys), decoded bytes per string, and decoded bytes in total. Exceeding one is `limit_exceeded`, never truncation.
- Integer fields require an integer literal. `1.0`, `1e3`, and integers beyond `i64` are rejected rather than coerced.

## Field matrix

Each field is exactly one of: **text** (decoded string sent through core inspection by #19), **structural** (validated by an explicit contract and transmitted as is), or **rejected**. Anything not listed is rejected, at every depth.

### Top level

| Field | Class | Contract | Why |
| --- | --- | --- | --- |
| `model` | structural | Required string, 1 to 128 bytes, ASCII alphanumeric first, then alphanumerics or `. _ : / @ + -`. Transmitted verbatim. | Selects a provider model; the character set rules out whitespace, quotes, and non-ASCII text. A 128-byte identifier can still carry a short value, so it is not a trusted channel (see residual risks). |
| `messages` | text | Required array, 1 to `max_messages` entries (default 256), each a message object (below). | The prompt. |
| `stream` | structural | Optional boolean. | Selects JSON or SSE response; both are relayed (#20, #21). |
| `stream_options` | structural | Optional object with only `include_usage` (boolean). | Bounded control. |
| `temperature` | structural | Optional number, `0` to `2`. | Sampling control. |
| `top_p` | structural | Optional number, `0` to `1`. | Sampling control. |
| `presence_penalty`, `frequency_penalty` | structural | Optional number, `-2` to `2`. | Sampling control. |
| `max_tokens`, `max_completion_tokens` | structural | Optional integer, `1` to `2147483647`. | Output bound; the provider applies model-specific limits. |
| `n` | structural | Optional integer, exactly `1`. | Larger values multiply response size and are outside the Alpha 1 response bounds. |
| `seed` | structural | Optional integer in the `i64` range. | Reproducibility control. |
| `stop` | text | Optional string, or array of 1 to 4 strings. | Application-authored text sent to the model. |
| `user` | text | Optional string, at most 256 bytes. | End-user identifier; commonly an email or ID, so it is inspected, not trusted. |
| `response_format` | structural | Optional object with only `type`, one of `text` or `json_object`. | `json_schema` carries schema text and is rejected. |

### Messages

| Field | Class | Contract |
| --- | --- | --- |
| `role` | structural | Required, one of `system`, `developer`, `user`, `assistant` (exact, lowercase). |
| `content` | text | Required. Either a string (any decoded text, including empty), or an array of 1 to 64 text parts. `null`, numbers, booleans, and objects are rejected. |

A message has no other key. In particular `name`, `tool_calls`, `tool_call_id`, `function_call`, `refusal`, `audio`, and `annotations` are rejected.

### Content parts

A part is exactly `{"type":"text","text":"<string>"}`. `type` must equal `text` (exact), `text` is inspected text, and any other key is rejected. The array form is kept as an array so serialization preserves the type the caller chose.

### Deliberately unsupported (all `unsupported_input`, HTTP 422)

| Form | Reason |
| --- | --- |
| `tools`, `tool_choice`, `parallel_tool_calls`, `functions`, `function_call` | Tool descriptions and schemas are text the core boundary does not yet classify. Wider tool coverage is Alpha 2 (#9). |
| Messages with role `tool` or `function`, assistant `tool_calls` | App-submitted tool arguments need their own contract ([field-classification](field-classification.md)); blind nested JSON parsing is not allowed. |
| Content parts of type `image_url`, `input_audio`, `file`, `refusal`, or any other type; `audio`, `modalities`, `prediction`, `web_search_options` | Binary, remote, or provider-stored content cannot be inspected through the text boundary. |
| `content: null` and non-string, non-array `content` | Opaque or unclassifiable content. |
| `name` on a message | Participant names are arbitrary identity text; they would need either inspection or a safe pattern, and Alpha 1 does neither. |
| `metadata`, `logit_bias`, `store`, `service_tier`, `reasoning_effort`, `logprobs`, `top_logprobs`, `response_format.json_schema` | Arbitrary key-value or schema text, or features with response shapes outside Alpha 1 relay bounds. |
| `n` other than `1` | See `n` above. |
| Any unknown key at any depth | Unknown nested fields get the same classification discipline as top-level fields. |

Unsupported means rejected, never stripped: the gateway does not drop fields and forward the rest.

## Typed boundary representation

A request that satisfies the matrix becomes `protocol::chat::ChatRequest`, held inside `protocol::ValidatedRequest` together with its `MemoryReservation` and `ReceiptPermit`. The original body buffer is already released. `ChatRequest` has private fields; text is reachable only through `for_each_text` / `for_each_text_mut`, in this deterministic order: `messages` in order (parts in order), then `stop`, then `user`. A later stage can replace text but cannot add keys, change roles, or alter structure. `chat_route::Admitted` pairs the `ValidatedRequest` with the operator-defined `RouteId` (`openai.chat_completions`), which is never derived from the request. `SanitizedRequest` is not constructed here; only `boundary` creates it, after inspection ([ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md)).

## Inspection and the outbound body (#19)

Every inspected text goes through the pinned core in the order above, with request-wide placeholder numbering. The outbound body is a fresh document serialized from the typed request (not the original bytes): the same keys, value types, and array order, with canonical key order inside objects (`model`, `messages`, `stream`, `stream_options`, the numeric controls, `stop`, `user`, `response_format`; within a message `role` then `content`). Strings are escaped by the JSON writer and Unicode (including Korean) is emitted as UTF-8. Output is bounded by `min(max_body_bytes, reservation bytes)`; over the bound is `413 limit_exceeded`. Object keys are fixed schema names and are not inspected; no free-text key is admitted (`metadata` and `logit_bias` are rejected above). `model` is checked detect-only and never rewritten.

## Residual risks

- `model` and numeric fields are transmitted unchanged. Their constraints limit what can ride in them but do not make them secret-free channels.
- The matrix controls structure, not detection. Whether text contains a secret is core's decision (#19).
- Unknown-field rejection can break SDK calls that send extra fields (for example replaying an assistant message with `refusal: null`). That is deliberate for Alpha 1.
