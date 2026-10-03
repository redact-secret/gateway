# Contract: `POST /v1/responses` stateless text request subset (frozen, planned)

Status: **frozen design contract; the #84 subset (`instructions`, string and text-message `input`, `store:false`, `stream`, `stream_options`, `temperature`, `top_p`, `max_output_tokens`) is implemented in `protocol::responses` and tested at the protocol and boundary layers, the rest is planned** (#82, epic #13, [ADR 0031](../decisions/0031-responses-stateless-text-contract.md)). Until the owning issues land, `POST /v1/responses` is an unrouted path and is answered like any other unknown path (`404 unsupported_input`, zero upstream bytes). No part of this is a served feature today: the route is unrouted until #86, and until #85 lands every field it owns (`tools`, `tool_choice`, `parallel_tool_calls`, `text`, `metadata`, function-call items) is rejected like any unknown field. Implementation owners: #83 (typed dispatch), #84 (`instructions`, string and text-message `input`), #85 (function-call items, function outputs, `tools`, structured text formats, `metadata`), #86 (fixed destination and caller-auth boundary), #87 (JSON and SSE relay), #88 (qualification and the Beta 1 endpoint matrix). Config freeze #62 depends on this contract and on the #61 caller-token authority, not on #86 or #88.

This is **not generic OpenAI compatibility**. It is the smallest request subset whose every text-bearing byte can pass through the pinned core. Everything not listed is rejected, at every depth, and rejected means rejected, never stripped. Parent rules: [field-classification](field-classification.md), [request-state](request-state.md), [request-policy](request-policy.md). Limits: [resource-limits](resource-limits.md). Errors: [errors-and-telemetry](errors-and-telemetry.md). The Chat Completions counterpart is [chat-completions-request](chat-completions-request.md); the Responses shapes differ and are not derived from it field by field.

Basis of the matrix: the request types of the pinned SDKs, read directly from the packages in `qualification/sdk` (Node.js `openai` 7.27.0, `resources/responses/responses.d.ts`; Python `openai` 3.24.0, `types/responses/response_create_params.py`, `easy_input_message_param.py`), plus the published API reference for `POST /v1/responses`. This is a type-surface review. Wire behavior of the pinned SDKs against each accepted form is measured by #88, not claimed here.

## Request admission

