# Contract: `POST /v1/chat/completions` request matrix (Alpha 1 text subset implemented; Alpha 2 recursive subset frozen)

Status: implemented for admission, parsing, and classification (#18) and for core inspection and transformation (#19, [ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md)). A request that passes everything below and is inspected and approved is forwarded once (#20, [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md)) and the provider's JSON response is relayed. `stream: true` follows the same road (admission, parsing, inspection, approval) and the provider's event stream is then relayed incrementally (#21, [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md)); every rejection sends no upstream byte, and a `stream: true` request from an HTTP/1.0 caller is rejected locally. **Alpha 2 (#52): the matrix below is the frozen recursive field contract for tool history, tool definitions, structured-output schemas, and metadata, with the block-versus-redact rules ([ADR 0025](../decisions/0025-alpha2-field-contract.md)). Rows labeled Planned #NN are not implemented: they are rejected like any unknown field until the named issue lands.** Implementation: `src/chat_route.rs` (HTTP admission), `src/protocol/json.rs` (strict budgeted parse), `src/protocol/chat/` (this matrix, one file per owner; see the module map in ADR 0025). Rationale: [ADR 0014](../decisions/0014-chat-completions-admission.md), [ADR 0005](../decisions/0005-protocol-expansion-and-central-enforcement.md), [ADR 0007](../decisions/0007-parsing-copying-and-plaintext-lifetime.md). Parent rules: [field-classification](field-classification.md), [request-state](request-state.md). Limits: [resource-limits](resource-limits.md). Errors: [errors-and-telemetry](errors-and-telemetry.md).

This file and `protocol/chat/` must agree. `tests/chat_field_matrix.rs` carries one synthetic example per row class (accepted, rejected, and planned), the unit tests in `src/protocol/chat/mod.rs` and `tests/chat_admission.rs` carry the parser, serializer, and zero-upstream cases.

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

Each field is exactly one of: **text** (a decoded string the core may redact in place), **label** (a structural string, such as an identifier, key, or enum value, that is constrained by a charset and a length and that the core only scans: any finding rejects the request and the string is never rewritten), **structural** (validated by an explicit contract and transmitted as is), or **rejected**. Anything not listed is rejected, at every depth, and unsupported means rejected, never stripped.

Status column. **Implemented** rows exist in code and tests today. **Planned #NN** rows are the frozen Alpha 2 contract: they are rejected as unknown (`unsupported_input`, zero upstream bytes) until the named issue lands and flips its rows in `tests/chat_field_matrix.rs`. A planned row is not a supported feature ([ADR 0025](../decisions/0025-alpha2-field-contract.md)).

### Shared constraints

| Name | Rule |
| --- | --- |
| NAME | 1 to 64 bytes of `[A-Za-z0-9_-]`. Tool and function names, response-schema names, schema property keys, `required` entries, and keys inside decoded tool arguments. |
| LINK | 1 to 64 bytes of `[A-Za-z0-9_.:-]`. Tool-call ids (`tool_calls[].id`, `tool_call_id`) and metadata keys. |
| ENUMTEXT | 1 to 64 bytes; Unicode alphanumerics plus ASCII space and `_ . : / + -`; no control characters, quotes, backslashes, or angle brackets. Schema `enum` and `const` strings. |
| label scan | Every NAME, LINK, and ENUMTEXT string is passed to the core in detect-only mode. A finding of any action rejects the request with `422 unsupported_input` (the code ADR 0015 already uses for `Block`). The string is never rewritten, because rewriting would change an identifier, break call correlation, or collide two keys. |
| derived budgets | Strings decoded out of a string (tool arguments) and schema trees are counted against request-wide derived budgets in addition to the parse budgets: nodes (`max_nodes`), decoded bytes (`max_body_bytes`), and depth (arguments: 8 containers including the root; schemas: 8 nested schema objects). They are shared by every call, tool, and schema in the request, so many small ones cannot add up past the bound. Exceeding one is `413 limit_exceeded`. |
| numbers | Integers must fit `i64`; floats must be finite. A number is written back as the JSON writer's canonical form: identical value for every `i64` and for every float at `f64` precision, so `1e3` is written `1000.0`. Integers beyond `i64` and non-finite numbers are rejected, never coerced. Exact decimal-text preservation beyond `f64` is not promised (see Decisions). |
| duplicate keys | Rejected at every depth, including inside decoded tool arguments (`malformed_input`). |

### Top level

| Field | Class | Status | Contract |
| --- | --- | --- | --- |
| `model` | structural (core detect-only scan, never rewritten) | Implemented | Required string, 1 to 128 bytes, ASCII alphanumeric first, then alphanumerics or `. _ : / @ + -`. Transmitted verbatim. |
| `messages` | text | Implemented | Required array, 1 to `max_messages` entries (default 256), each a message object (below). |
| `stream`, `stream_options.include_usage` | structural | Implemented | Optional booleans; `stream_options` has no other key. |
| `temperature` | structural | Implemented | Number `0` to `2`. |
| `top_p` | structural | Implemented | Number `0` to `1`. |
| `presence_penalty`, `frequency_penalty` | structural | Implemented | Number `-2` to `2`. |
| `max_tokens`, `max_completion_tokens` | structural | Implemented | Integer `1` to `2147483647`. |
| `n` | structural | Implemented | Integer, exactly `1`. |
| `seed` | structural | Implemented | Integer in the `i64` range. |
| `stop` | text | Implemented | String, or array of 1 to 4 strings. |
| `user` | text | Implemented | String, at most 256 bytes. |
| `response_format` | structural | Implemented for `text`, `json_object`, and `json_schema` (#54) | See below. |
| `tools` | see below | Implemented (#54) | Array of 1 to 64 function tools. |
| `tool_choice` | structural + label | Implemented (#54) | See below. |
| `parallel_tool_calls` | structural | Implemented (#54) | Boolean. Rejected unless `tools` is present. |
| `metadata` | label + text | Planned #55 | See below. |
| `functions`, `function_call` | rejected | Rejected | Legacy shapes; the current `tools` shapes replace them. Never planned. |
| `logit_bias`, `prediction`, `modalities`, `audio`, `web_search_options`, `store`, `service_tier`, `reasoning_effort`, `logprobs`, `top_logprobs`, `verbosity`, `prompt_cache_key`, `safety_identifier`, anything unknown | rejected | Rejected | Arbitrary key-value data, binary or provider-stored content, or response shapes outside the relay bounds. Each needs its own contract and ADR. |

### Messages

| Field | Class | Status | Contract |
| --- | --- | --- | --- |
| `role` | structural | Implemented for `system`, `developer`, `user`, `assistant`, `tool` (#53) | Required, exact lowercase. `function` is rejected (legacy). |
| `content` | text | Implemented | String (any decoded text, including empty) or an array of 1 to 64 text parts. `null` is accepted only for an assistant message that carries `tool_calls` (Implemented, #53); every other `null`, number, boolean, or object is rejected. |
| `tool_calls` | see below | Implemented (#53) | Assistant only. Array of 1 to 32 calls. |
| `tool_call_id` | label (LINK) | Implemented (#53) | `role: tool` only, required there. |
| `name` | rejected | Rejected | Participant names are ambiguous identity text (and the legacy function-name carrier). |
| `refusal`, `audio`, `annotations`, `function_call`, anything unknown | rejected | Rejected | Provider-generated or non-text output fields. |

Role and content consistency (Implemented, #53; every violation is `unsupported_input`):

| Role | `content` | `tool_calls` | `tool_call_id` |
| --- | --- | --- | --- |
| `system`, `developer`, `user` | string or parts, never `null` | not allowed | not allowed |
| `assistant` | string, parts, or `null` | optional; non-empty when present; `content: null` requires it | not allowed |
| `tool` | string or parts, never `null` | not allowed | required |

### Content parts

A part is exactly `{"type":"text","text":"<string>"}`. `type` must equal `text` (exact), `text` is inspected text, and any other key is rejected. The array form is kept as an array so serialization preserves the type the caller chose. Tool-result `content` uses the same two forms and is ordinary text: a result that looks like JSON is never parsed.

### Assistant `tool_calls[]` (Implemented, #53)

| Field | Class | Contract |
| --- | --- | --- |
| `id` | label (LINK) | Required. Unique across the whole request (all assistant messages). |
| `type` | structural | Required, exactly `function`. `custom` and anything else is rejected. |
| `function.name` | label (NAME) | Required. It is not required to match a declared tool (history may predate the current `tools`). |
| `function.arguments` | derived: label keys + text leaves | Required string. Parsed with the strict duplicate-key-rejecting parser under the derived budgets; the top level must be a JSON object (`"{}"` is accepted, `""` is not). Object keys are NAME labels; string values are text leaves; numbers, booleans, `null`, nested objects, and arrays are preserved structure. Re-encoded compactly to a JSON string on output. This is the only string ever parsed as JSON. Failure classes: malformed JSON, trailing bytes, a duplicate key (also after `\u` decoding) and the empty string are `400 malformed_input`; a non-object root, a key outside NAME, or an integer literal outside `i64` is `422 unsupported_input`; depth over 8, or a node or decoded-byte total over the request-wide derived budgets, is `413 limit_exceeded`. At most 32 calls per message (`413 limit_exceeded` beyond). |
| anything else (`index`, `extra_content`, ...) | rejected | |

Correlation (Implemented, #53): a `role: tool` message must directly follow the assistant message that issued its `tool_call_id` (or another tool message answering the same assistant message), the id must be one of that message's calls, and each id is answered at most once. Unanswered calls are not enforced by the gateway (the provider rejects them). Tool results never name the tool: `name` stays rejected.

Example (synthetic):

```json
{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"서울\",\"days\":3}"}}]}
{"role":"tool","tool_call_id":"call_1","content":"sunny"}
```

### `tools[]` (Implemented, #54)

| Field | Class | Contract |
| --- | --- | --- |
| `type` | structural | Required, exactly `function`. |
| `function.name` | label (NAME) | Required. Unique across `tools`. |
| `function.description` | text | Optional, at most 4096 bytes after redaction. |
| `function.parameters` | schema subset (below) | Optional; when present the root is an object schema with `type: "object"`. |
| `function.strict` | structural | Optional boolean, preserved. |
| anything else | rejected | |

`tool_choice` (Implemented, #54), exactly one of:

| Form | Class | Contract |
| --- | --- | --- |
| `"none"`, `"auto"`, `"required"` | structural | Allowed only when `tools` is present. |
| `{"type":"function","function":{"name":N}}` | structural + label | `N` is a NAME that must equal a declared tool's name. `allowed_tools`, `custom`, and every other shape is rejected. |

`parallel_tool_calls` is a boolean and is allowed only when `tools` is present.

### Schema subset (Implemented, #54)

Used by `tools[].function.parameters` and `response_format.json_schema.schema`. A schema is a JSON object that uses only these keywords, written by the serializer in this canonical order (`properties` keeps the caller's entry order):

| Keyword | Class | Contract |
| --- | --- | --- |
| `type` | structural | One of `object`, `array`, `string`, `number`, `integer`, `boolean`, `null`, or an array of 1 to 4 distinct ones. |
| `description` | text | At most 4096 bytes after redaction. |
| `properties` | object of schemas | At most 64 entries; keys are NAME labels. |
| `items` | schema | One schema (no tuple form). |
| `required` | array of NAME labels | At most 64 distinct entries. Membership in `properties` is not enforced; the provider validates it. |
| `enum` | array of ENUMTEXT labels, numbers, booleans, or `null` | 1 to 64 entries. |
| `const` | one ENUMTEXT label, number, boolean, or `null` | |
| `additionalProperties` | structural | Boolean only. |
| `anyOf` | array of schemas | 1 to 8 entries. |
| `minimum`, `maximum` | structural | Number. |
| `minLength`, `maxLength`, `minItems`, `maxItems` | structural | Non-negative integer. |

Everything else is rejected, in particular: `$ref`, `$defs`, `definitions`, `$id`, `$schema`, `$anchor`, `$dynamicRef` (no reference of any kind is resolved or fetched, so there are no unresolved, external, or recursive constructs), `allOf`, `oneOf`, `not`, `if`/`then`/`else`, `patternProperties`, `propertyNames`, `prefixItems`, `unevaluated*`, `dependent*`, `pattern` and `format` (free-form strings that only a provider-specific engine interprets), `default`, `examples`, and `title` (arbitrary data or text that the smallest useful subset does not need), and any `x-` or unknown keyword. Schema nesting is also bounded by `max_depth` (default 16 containers including the request wrapper), so deep schemas need a raised limit. Schema depth is at most 8 nested schema objects, at most 256 schema objects per schema, and every node counts against the derived budgets.

Implementation notes (#54, `src/protocol/chat/schema.rs`): the root schema must carry `type: "object"` (`{}` and non-object roots are rejected); `type` keeps the caller's shape (string or array); an empty `properties` object and an empty `required` array are accepted, an empty `enum` or `anyOf` is not; `tools: []` is rejected. Count, depth, and size violations (more than 64 tools, 64 properties, 64 `required`, 64 `enum`, 8 `anyOf`, 4 `type` entries, depth 8, 256 schema objects per schema, a description over 4096 bytes before redaction) are `413 limit_exceeded`; every other violation is `422 unsupported_input`; duplicate keys are `400 malformed_input` from the strict parse. Schema nodes, label bytes, and description bytes are charged to one request-wide derived budget (`Derived`, shared by `tools` and `response_format`; #53 charges decoded arguments to the same type). After redaction a description is rechecked against 4096 bytes and every label against its charset and length; the aggregate size is bounded by the output bound. An integer literal beyond `u64` reaches the module as a float (no `arbitrary_precision`), so it is written as a float rather than rejected; this is the D10 limit.

Example (synthetic):

```json
{"type":"object","properties":{"city":{"type":"string","description":"City name."},"unit":{"type":"string","enum":["celsius","fahrenheit"]}},"required":["city"],"additionalProperties":false}
```

### `response_format`

| Form | Class | Status | Contract |
| --- | --- | --- | --- |
| `{"type":"text"}`, `{"type":"json_object"}` | structural | Implemented | Only `type`. |
| `{"type":"json_schema","json_schema":{...}}` | label + text + schema | Implemented (#54) | `name` required NAME label; `description` optional text (at most 4096 bytes after redaction); `strict` optional boolean; `schema` required, the schema subset above (root `type: "object"`). Any other key is rejected. |

### `metadata` (Planned #55)

A JSON object of at most 16 entries (the provider's limit). Keys are LINK labels (scanned, never rewritten); values are strings of at most 512 bytes after redaction and are inspected text. Numbers, booleans, `null`, arrays, and nested objects are rejected, as is an empty-string key. Example: `{"trace_id":"abc","team":"synthetic"}`. Entries keep the caller's order.

### Deliberately unsupported (all `unsupported_input`, HTTP 422)

| Form | Reason |
| --- | --- |
| `functions`, `function_call`, role `function` | Legacy shapes; replaced by `tools` and `tool_calls`. |
| Content parts of type `image_url`, `input_audio`, `file`, `refusal`, or any other type; `audio`, `modalities`, `prediction`, `web_search_options` | Binary, remote, or provider-stored content cannot be inspected through the text boundary. |
| `content: null` outside an assistant message with `tool_calls`, and non-string, non-array `content` | Opaque or unclassifiable content. |
| `name` on a message | Ambiguous identity text. |
| `logit_bias`, `store`, `service_tier`, `reasoning_effort`, `logprobs`, `top_logprobs` | Arbitrary key-value data, or features with response shapes outside the relay bounds. |
| `n` other than `1` | Larger values multiply response size beyond the relay bounds. |
| Any unknown key at any depth, any client claim that input is already scanned or redacted | Unknown nested fields get the same classification discipline as top-level fields; no claim is ever read. |

Until a Planned row lands, `metadata` is in this table's rejected set, with zero upstream bytes. Role `tool`, `tool_calls`, `tool_call_id`, and assistant `content: null` beside `tool_calls` are implemented (#53).

## Block versus redact

| Position | Mode | On a core finding | Why |
| --- | --- | --- | --- |
| Message, tool-result, and part text; `stop`; `user`; tool and schema `description`; metadata values; string leaves of decoded tool arguments | redact | Replaced in place by the core's placeholder (`<SECRET_n>`) | Free text: a placeholder keeps the request meaningful. Nothing in the gateway interprets the text. |
| `model`, tool and function names, response-schema name, `tool_choice` name, tool-call ids, `tool_call_id`, metadata keys, schema property keys, `required` entries, `enum`/`const` strings, object keys inside decoded tool arguments | label, detect-only | The request is rejected (`422 unsupported_input`); nothing is rewritten | A placeholder would change an identifier, break id linkage, make two keys collide (a duplicate-key document), or change a schema's meaning. The charset and length limits plus the label scan stop a secret from using a structural label as a channel; a short value that no detector recognizes can still ride (residual risk). |
| `type`, `role`, `strict`, booleans, numbers, fixed keywords | structural, not scanned | Not applicable | Fixed vocabulary or numeric range; no free text. |
| `default`, `examples`, `title`, `pattern`, `format`, `$ref`, legacy and unknown fields | rejected | Rejected before inspection | No safe rule that is small enough to justify. |

A redaction that would break a post-replacement bound (a metadata value or description over its limit, an `arguments` tree whose re-encoding changed shape, the output over its bound) is never truncated or patched: the request is rejected (`limit_exceeded` for sizes, `incomplete_inspection` for a shape change). The only action on a finding is replace (text) or reject (label); there is no strip-and-forward and no raw fallback. Warn handling stays ADR 0015's `content.on_warn`; it never applies to label scans, where any finding rejects.

## Traversal, mutation, and revalidation

Text-slot order is fixed and identical for reading and mutation ([ADR 0025](../decisions/0025-alpha2-field-contract.md), `src/protocol/chat/slots.rs`):

1. `messages` in order. Within a message: for `role: tool`, `tool_call_id`, then content (parts in order); otherwise content (parts in order), then each tool call in order: `id`, `function.name`, then the decoded argument leaves in document order (keys as labels, string values as text; the `leaf` ordinal counts keys and string values together, starting at 0 for each call).
2. `tools` in order: `function.name`, `function.description`, then schema leaves in canonical keyword order (property keys, `required`, enum, const as labels; `description` as text; the `leaf` ordinal counts them together). Then `tool_choice` function name.
3. `stop`.
4. `user`.
5. `metadata` entries in input order, key then value.
6. `response_format.json_schema`: `name`, `description`, then schema leaves.

Today classes 1 (messages, including tool history, #53), 2 (`tools`, `tool_choice`; #54), 3, 4, and 6 (`response_format.json_schema`; #54) produce slots; class 5 (`metadata`) is planned (#55). After the core call on every slot returns `Ok`, and before serialization, the request is revalidated: the visited slot count equals the classified count; every replaced string is rechecked against its slot bound; every decoded argument tree is re-encoded and its shape (container kinds, key sequence, non-string leaves) must equal the pre-inspection shape; label slots must be byte-identical to what was classified; derived budgets are recomputed on the replaced content. Implemented for tool history (#53): the call and result labels must still conform and match a digest taken at parse time, linkage and id uniqueness are re-checked, each decoded argument tree must have its parse-time node count with conforming, unique keys, and a replaced string over the per-string bound is `limit_exceeded`; the aggregate size is bounded by the serializer's output bound. Serialization is then bounded as before. Any failure rejects the request with no upstream bytes.

## Typed boundary representation

A request that satisfies the matrix becomes `protocol::chat::ChatRequest`, held inside `protocol::ValidatedRequest` together with its `MemoryReservation` and `ReceiptPermit`. The original body buffer is already released. `ChatRequest` has private fields; text is reachable only through `for_each_text` / `for_each_text_mut`, each slot carrying a `TextSlot` whose `mode()` (`Redact` or `DetectOnly`) is a property of its class. A later stage can replace text but cannot add keys, change roles, or alter structure. `chat_route::Admitted` pairs the `ValidatedRequest` with the operator-defined `RouteId` (`openai.chat_completions`), which is never derived from the request. `SanitizedRequest` is not constructed here; only `boundary` creates it, after inspection ([ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md)).

## Inspection and the outbound body (#19)

Every inspected text goes through the pinned core in the order above, with request-wide placeholder numbering (label slots are scanned detect-only and do not consume numbering). The outbound body is a fresh document serialized from the typed request (not the original bytes): the same keys, value types, and array order, with canonical key order inside objects (`model`, `messages`, `tools`, `tool_choice`, `parallel_tool_calls`, `stream`, `stream_options`, the numeric controls, `stop`, `user`, `metadata`, `response_format`; within a message `role` then `content`, then `tool_calls` or `tool_call_id`). Strings are escaped by the JSON writer and Unicode (including Korean) is emitted as UTF-8. Output is bounded by `min(max_body_bytes, reservation bytes)`; over the bound is `413 limit_exceeded`. Alpha 1 requests produce byte-identical output across the Alpha 2 module split (pinned by a unit test).

## Residual risks

- `model`, labels, and numeric fields are transmitted unchanged. Their constraints limit what can ride in them but do not make them secret-free channels.
- The matrix controls structure, not detection. Whether text contains a secret is core's decision (#19).
- Tool arguments are re-encoded, so an `arguments` string is not byte-equal to the caller's even when nothing was found.
- Unknown-field rejection can break SDK calls that send extra fields (for example replaying an assistant message with `refusal: null`, or a `name` on a message). That is deliberate.
- Pre-redacted input: a client-supplied `<SECRET_n>`-shaped string is ordinary text, not a finding, and no claim that input was scanned is ever read.
