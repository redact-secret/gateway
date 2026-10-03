//! Bounded JSON-schema subset shared by `tools[].function.parameters` and
//! `response_format.json_schema.schema`. Owner: #54. Contract:
//! `docs/contracts/chat-completions-request.md` (schema subset), ADR 0025.
//!
//! Empty until #54: it adds the typed schema tree, the keyword table, the parse step with
//! the request-wide derived node/depth/byte counters, the label/text slot visitors and the
//! serializer here, and calls them from `tool_defs.rs` and `response_format.rs`. Nothing
//! outside those three files changes.