Identical to the Chat Completions admission table in [chat-completions-request](chat-completions-request.md#request-admission-before-any-body-byte-is-read) with the exact path `/v1/responses` (no trailing slash, no case folding, no percent-decoding, no query string), `POST` only, `application/json` only, no compression, one length framing, and the same size, deadline, and receipt rules. Parsing rules (one JSON document, decoded strings and keys, duplicate keys rejected after decoding, budgets enforced while the tree is built, integers must be integer literals) are also the same. The route is the operator-defined `RouteId`, never derived from the request; its fixed destination is #86. Caller authentication is not defined here: when #63 lands, the local caller token boundary of [local-caller-auth](local-caller-auth.md) (#61) is applied to this route by #86 before any body byte is collected, and the token header is stripped before forwarding exactly as for Chat; this contract adds no auth field and no config key.

## Field classes

Same four classes as the Chat contract: **text** (decoded string the core may redact in place), **label** (constrained structural string the core only scans, detect-only; any finding rejects, never rewritten), **structural** (explicit contract, transmitted as is), **rejected**.

| Name | Rule |
| --- | --- |
| NAME | 1 to 64 bytes of `[A-Za-z0-9_-]`. Function tool names, `text.format` schema name, schema property keys, `required` entries, keys inside decoded function-call arguments, function-call `name`. |
| LINK | 1 to 64 bytes of `[A-Za-z0-9_.:-]`. `call_id` on function calls and outputs, metadata keys. |
| ENUMTEXT | As in the Chat contract: 1 to 64 bytes of Unicode alphanumerics plus ASCII space and `_ . : / + -`. Schema `enum` and `const` strings. |
| label scan | Every NAME, LINK, ENUMTEXT string and `model` is passed to the core in detect-only mode. Any finding rejects the request with `422 unsupported_input`; the string is never rewritten (a rewrite would change an identifier, break call correlation, or collide two keys). |
| derived budgets | Strings decoded out of `arguments` and schema trees are counted against the same request-wide derived budgets as Chat (`max_nodes`, decoded bytes, depth 8 for arguments, 8 nested schema objects). Shared by every call, tool, and schema in the request. |
| numbers | `i64` integers and finite floats; canonical rewrite as in the Chat contract. |

### Null, false, empty, and absent

A field is either absent or has exactly the one shape below. **`null` is rejected on every field except the two places the pinned SDK types make a key required and nullable (`tools[].parameters`, `tools[].strict`)**, which are listed explicitly. A rejected field is rejected when present with any value, including `null`, `false`, `""`, `[]`, and `{}`. There are no shortcut forms: an empty `input` array, an empty `tools` array, an empty `metadata` object and an empty `text` object are each handled by an explicit rule below, never by "treat as absent".

### Top level

| Field | Class | Contract |
| --- | --- | --- |
| `model` | structural (detect-only scan) | Required. Same rule as Chat `model`. |
| `input` | text | Required. A string (any decoded text, including empty), or an array of 1 to `max_messages` items (default 256) of the item forms below. |
| `instructions` | text | Optional string (any decoded text, including empty). `null` rejected. |
| `store` | structural | **Required, exactly the boolean `false`.** Absent, `null`, and `true` are rejected ([ADR 0031](../decisions/0031-responses-stateless-text-contract.md)). Serialized upstream as `"store":false`. |
| `stream` | structural | Optional boolean. `null` rejected. |
| `stream_options` | structural | Optional, only with `stream: true`; an object whose only key is `include_obfuscation` (boolean). |
| `temperature` | structural | Number `0` to `2`. |
| `top_p` | structural | Number `0` to `1`. |
| `max_output_tokens` | structural | Integer `1` to `2147483647`. |
| `tools` | see below | Array of 1 to 64 function tools. `[]` rejected. |
| `tool_choice` | structural + label | See below. Only with `tools`. |
| `parallel_tool_calls` | structural | Boolean. Only with `tools`. |
| `text` | structural + label + text | Object with at most the keys `format` and `verbosity`; see below. `{}` is accepted and serialized as `{}`. |
| `metadata` | label + text | At most 16 entries; keys LINK labels, values strings of at most 512 bytes (inspected text). Rules identical to the Chat `metadata` contract (no numbers, booleans, `null`, arrays, or nested objects; `{}` accepted). |
| `previous_response_id`, `conversation` | rejected | Provider-stored conversation state is never inspected by the gateway. |
| `prompt` | rejected | Stored prompt template reference. |
| `background` | rejected | Asynchronous processing with provider-side state. `false` and `null` are rejected too (there is no accepted spelling). |
| `include` | rejected | Selects extra output data, including encrypted reasoning content. `[]` is rejected too. |
| `reasoning` | rejected | Reasoning controls and opaque reasoning state. `{}` and `null` are rejected too. |
| `truncation`, `context_management`, `moderation`, `access_programs`, `service_tier`, `max_tool_calls`, `top_logprobs` | rejected | Features outside the relay bounds or without a text-inspection contract. Each needs its own contract. |
| `user`, `safety_identifier`, `prompt_cache_key`, `prompt_cache_options`, `prompt_cache_retention` | rejected | Arbitrary identity or cache-key text with no review. |
| anything unknown, including fields added by newer SDKs | rejected | Default-rejected. This is the documented compatibility tradeoff: a newer client that sends a new field is refused until that field is reviewed. |

## `input` items

An `input` array holds items in order. An item is an object whose shape is selected by its `type`, or by `role` and `content` when `type` is absent (the SDK's `EasyInputMessage`). Any key not listed for the form is rejected, at every depth. Provider output objects are **not** automatically valid input: every provider-generated field (`id`, `status`, `annotations`, `logprobs`, `caller`, `namespace`, `async`) is rejected, so an application builds history from the listed keys only.

### Text message item

`{"role": R, "content": C}` with optional `"type":"message"` and, for assistant only, optional `"phase"`.

| Field | Class | Contract |
| --- | --- | --- |
| `type` | structural | Optional; when present exactly `message`. Preserved as written (present stays present, absent stays absent). |
| `role` | structural | Required, exactly `user`, `assistant`, `system`, or `developer`. |
| `content` | text | A string (any decoded text, including empty), or for `user`, `system`, and `developer` an array of 1 to 64 parts `{"type":"input_text","text":T}`. `null`, numbers, objects, and an assistant parts array are rejected. |
| `phase` | structural | `assistant` only; exactly `commentary` or `final_answer`; `null` rejected. |
| `status`, `id` | rejected | Provider-populated. |

Part: exactly `{"type":"input_text","text":"<string>"}`. `type` is exact, `text` is inspected text, every other key (`prompt_cache_breakpoint`, ...) is rejected. `input_image`, `input_file`, `input_audio`, and every other part type are rejected.

Assistant replay: only the string form of the easy message is supported (`{"role":"assistant","content":"..."}`). The provider output message (`type: message` with `id`, `status`, and `content` of `output_text` parts carrying `annotations`) is rejected as unknown fields and parts, because its annotations carry file ids and URLs and its `id` is a provider handle that is meaningless under `store:false`. An application replaying `response.output` must rebuild each assistant turn from its text.

### `function_call` item (app-submitted, #85)

| Field | Class | Contract |
| --- | --- | --- |
| `type` | structural | Required, exactly `function_call`. |
| `call_id` | label (LINK) | Required. Unique across all `function_call` items of the request. |
| `name` | label (NAME) | Required. Not required to match a declared tool (history may predate the current `tools`). |
| `arguments` | derived: label keys + text leaves | Required string. Parsed by the same strict parser and rules as Chat `function.arguments` (object root, duplicate keys rejected after decoding, keys NAME labels, string leaves text, depth 8, request-wide derived budgets, compact re-encoding, `""` rejected). |
| `id`, `status`, `caller`, `namespace`, `async` | rejected | Provider-populated or programmatic-calling state. |

### `function_call_output` item (#85)

| Field | Class | Contract |
| --- | --- | --- |
| `type` | structural | Required, exactly `function_call_output`. |
| `call_id` | label (LINK) | Required. Must equal the `call_id` of an earlier `function_call` item in the same `input` array; each call is answered at most once. |
| `output` | text | Required. A string (plain text, never parsed as JSON, even if it looks like JSON), or an array of 1 to 64 parts `{"type":"input_text","text":T}`. Image and file parts are rejected. |
| `id`, `status`, `caller`, `name`, `namespace` | rejected | Provider-populated. |

### Correlation and ordering

An output must come after its call; it need not be adjacent (parallel calls are normally followed by their outputs together). Unanswered calls are not enforced by the gateway (the provider rejects them). A message item may appear anywhere. These checks run before inspection, so a linkage error never reaches the core or the provider.

### Rejected item forms (all `422 unsupported_input`)

`item_reference` (stored item), `reasoning` (including encrypted or opaque content), provider `message` output objects, `function_call` or `function_call_output` carrying a rejected key, `custom_tool_call` and `custom_tool_call_output`, `file_search_call`, `web_search_call`, `computer_call` and `computer_call_output`, `code_interpreter_call`, `image_generation_call`, `local_shell_call` and output, `shell_call` and output, `apply_patch_call` and output, `mcp_list_tools`, `mcp_approval_request`, `mcp_approval_response`, `mcp_call`, `tool_search_call` and output, `additional_tools`, `compaction` and `compaction_trigger`, `program` and `program_output`, `configuration_update`, and every unknown `type`. An item with neither a known `type` nor `role` and `content` is rejected.

## `tools[]` (function tools only, #85)

| Field | Class | Contract |
| --- | --- | --- |
| `type` | structural | Required, exactly `function`. |
| `name` | label (NAME) | Required. Unique across `tools`. |
| `description` | text | Optional, at most 4096 bytes after redaction. `null` rejected. |
| `parameters` | schema subset | Required key. The Chat schema subset with root `type: "object"`, or `null` (the pinned SDK types require the key and allow `null`). Serialized as given. |
| `strict` | structural | Required key. Boolean or `null` (same reason). Serialized as given. |
| `allowed_callers`, `async`, `defer_loading`, `output_schema`, `namespace` | rejected | Programmatic calling, deferred loading, and output schemas have no review. |

Hosted and remote tools (`file_search`, `web_search`, `web_search_preview`, `computer`, `computer_use_preview`, `mcp`, `code_interpreter`, `image_generation`, `local_shell`, `shell`, `apply_patch`, `custom`, `namespace`, `tool_search`, `programmatic_tool_calling`) are rejected. The schema subset is exactly the Chat contract's ([schema subset](chat-completions-request.md#schema-subset-implemented-54)): `$ref`, `$defs`, `default`, `pattern`, `format`, and every other keyword outside it are rejected, and SDK helper output is accepted or rejected exactly as in the [SDK helper table](chat-completions-request.md#sdk-helper-output-against-this-subset-57) (the helpers share one schema generator).

`tool_choice` is exactly one of `"none"`, `"auto"`, `"required"`, or `{"type":"function","name":N}` (the Responses shape is flat; there is no `function` wrapper) where `N` is a NAME equal to a declared tool. `allowed_tools`, `custom`, `mcp`, hosted-tool types, `shell`, `apply_patch`, `programmatic_tool_calling`, and a Chat-shaped nested form are rejected.

## `text` (structured output, #85)

| Field | Class | Contract |
| --- | --- | --- |
| `format` | structural | `{"type":"text"}`, `{"type":"json_object"}`, or the flat form below. Any other `type` or key is rejected. |
| `format` (`json_schema`) | label + text + schema | `{"type":"json_schema","name":NAME,"schema":S,"strict":bool,"description":T}`: `name` and `schema` required, `description` optional text (at most 4096 bytes after redaction), `strict` optional boolean (`null` rejected), `schema` the schema subset with root `type: "object"`. There is no nested `json_schema` object. |
| `verbosity` | structural | Optional, exactly `low`, `medium`, or `high`. `null` rejected. |

## Block versus redact

| Position | Mode | On a core finding |
| --- | --- | --- |
| `instructions`; string `input`; message `content` and part `text`; `function_call_output.output` and part `text`; function-call `arguments` string leaves; tool, schema, and format `description`; schema `title`; metadata values | redact | Replaced in place by the core's placeholder (`<SECRET_n>`) |
| `model`; `call_id`; `name`; tool names; format `name`; `tool_choice` name; metadata keys; schema property keys; `required` entries; `enum` and `const` strings; keys inside decoded arguments | label, detect-only | Request rejected (`422 unsupported_input`); nothing rewritten |
| `type`, `role`, `phase`, `store`, `verbosity`, booleans, numbers, fixed keywords | structural, not scanned | Not applicable |
| every rejected field and form | rejected | Rejected before inspection |

**Secret-placement coverage requirement**: every text slot and every label slot above has at least one test that places a synthetic secret in that exact position (redact positions: the secret is replaced and the outbound document is valid; label positions: the request is rejected and zero upstream bytes are sent) and one test that a structural field cannot carry free text (a secret in `type`, `role`, `phase`, `verbosity`, or a rejected key is rejected by the matrix, not by the core). No accepted field is an arbitrary text bypass: a structural field is either an exact enum or boolean or number, or it is a label with a charset and length. The owning issues (#84, #85) add these tests in the same change that admits each slot; #88 verifies the matrix is complete.

Redaction that would break a bound or a shape is rejected (`413 limit_exceeded` for sizes, `500 incomplete_inspection` for a shape change); there is no truncation, strip-and-forward, or raw fallback. `Warn` handling is the existing `content.on_warn`; it never applies to label scans.

## Traversal and request-local placeholder scope

Text-slot order is fixed, identical for reading and mutation, and different from Chat's:

1. `instructions`.
2. `input`: the string; or each item in order. Message: `content` (parts in order). `function_call`: `call_id`, `name`, then decoded argument leaves in document order (keys as labels, string values as text, one `leaf` ordinal counting both from 0). `function_call_output`: `call_id`, then `output` (parts in order).
3. `tools` in order: `name`, `description`, then schema leaves in canonical keyword order.
4. `tool_choice` name.
5. `text.format` (`json_schema`): `name`, `description`, then schema leaves.
6. `metadata` entries in input order, key then value.

`model` is scanned detect-only before the slots, as in Chat. Placeholder numbering (`<SECRET_n>`) is request-wide, in this slot order, and starts at 1 for every request; label slots do not consume numbers; nothing persists across requests or items. A client-supplied `<SECRET_n>`-shaped string is ordinary text, never trusted as proof of scanning, never restored.

After the core call on every slot returns `Ok`, and before serialization, the request is revalidated exactly as in Chat (slot count, per-string bounds, argument tree shape, label slots byte-identical, derived budgets recomputed). Any failure rejects with zero upstream bytes.

## Outbound document and limits

The outbound body is a fresh document serialized from the typed request, never the original bytes, with the same keys, value types, and array order. Canonical key order: `model`, `instructions`, `input`, `store` (always written, `false`), `tools`, `tool_choice`, `parallel_tool_calls`, `text`, `metadata`, `stream`, `stream_options`, `temperature`, `top_p`, `max_output_tokens`; within an item `type` (only if present), `role`, `content`, `phase` for messages, and `type`, `call_id`, `name`, `arguments` or `type`, `call_id`, `output` for calls and outputs. Unicode is emitted as UTF-8.

All limits are finite and reuse the existing configured values, so no new operator key is introduced by this contract: body bytes, node count, depth (the item and part wrappers count as containers, so nested schemas need a depth the operator has configured), decoded bytes, the item-count bound `max_messages` (items), 64 parts per content array, 64 tools, 64 properties, 64 enum entries, 8 `anyOf` entries, the request-wide derived budgets, and the output bound `min(max_body_bytes, reservation bytes)`. Count, depth, and size violations are `413 limit_exceeded`; every other violation is `422 unsupported_input`; duplicate keys and malformed arguments are `400 malformed_input`; revalidation or incomplete inspection is `500 incomplete_inspection`.

## Storage, retention, and replay limitations

- `store` is mandatory and must be `false`. The gateway never sets, defaults, or infers it. This is a request-shape rule, not a retention guarantee: `store:false` does not guarantee zero provider retention (providers can retain request data for abuse monitoring, safety, or legal reasons independent of this flag; zero data retention is an arrangement between the application's organization and the provider). The gateway cannot observe or enforce provider retention.
- Reference and state paths are closed: `previous_response_id`, `conversation`, `prompt`, `item_reference`, `include`, `background`, and every provider-generated item field are rejected, so no request can make the provider inject stored content the gateway never saw. Multi-turn use is done by the application resending its own text history in `input`.
- Opaque reasoning is not replayable: `reasoning` items, `reasoning` controls, and `include: ["reasoning.encrypted_content"]` are rejected. Applications that depend on reasoning-item replay across tool turns cannot use this subset.
- The `id` of a provider response is of no use to the caller here (it cannot be sent back), and the provider's response, JSON or SSE, is relayed without redaction. Terminal-event and status semantics for Responses streams (`response.completed`, `response.incomplete`, `response.failed`, `error`; not Chat `finish_reason`) are #87.

## Examples (synthetic)

Accepted:

```json accepted
{"model":"synthetic-model","store":false,"instructions":"Be brief.","input":"서울의 날씨는?"}
```

```json accepted
{"model":"synthetic-model","store":false,"input":[{"role":"user","content":[{"type":"input_text","text":"Weather in Seoul?"}]},{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"서울\"}"},{"type":"function_call_output","call_id":"call_1","output":"sunny"}],"tools":[{"type":"function","name":"get_weather","description":"City weather.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false},"strict":true}],"tool_choice":{"type":"function","name":"get_weather"},"text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false},"strict":true}},"metadata":{"trace_id":"abc"}}
```

Rejected (each is `422 unsupported_input` unless noted, with zero upstream bytes):

```json rejected store-omitted
{"model":"synthetic-model","input":"hi"}
```

```json rejected store-true
{"model":"synthetic-model","store":true,"input":"hi"}
```

```json rejected store-null
{"model":"synthetic-model","store":null,"input":"hi"}
```

```json rejected previous-response
{"model":"synthetic-model","store":false,"previous_response_id":"resp_synthetic","input":"hi"}
```

```json rejected conversation-null
{"model":"synthetic-model","store":false,"conversation":null,"input":"hi"}
```

```json rejected background-false
{"model":"synthetic-model","store":false,"background":false,"input":"hi"}
```

```json rejected include-empty
{"model":"synthetic-model","store":false,"include":[],"input":"hi"}
```

```json rejected reasoning-item
{"model":"synthetic-model","store":false,"input":[{"type":"reasoning","id":"rs_synthetic","summary":[]}]}
```

```json rejected item-reference
{"model":"synthetic-model","store":false,"input":[{"type":"item_reference","id":"msg_synthetic"}]}
```

```json rejected image-part
{"model":"synthetic-model","store":false,"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/a.png"}]}]}
```

```json rejected hosted-tool
{"model":"synthetic-model","store":false,"input":"hi","tools":[{"type":"web_search"}]}
```

```json rejected provider-output-message
{"model":"synthetic-model","store":false,"input":[{"type":"message","id":"msg_synthetic","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hi","annotations":[]}]}]}
```

```json rejected output-without-call
{"model":"synthetic-model","store":false,"input":[{"type":"function_call_output","call_id":"call_9","output":"x"}]}
```

```json rejected chat-shaped-format
{"model":"synthetic-model","store":false,"input":"hi","text":{"format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"}}}}}
```

```json rejected null-instructions
{"model":"synthetic-model","store":false,"instructions":null,"input":"hi"}
```

```json rejected unknown-field
{"model":"synthetic-model","store":false,"input":"hi","future_field":1}
```

`tests/responses_contract.rs` parses every fenced example above, checks the `store` rule on all of them, and pins the planned status of the route until #86.

## Residual risks

- Labels, `model`, and numeric fields are transmitted unchanged; constraints limit what can ride in them but do not make them secret-free channels.
- The matrix controls structure, not detection. Whether text contains a secret is core's decision.
- Function-call `arguments` are re-encoded, so the string is not byte-equal to the caller's even when nothing was found.
- Rejecting provider-generated fields means an SDK loop that replays `response.output` unchanged is refused; an application must rebuild items from the listed keys.
- Unknown-field rejection breaks clients that send newly added fields until each is reviewed. That is deliberate.
- `store:false` and this contract do not make the provider forget the request.
