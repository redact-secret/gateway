# ADR 0029: Admit the `title` schema keyword as inspected free text

Status: Accepted, by maintainer delegation to the #57 author. Amends ADR 0025 D5 only for `title`. Every other keyword decision in D5 stands.

## Context

#54 recorded that the schemas real SDK helpers emit are rejected: the OpenAI Python SDK's strict conversion (`openai.lib._pydantic.to_strict_json_schema`, used by `pydantic_function_tool` and by `response_format=PydanticModel` through the parse helper) keeps Pydantic's `title` on the root and on every property, and nothing else outside the subset. `title` is human-readable text that a provider does not interpret structurally, so it has the same shape and risk as `description`: it can carry a secret, and redacting it in place does not change the schema's meaning.

## Decision

1. `title` is accepted in every schema object (root, property, `items`, `anyOf` member) as **inspected text**: redacted in place by the core, with the same bound as `description` (4096 bytes at parse time, rechecked after redaction, `413 limit_exceeded` beyond; charged to the request-wide derived budget). A non-string `title` is `422 unsupported_input`; a duplicate `title` is `400 malformed_input` from the strict parse.
2. Canonical order: `type`, `title`, `description`, `properties`, ... The traversal visits `title` before `description`, so leaf ordinals and placeholder numbering follow that order.
3. `title` is **only** a schema keyword. It remains rejected beside `function`, `json_schema`, messages, and every other object, as an unknown field.
4. Nothing else is relaxed. `default`, `examples`, `$ref`, `$defs`, `definitions`, `$id`, `$schema`, `pattern`, `format`, `allOf`, `oneOf`, `not`, `if`, and unknown keywords stay rejected. No evidence in #57 justifies a further change.

## Consequences

- The Python strict-converted shape (`to_strict_json_schema`, `pydantic_function_tool`, parse-helper `response_format`) is accepted as emitted for models that use only the subset's keywords. Models with nested models (`$defs`/`$ref` inlined by the strict conversion), `Optional` fields (`anyOf` with `null`), enums and `Literal` types pass; a field with `Field(pattern=...)`, a `format`-bearing type (`EmailStr`, `datetime`, `UUID`, `HttpUrl`), or a default value does not (see the coverage report).
- `title` text can reach the provider only after redaction; a short value no detector recognizes can still ride (the same residual risk as `description`).

## Verification

`tests/tool_schema.rs` (round trip, secret in `title` redacted, wrong-type/oversized/duplicate/misplaced `title` still rejected, SDK shape fixtures), `src/protocol/chat/tool_defs.rs`, `tests/chat_field_matrix.rs` rows, and the pinned SDK runs in `qualification/` (the Python run uses the SDK's real helper output).
