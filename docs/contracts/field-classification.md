# Contract: initial field classification

Status: approved design contract. The Chat Completions tables are complete for Alpha 1 (implemented, #18, #19) and frozen for Alpha 2 (#52, [ADR 0025](../decisions/0025-alpha2-field-contract.md)); the Alpha 2 additions are planned until #53 and #55 land; tool definitions and structured-output schemas (#54) are implemented. Owner: `protocol` module.

## Classes

Every allowed request field is exactly one of:

1. **Inspected application text**: decoded string content sent through core.
2. **Validated structural/control data**: has an explicit semantic contract (allowed enum, identifier pattern, numeric range, or similar). A label such as "model ID" does not allow arbitrary text. Intentionally transmitted structural data is documented. Structural strings that an identifier, key, or enum value must keep intact (**labels**: tool and function names, tool-call ids, schema property keys, enum and const strings, metadata keys, keys inside decoded tool arguments) have a constrained charset and length and are scanned by the core in detect-only mode: a finding rejects the request, and the string is never rewritten.
3. **Rejected content**: not allowed.

Unknown fields, at any nesting depth, are rejected by default.

## Rules

- Classify recursively, including nested objects and arrays.
- Reject values that cannot be classified or that fail their contract.
- Parse once. Reject duplicate keys and invalid UTF-8. Inspect decoded strings so escapes cannot bypass checks.
- Preserve JSON types and keys. Deterministic traversal order and placeholder/session scope are fixed in the contract and tested (#5).
- Never replace text in raw serialized JSON.
- Contracts are needed for: prompt/instructions, messages, tool results, app-submitted tool arguments, metadata, and tool descriptions/schema text. Chat Completions has them in [chat-completions-request](chat-completions-request.md) (Alpha 2 rows are planned); Responses `instructions` and its distinct field model stay with #13.
- Free text is redacted in place; a label is never rewritten (block, not redact). Where redaction would corrupt an identifier, a key, an enum value, or tool linkage, the field is a label. A new field must be classified as text, label, structural, or rejected in the same change that admits it.
- App-submitted tool arguments may be JSON inside a string. The dedicated contract (implemented, #53) parses only the designated `function.arguments` string, with bounded duplicate-key-rejecting parsing, inspects decoded string leaves, treats keys as labels, and re-encodes. Blind nested parsing of every string is not allowed. Gateway does not execute tools, and redaction grants no execution permission.

## Rejected forms (initial)

Files, images, audio, URL content, stored conversation/file references, encrypted or opaque content, arbitrary binary uploads, realtime/WebSocket, request compression, and any unclassified field. Transport chunking is receipt framing only, never incremental forwarding.

## Scope

Alpha 1: Chat Completions text subset. Alpha 2: the recursive Chat Completions subset with tool history, function tool definitions, structured-output schemas, and metadata (contract frozen in #52; tool history implemented by #53, the rest by #54 and #55). Beta 1: Responses API text subset. Anything else needs its own ADR. The field-by-field table is [chat-completions-request](chat-completions-request.md).

## Keys, structural strings, and `Warn`/`Block` (#19)

- Object keys are not inspected as text: in the implemented subset every key is a fixed schema name validated by the field matrix, and every other free-form-keyed object (`logit_bias`, tool schemas) is rejected. The one implemented exception is `metadata` (#55), whose keys are labels: 1 to 64 bytes of `[A-Za-z0-9_.:-]` plus a detect-only core scan, never a rewrite. The Alpha 2 contract admits free-form keys only as labels (metadata keys, schema property keys, argument keys): a constrained charset and length plus a detect-only core scan, never a rewrite (implemented for tool-call ids, names and argument keys in #53; metadata keys in #55; implemented for schema property keys, #54).
- The validated structural string `model` is checked by the core in detect-only mode: any finding rejects the request, and it is never rewritten.
- A `Block` finding in inspected text rejects the request; a `Warn` finding rejects unless `content.on_warn` is `"forward"` ([ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md)).
- Pre-redacted input: a client-supplied `<SECRET_n>`-shaped string is ordinary text, not a finding. No client claim that input was scanned has any effect.

## Status

Implemented for the Chat Completions text subset (#18): the matrix, the recursive unknown-field rejection, and the decoded-string and duplicate-key rules. Tool results and app-submitted tool arguments (assistant `tool_calls`, `role: tool`) are implemented (#53). Tool descriptions and schema text (#54: `tools`, `tool_choice`, `parallel_tool_calls`, `response_format.json_schema`) are implemented. Metadata is implemented (#55): label keys, inspected string values, finite bounds, and a regression suite with zero upstream bytes on every rejection. The slot-mode mechanism (redact versus detect-only) and the post-mutation revalidation hook are implemented and behavior-neutral for Alpha 1.
