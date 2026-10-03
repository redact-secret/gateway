# ADR 0025: Alpha 2 recursive Chat Completions field contract and module boundaries

Status: Accepted (design), by maintainer delegation to the #52 author. Implementation status: the contract is frozen; the tool-history forms (assistant `tool_calls`, `role: tool`, nullable assistant content, decoded `function.arguments`) are **implemented** (#53); tool definitions, `tool_choice`, `parallel_tool_calls` and `response_format.json_schema` are **implemented** (#54: the D5 schema subset, the D6 `tool_choice` rules, D7, the shared D11 derived budget, and D12 revalidation; no design decision changed, clarifications are in the contract); the remaining form (#55 metadata) is **planned** and rejected like unknown fields until they land. Implemented now (behavior-neutral): the `protocol::chat` module split, the slot-mode mechanism, the revalidation hook, and the contract test matrix. Date: 2026-10-03. Builds on [ADR 0005](0005-protocol-expansion-and-central-enforcement.md), [ADR 0007](0007-parsing-copying-and-plaintext-lifetime.md), [ADR 0014](0014-chat-completions-admission.md), [ADR 0015](0015-core-inspection-and-request-transformation.md). Contract: [chat-completions-request](../contracts/chat-completions-request.md), [field-classification](../contracts/field-classification.md). Issue: #52 (epic #9).

## Context

