# Contract: initial field classification

Status: approved design contract; per-field tables are produced with the protocol work (#18, #19; complete contracts in Alpha 2). Owner: `protocol` module.

## Classes

Every allowed request field is exactly one of:

1. **Inspected application text**: decoded string content sent through core.
2. **Validated structural/control data**: has an explicit semantic contract (allowed enum, identifier pattern, numeric range, or similar). A label such as "model ID" does not allow arbitrary text. Intentionally transmitted structural data is documented.
3. **Rejected content**: not allowed.

Unknown fields, at any nesting depth, are rejected by default.

## Rules

- Classify recursively, including nested objects and arrays.
- Reject values that cannot be classified or that fail their contract.
- Parse once. Reject duplicate keys and invalid UTF-8. Inspect decoded strings so escapes cannot bypass checks.
- Preserve JSON types and keys. Deterministic traversal order and placeholder/session scope are fixed in the contract and tested (#5).
- Never replace text in raw serialized JSON.
- Contracts are needed for: prompt/instructions, messages, tool results, app-submitted tool arguments, metadata, and tool descriptions/schema text.
- App-submitted tool arguments may be JSON inside a string. They need a dedicated contract. Blind nested parsing of every string is not allowed. Gateway does not execute tools, and redaction grants no execution permission.

## Rejected forms (initial)

Files, images, audio, URL content, stored conversation/file references, encrypted or opaque content, arbitrary binary uploads, realtime/WebSocket, request compression, and any unclassified field. Transport chunking is receipt framing only, never incremental forwarding.

## Scope

Alpha 1: Chat Completions text subset. Beta 1: Responses API text subset. Anything else needs its own ADR. This contract does not enumerate fields yet; the field-by-field table is a deliverable of #18/#19 and Alpha 2.

## Status

Planned. Nothing implemented.