Alpha 1 accepts a text-only subset and rejects everything with free-form keys or nested structure. Alpha 2 adds tool history (#53), tool definitions and structured-output schemas (#54), metadata (#55), and request actions (#56). Four agents will build those in parallel, so the field contract, the answers to the hard questions, and the file boundaries must be fixed first. The product decisions were delegated to the #52 author with the rule: choose the smallest safe subset, reject by default, justify each decision.

## Decisions

Each row is a choice the maintainer can overturn; the contract file carries the exact rules.

| # | Decision | Why |
| --- | --- | --- |
| D1 | Every structural string (tool and function names, response-schema name, `tool_choice` name, tool-call ids, `tool_call_id`, metadata keys, schema property keys, `required` entries, `enum`/`const` strings, keys inside decoded tool arguments) is a **label**: constrained charset and length (NAME, LINK, ENUMTEXT) and scanned by the core in **detect-only** mode. Any finding rejects the request (`422 unsupported_input`); the string is never rewritten. | Rewriting would corrupt identifiers, break call linkage, or make two keys collide into a duplicate-key document. The charset and length stop a secret from riding in a label as free text, and the scan catches what the core recognizes. This is the `model` rule (ADR 0015 #9) generalized. |
| D2 | Free text is **redacted in place**: message, tool-result, and part text; `stop`; `user`; tool and schema `description`; metadata values; string leaves of decoded tool arguments. | Nothing in the gateway interprets these strings, so a placeholder keeps the request meaningful. |
| D3 | Object **keys inside tool arguments** are NAME labels (detect-only, constrained), not inspected-and-redacted text. | A redacted key can collide with another key and can no longer match the tool's schema. Keys replayed from history come from a schema whose property keys are already NAME labels. |
| D4 | Only `function.arguments` is ever parsed as JSON: strict, duplicate-key-rejecting, under request-wide derived budgets; the top level must be an object; `""` and non-object documents are rejected. It is re-encoded compactly on output. Tool-result content is plain text and is never parsed. | "Never blindly parse other strings" (field-classification). Re-encoding, not splicing into raw text, is the only way to keep valid JSON after replacement. |
| D5 | Schema subset: `type`, `description`, `properties`, `items`, `required`, `enum`, `const`, `additionalProperties` (boolean), `anyOf`, and six numeric/length bounds. Reject `$ref`/`$defs`/`definitions`/`$id`/`$schema`, `allOf`/`oneOf`/`not`/`if`, `patternProperties`/`propertyNames`, `pattern`, `format`, `default`, `examples`, `title`, and anything unknown. | This covers the common function-calling and structured-output schemas. Every rejected keyword is either a reference or recursion mechanism (never fetched or resolved), free-form text a provider interprets (`pattern`, `format`), or arbitrary data that would need its own label/text rule (`default`, `examples`, `title`). Each can be added later with its own rule. |
| D6 | `tool_choice`: `"none"`, `"auto"`, `"required"`, or `{"type":"function","function":{"name":N}}` with `N` a declared tool. Allowed only with `tools`. `parallel_tool_calls` boolean, only with `tools`. `allowed_tools`, `custom`, and everything else rejected. | Smallest set the SDKs emit for function calling. A named choice that does not match a tool is a linkage error caught locally. |
| D7 | `response_format.json_schema`: `name` (NAME label), `description` (text), `strict` (boolean), `schema` (the same subset, root object). | Same label/text split as tool definitions. |
| D8 | `metadata`: at most 16 entries, keys LINK labels, values strings up to 512 bytes (text). No numbers, nested objects, or arrays. | Matches the provider's published limits; a closed shape cannot become a generic object channel. |
| D9 | Tool history: assistant `content: null` only with non-empty `tool_calls`; `role: tool` needs `tool_call_id` and non-null content; ids unique across the request; a tool message answers an id issued by the assistant message it follows; `type` exactly `function`; message `name` stays rejected; legacy `functions`/`function_call`/role `function` rejected. | Role/content consistency and correlation are checked before inspection so a linkage error never reaches the core or the provider. Unanswered calls are left to the provider. |
| D10 | Numbers are preserved at `i64` / `f64` precision and written in canonical form; integers beyond `i64` and non-finite numbers are rejected. | The pinned `serde_json` build does not retain the source text of a number and enabling `arbitrary_precision` is a dependency feature change outside this task. **Flagged for the maintainer**: exact decimal text preservation needs that feature and its own measurement. |
| D11 | Derived budgets: strings decoded out of strings (arguments) and schema trees count against request-wide node, byte, and depth budgets in addition to the parse budgets; arguments depth 8, schema depth 8, at most 64 tools, 32 calls per message, 64 properties, 64 enum entries, 8 `anyOf` entries. All provisional (ADR 0008). | Many small arguments or schemas must not add up past the bound the parse already enforces. Findings and output reuse `content.max_findings` and the output bound. |
| D12 | Deterministic slot order across all classes and a **revalidation** step after mutation and before serialization: slot count, per-string and aggregate bounds, decoded-argument shape, label slots unchanged. Any failure rejects; nothing is truncated, patched, or sent raw. | A replacement can lengthen a string or, if done wrongly, change structure. Checking the typed tree again is cheap and local. |
| D13 | No client claim that input is "already scanned" or "already redacted" is read, anywhere. A `<SECRET_n>`-shaped client string is ordinary text. Responses `instructions` stays out of scope (#13). | ADR 0015 #10, unchanged. |

## Block versus redact

Redact (replace with the core's placeholder): free text (D2). Block (reject the whole request, `422 unsupported_input`): any finding in a label position (D1, D3). Reject before inspection: anything outside the matrix. If a redaction cannot be applied without breaking a bound or a shape (a metadata value over 512 bytes after replacement, an `arguments` tree whose shape changed), the request is rejected (`limit_exceeded` or `incomplete_inspection`), never degraded. Request-level action selection (#56) may change what a finding does in a text position; it never turns a label scan into a rewrite, and it never lets a request skip inspection.

## Mechanism (implemented now)

- `TextSlot` (in `src/protocol/chat/slots.rs`) has every planned slot variant pre-declared and a `mode()` that is exhaustive: `SlotMode::Redact` or `SlotMode::DetectOnly`. The mode follows the class, never the value or any client claim.
- `boundary::inspect_request` calls `reject_if_findings` for detect-only slots and `inspect_text` plus in-place replacement for redact slots, checks that every slot was visited and every redactable slot was scanned, and then calls `ChatRequest::revalidate()` before serializing. No Alpha 1 slot is detect-only, so behavior is unchanged.
- Each follow-up module owns three small functions that `slots.rs`, `serialize.rs`, and `mod.rs` already call: visit (read and mutable), write, and revalidate; plus its parse entry. Today they are rejecting or empty stubs.
- `tests/chat_field_matrix.rs` is the matrix: one synthetic body per row, the class, the outcome today, and the target outcome for planned rows. Planned rows are asserted rejected until their owner flips them.

## Module ownership map and conflict guidance

All paths are under `src/protocol/chat/` unless stated. One owner edits a file; the dispatch lines already exist, so a follow-up should not need to touch anyone else's file.

| File | Owner | Contains | Already wired from |
| --- | --- | --- | --- |
| `messages.rs` | #53 | `Role`, `Content`, `Message`, message parse, message visit/mutable visit/write | `mod.rs` classify (messages), `slots.rs`, `serialize.rs` |
| `tool_calls.rs` | #53 | `parse_message_field` (keys `tool_calls`, `tool_call_id`), `validate_history`, `revalidate`; the arguments tree and correlation | `messages.rs` parse, `mod.rs` classify (after the key loop), `slots.rs` `revalidate` |
| `tool_defs.rs` | #54 | `ToolDefs`, `parse_field` (keys `tools`, `tool_choice`, `parallel_tool_calls`), `finish`, visit, write, revalidate | `mod.rs` classify, `slots.rs`, `serialize.rs` |
| `schema.rs` | #54 | Typed schema subset shared by tools and `response_format` | `tool_defs.rs`, `response_format.rs` |
| `response_format.rs` | #54 | `ResponseFormat` incl. `json_schema`; visit, write, revalidate | `mod.rs` classify, `slots.rs`, `serialize.rs` |
| `metadata.rs` | #55 | `Metadata`, `parse`, visit, write, revalidate | `mod.rs` classify, `slots.rs`, `serialize.rs` |
| `slots.rs`, `serialize.rs`, `mod.rs` | baseline | `TextSlot` and `SlotMode`, traversal order, canonical key order, `ChatRequest`, `classify` | Shared, see below |
| `controls.rs`, `stop_user.rs` | none | Unchanged Alpha 1 fields | |
| `src/boundary/mod.rs`, `src/core_bridge.rs`, `src/config.rs` | #56 | Request-level action selection and policy, outside the protocol module | Detect-only handling is already in `inspect_request`; #56 changes how a finding in a redact slot is acted on |
| `tests/chat_field_matrix.rs` | each owner | Flip your own rows | |

Where the four tasks can still collide, and how to avoid it:

- `mod.rs`: `ChatRequest` struct fields and the `classify` match. The planned dispatch arms and fields (`tool_defs`, `metadata`) are pre-wired. #54 changes the `response_format` field type (an enum with a schema variant) and the matching `classify` arm; nobody else touches those two lines. If you must add a field, add it next to your module's existing field, not at the end.
- `slots.rs`: a new slot variant is already declared for each class. If a variant must change shape, change only your variant and its `mode()` arm; do not reorder variants. The order in the traversal doc comment is contractual.
- `serialize.rs`: one call per module, already in canonical order. Do not move calls.
- `tests/chat_field_matrix.rs`: flip only your owner's rows and its `target_rows_for_NN` test. Rows are grouped by owner.
- `tests/chat_admission.rs`: its unsupported-payload list contains `tools`, `tool_choice`, `tool message`, `assistant tool_calls`, `metadata`, `json_schema format`, and `null content` cases. Delete only the cases your task makes valid, in your own commit, and add the accepted forms to your own test file. Do not reformat the list.
- `docs/`: each owner edits only the rows of its own status in the contract (change "Planned #NN" to "Implemented") and its own ADR section. Do not renumber tables.
- `README.md` and `ARCHITECTURE.md`: change the planned/implemented line for your own feature only.
- A new ADR from any follow-up takes the next free number at merge time; do not reserve one in advance.

Merge order does not matter for correctness. Expected conflict hot spots, most to least likely: `tests/chat_admission.rs` (adjacent deleted cases), `slots.rs` (if two owners edit `mode()` arms), the contract file's status cells, and `mod.rs` `classify`.

## Invariants

1. No upstream request-body bytes until all required bounded parsing, complete inspection (including label scans), transformation, and revalidation succeed. No raw fallback.
2. A label is never rewritten. A finding in a label rejects the request.
3. Only `function.arguments` is parsed as JSON; every other string is text.
4. Every accepted field at every depth is classified (text, label, structural) or rejected. Unknown fields reject at any depth.
5. Slot order is deterministic and identical for read and mutation; slot mode is a property of the slot class.
6. Errors, `Debug`, logs, and telemetry carry no field name, value, or finding.
7. Detection stays core-owned; no new detector is introduced here.

## Failure behavior

| Outcome | Status | Code |
| --- | --- | --- |
| Unknown or planned-but-unimplemented field, wrong type, charset or enum violation, role/content or correlation violation, schema keyword outside the subset | 422 | `unsupported_input` |
| Duplicate key (including inside decoded arguments), malformed arguments JSON | 400 | `malformed_input` |
| Derived or parse budget, count limit, output over bound, post-replacement bound | 413 | `limit_exceeded` |
| Finding in a label position | 422 | `unsupported_input` |
| Revalidation shape failure, incomplete inspection | 500 | `incomplete_inspection` |

## Verification

`tests/chat_field_matrix.rs` (table: class, synthetic example, outcome now and target; planned rows asserted rejected; ignored per-owner target tests), `src/protocol/chat/mod.rs` unit tests (including a pinned Alpha 1 byte-identity test and slot-mode tests), the existing `tests/chat_admission.rs` and `tests/inspection_transform.rs` unchanged, and the zero-upstream cases in `tests/chat_admission.rs`. Each follow-up adds the positive round trip, the secret-placement, and the negative cases named in its issue, and must add a boundary-level test for the first detect-only producer (the mechanism exists but no Alpha 1 slot exercises it).

## Deferred measured choices

All numeric bounds in D11 and the 4096-byte description limit are provisional pending ADR 0008 measurement. Exact decimal preservation (D10) needs an `arbitrary_precision` decision. `anyOf` depth interplay with `max_depth` (default 16) bounds schema nesting to a handful of levels at the default; raising it is a configuration choice, not a contract change.

## Consequences and limits

A rejected `title`, `default`, `examples`, or `pattern` can break an SDK that generates schemas with them (for example schemas emitted by some validators); that is deliberate. Reusing a label position for a short, undetected secret is possible and documented as residual risk. The block rule makes a request fail where a redaction could have continued; that is the price of not corrupting identifiers.
