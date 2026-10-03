# Alpha 2 qualification report: field coverage and policy behavior with the pinned SDK clients

Status: evidence for issue #57 (epic #9, Alpha 2). Everything here is synthetic: invented prompts, revoked-looking tokens, a scripted fake provider on loopback, no provider key, no network beyond 127.0.0.1. The gateway under test is the **non-release qualification build** ([ADR 0020](../decisions/0020-sdk-qualification-test-build.md)): the shipped source plus a seam that points the unchanged forwarding code at the fake provider. It is not the shipped binary. The candidate is still unpublished. Related: [Alpha 1 report](alpha1-qualification-report.md), [field contract](../contracts/chat-completions-request.md), [request policy](../contracts/request-policy.md), [ADR 0025](../decisions/0025-alpha2-field-contract.md), [ADR 0026](../decisions/0026-request-policy-over-pinned-core.md), [ADR 0028](../decisions/0028-aggregate-load-qualification.md), [ADR 0029](../decisions/0029-schema-title-keyword.md).

## What is claimed, and what is not

Claimed, each with the evidence below:

- The pinned OpenAI Node.js/TypeScript SDK (`openai` 7.27.0, with `zod` 4.6.5 for its `zod` helpers) and Python SDK (`openai` 3.24.0, with its bundled `pydantic`) drive the implemented Alpha 2 subsets through the gateway: tool-history round trips (assistant `tool_calls` with JSON arguments, tool results, parallel calls), `tools`, `tool_choice`, `parallel_tool_calls`, `response_format` `json_schema`, `metadata`, and the policy outcomes (redact, block, warn rejected, warn forwarded, finding bound).
- For every forwarded row the provider's **recorded request** is parsed and compared with the expected sanitized body (every retained structure, not just a `200`), the planted secrets are absent, the placeholder count and numbering are as the contract orders them, and exactly one upstream request was delivered.
- For every rejected row (92 of 123 per SDK) the provider recorded **zero connections and zero requests**, the status and safe code are the documented ones, and the SDK-visible error carries no secret marker.
- Placeholder numbering and sanitized bodies are per request when 24 requests run at once, with rejected neighbours in flight.
- The captured stdout/stderr of all eight gateway instances contain no synthetic marker (the gateway emits no request logs).
- The cost and output size of the Alpha 2 shapes next to the Alpha 1 cases, as provisional observations.

Not claimed: that any secret is detected beyond what the pinned core detects, a real-provider result (there is none), other SDK/runtime versions, the exact shipped binary accepting Alpha 2 requests end to end, a quiet-host performance result, or any production-readiness, security-audit, or throughput statement.

## Evidence layers

| Layer | What it proves | Where | Alpha 2 coverage |
| --- | --- | --- | --- |
| Contract tests (in process, no SDK) | The field matrix, the schema subset, label scans, the policy action table, slot coverage and revalidation, zero-upstream rejections | `tests/chat_field_matrix.rs`, `tests/tool_schema.rs`, `tests/policy_actions.rs`, `tests/chat_admission.rs`, `src/protocol/chat/*` unit tests, `src/transport/tests/slot_coverage_tests.rs` | Every matrix row and text class; the `title` change (ADR 0029) |
| Fake-upstream test build, pinned SDK runs (this report) | Real SDK clients and their real schema helpers against a real gateway process, with the provider's own record as the oracle | `qualification/` (`alpha2-cases.json`, `sdk/node/test/alpha2-*.ts`, `sdk/python/tests/*alpha2*.py`), the `Qualification` workflow | 123 rows per SDK plus SDK-specific tests (below) |
| Exact shipped-binary smoke | The candidate bytes start, validate config, and refuse the documented probes locally | `scripts/smoke-binary.sh`, `scripts/probe-endpoints.sh`, the `Candidate artifacts` workflow | Four Alpha 2 shapes refused before inspection and forwarding (`$ref` schema with a synthetic token, non-string metadata, malformed and duplicate-key tool arguments). **Accepted Alpha 2 requests are not exercised on the exact binary**: it can only reach the real HTTPS provider |
| Real provider | Behavior of an actual provider with the sanitized body | none | **None.** No statement here depends on a provider accepting the sanitized body; the repository never calls a provider |

## SDK matrix: what ran

| Item | Node.js/TypeScript | Python |
| --- | --- | --- |
| SDK | `openai` 7.27.0 (lockfile integrity-pinned), `zod` 4.6.5 added exact-pinned | `openai` 3.24.0 (sha256-locked requirements; `pydantic` is its locked dependency) |
| Runtimes | Node.js 24 (full) and 22.16.0 (undici 6.21.2) legs of the Qualification workflow | Python 3.13 |
| Case table | `qualification/alpha2-cases.json`: 108 cases, 123 case-by-gateway rows (31 forwarded, 92 rejected; 14 raw-body rows) | the same file |
| SDK-specific tests | 7 (zod helpers 3, round trips 3, concurrency 1) | 8 (pydantic helpers 4, round trips 3, concurrency 1) |
| Evidence files | `alpha2-cases-node.json`, `alpha2-sdk-node.json` (uploaded by the workflow) | `alpha2-cases-python.json`, `alpha2-sdk-python.json` |

Gateway instances (the existing config mechanism, `qualification/configs/`): `standard` (`full` profile, `on_warn` reject), `policy-forward` (`on_warn` forward), `policy-common` (`common` profile), `tight` (`max_findings` 2, small body bounds), `concurrent` (wider capacity). The fake provider replies with a plain completion, an assistant `tool_calls` message (one or two calls, shaped like a real reply including `refusal: null` and `annotations: []`), or a JSON object for the `parse` helper.

## Coverage by text class

Each row of the [field matrix](../contracts/chat-completions-request.md) has at least one case in the shared table. IDs are `alpha2-cases.json` `id` values.

| Class | Position | Outcome | Case ids (group) |
| --- | --- | --- | --- |
| text | message content (string, parts), all roles | redacted in place, numbered in traversal order | `roles-string-content`, `parts-content`, `unicode-adjacent-secret`, `quotes-backslash-newline` (redact-text, unicode-escaping) |
| text | `stop` (string, array), `user` | redacted | `stop-string`, `stop-array`, `user-field` |
| text | tool result content (string, parts) | redacted | `tool-args-leaves-and-result`, `tool-result-parts` |
| text | decoded tool-argument string leaves (nested, arrays, Korean, escapes) | redacted; tree re-encoded compactly | `tool-args-leaves-and-result`, `tool-args-empty-object-and-escapes`, `parallel-calls-numbering`, `two-rounds-multi-turn` |
| text | tool and function `description`, schema `description`, schema `title`, response-format `description` | redacted | `tool-defs-descriptions-and-titles`, `tool-def-no-parameters`, `response-format-json-schema` |
| text | metadata values (EN and KR) | redacted | `metadata-values`, `metadata-sixteen-entries`, `metadata-empty-and-plain` |
| all text classes together | traversal order across messages, tools, `stop`, `user`, metadata, response format | 11 distinct secrets numbered 1 to 11 in the documented order | `every-text-class-in-traversal-order` |
| label | tool name, function name in history, schema property key, `enum` string, `const` string, `required` entry, metadata key, tool-call id, argument key, response-schema name, `model`, a secret hidden by `\u` escapes in a key | whole request refused `422 unsupported_input`, nothing rewritten, on `standard` and `policy-forward` | 12 `label-*` ids (block-labels) |
| structural | controls, `tool_choice` forms, `parallel_tool_calls`, schema bounds | preserved exactly | `controls-preserved`, `tool-choice-and-parallel-forms`, `schema-all-keywords-no-secret`, `no-secret-passthrough-all-shapes` |
| unsupported structure | unknown fields, legacy shapes, wrong roles and linkage, unsupported schema keywords, metadata shapes | `422 unsupported_input`, zero upstream | 34 ids (unsupported) |
| malformed arguments | invalid JSON, empty string, trailing bytes, duplicate key (also after `\u` decoding), non-object root, key outside the charset, integer beyond `i64`, depth 9 | `400 malformed_input`, `422`, or `413` as the contract says | 10 ids (malformed-arguments) |
| limits | 65 tools, 4097-byte description and title, 33 calls in a message | `413 limit_exceeded` | 4 ids (limits) |
| raw body | duplicate keys (top level, escaped, in a tool, in a schema, in metadata), lone surrogate, trailing bytes, BOM, invalid UTF-8, `NaN`; plus `\u`-escaped secrets that must still be redacted | `400 malformed_input`, or forwarded redacted | 10 raw-body ids and 3 `raw-*` unicode ids |
| policy | `Block` (private key) in content, tool result and metadata beside a redactable secret; `Warn` rejected by default and in a tool argument; `Warn` forwarded unchanged when configured while a `Redact` secret beside it is still replaced; `common` profile; finding bound | block `422` in both modes; warn per `on_warn`; bound `413` | 15 ids (policy) |

Observations the cases pinned, which are not tidy:

- `common` forwards the synthetic GitHub-style token **unchanged** (`common-profile-leaves-github-token`): that profile has no such detector. `full` redacts it.
- Invented Korean national-ID and phone-number shapes are forwarded unchanged (`korean-national-id-and-phone-not-detected`): the pinned core has no Korean selector.
- Numbers in tool arguments are canonicalized: `1e3` is forwarded as `1000.0` (`numbers-canonicalized`).
- A secret that comes first in a message `content` is numbered before the secrets in that message's tool-call arguments (`parallel-calls-numbering`).

## SDK helper output (item 2)

The SDKs' real helpers were run, not hand-built fixtures.

| Helper (pinned) | Result against the gateway | What decided it |
| --- | --- | --- |
| Python `openai.pydantic_function_tool(FlatModel)`; `chat.completions.parse(response_format=FlatModel)`; `parse(tools=[pydantic_function_tool(...)])` | **Forwarded as emitted.** The sanitized tool equals the helper's output with only the secret in a `Field(description=...)` replaced; `strict`, `required`, `additionalProperties: false`, `minLength`/`maxLength`/`minimum`/`maximum`/`maxItems` retained; the parse helper returned a typed instance from the relayed reply | The `title` relaxation (ADR 0029): the strict conversion writes `title` on every schema object |
| Same with a nested model, an `Enum` class, a field default, `Field(pattern=...)`, `datetime`, `UUID` | **Rejected** `422`, zero upstream, as tool and as `response_format` | `$defs`/`$ref` stay in the helper output (the strict conversion does not inline them); `default`; `pattern`; `format` |
| Node `zodResponseFormat` and `zodFunction` as emitted | **Rejected** `422` | `$schema` (draft-07) is always written |
| Same with `$schema` deleted | **Forwarded**, description redacted, `strict`/`required`/`additionalProperties` retained | The application removed the one rejected key |
| zod `.email()`, `.regex()`, `.default()` even without `$schema` | **Rejected** | `format`, `pattern`, `default` |

Decision (ADR 0029): only `title` was relaxed, as inspected free text with the `description` bound. `default`, `examples`, `$ref`/`$defs`/`$id`/`$schema`, `pattern`, `format` and the rest stay rejected. Incompatibilities an SDK user must plan for: no nested Pydantic models or enums as classes, no defaults, no regex or format-bearing types, delete `$schema` from zod output, and do not replay a provider reply object as history (next row).

| Replay | Result |
| --- | --- |
| Assistant turn built from `role`, `content: null`, `tool_calls[].{id, type, function.{name, arguments}}` | Forwarded, ids and argument trees retained, results redacted |
| The provider's message object verbatim (Node typed object, Python message object, `model_dump(exclude_none=True)`) | `422 unsupported_input` (`refusal`, `annotations`, parsed fields are unknown fields), zero further upstream requests |

## Request-state isolation

24 concurrent requests per SDK (1 to 4 secrets each, tool history, metadata, distinct per-request markers) plus two rejected neighbours (a label secret and a private key) on the `concurrent` instance. Result: 24 upstream requests exactly, none for the neighbours; each body equals its expected sanitized body with numbering restarting at 1; the placeholder count per body matches; no other request's text or secret appears; no planted secret reached the provider.

## Incremental cost and output growth

Reuses the #58 harness: `qualification/perf/run.mjs --incremental` calls the same `aggStages` sequential per-request stage measurement, gateway configuration and body builders as `--aggregate`, with four same-size Alpha 2 shapes added (`tool_history_4KiB`, `tool_defs_4KiB`, `tool_history_16KiB_findings`, `tool_defs_16KiB_findings`) so the comparison with `small_4KiB` and `many_findings_16KiB` is like for like. No second load harness. Raw data: [alpha2-incremental-cost-2026-10-03.json](alpha2-incremental-cost-2026-10-03.json); the load, memory and starvation results are in [alpha2-aggregate-load-2026-10-03.json](alpha2-aggregate-load-2026-10-03.json) and [alpha2-memory-accounting-2026-10-03.json](alpha2-memory-accounting-2026-10-03.json) (see the Alpha 1 report's Alpha 2 load subsection, ADR 0028).

**Conditions.** Release profile of the qualification build, Apple M4 (10 cores, macOS 25.5.0), Node.js 22.16.0, five sequential passes, fake provider. **Not a quiet host**: the one-minute load average was 5.4 to 10.2 at the start of each pass (the quiet condition is at most 2.5), so every figure is provisional (ADR 0008). Stage figures are exact sequential deltas; the table is the median over the five passes of each pass's p50. Inspection includes serialization.

| Shape | Input bytes | Output bytes | Findings | Parse p50 us | Inspection p50 us | Serialization p50 us | Parse us/KiB | Inspection us/KiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `small_4KiB` (Alpha 1) | 4,162 | 4,162 | 0 | 2 | 46 | 1 | 0.5 | 11.3 |
| `tool_history_4KiB` | 4,039 | 4,039 | 0 | 15 | 67 | 4 | 3.8 | 17.0 |
| `tool_defs_4KiB` | 3,379 | 3,379 | 0 | 12 | 61 | 3 | 3.6 | 18.5 |
| `metadata` (16 entries) | 7,724 | 7,484 | 8 | 7 | 134 | 2 | 0.9 | 17.8 |
| `many_findings_16KiB` (Alpha 1) | 16,469 | 6,589 | 349 | 3 | 330 | 2 | 0.2 | 20.5 |
| `tool_history_16KiB_findings` | 17,129 | 16,192 | 32 | 42 | 357 | 13 | 2.5 | 21.3 |
| `tool_defs_16KiB_findings` | 13,909 | 12,508 | 48 | 32 | 298 | 8 | 2.4 | 21.9 |
| `large_500KiB` (Alpha 1) | 512,066 | 512,066 | 0 | 52 | 3,271 | 139 | 0.1 | 6.5 |
| `tool_history` (large) | 661,143 | 650,283 | 384 | 912 | 11,533 | 355 | 1.4 | 17.9 |
| `tool_defs` (large) | 825,731 | 818,455 | 256 | 932 | 8,989 | 373 | 1.1 | 11.2 |
| `node_dense` (about 12,800 enum labels) | 84,987 | 84,987 | 0 | 563 | 9,294 | 177 | 6.8 | 112.0 |

What the figures support:

- **Parse** is 3 to 8 times the plain-text Alpha 1 cost per KiB for structured bodies at 4 KiB (15 us against 2 us), because Alpha 2 builds a typed tree and parses `arguments` strings with a second strict parse. It stays on the request thread and is 1.1 to 1.4 us/KiB for the large Alpha 2 bodies (0.9 ms for each of the 661 KB and 826 KB bodies).
- **Inspection** (the core) per KiB is 1.5 to 1.6 times the Alpha 1 cost at 4 KiB without findings (17.0 and 18.5 against 11.3 us/KiB) and about the same at 16 KiB with findings (21.3 and 21.9 against 20.5) and follows the number of inspected slots rather than bytes or findings: 32 findings in 17 KiB of tool history cost about as much as 349 findings in 16 KiB of text; 12,800 label slots in 85 KB cost 9.3 ms, more than 660 KB of tool history.
- **Serialization** is a small part (2 to 6% of inspection) in every shape.
- **Output growth.** With no findings the sanitized body is byte-identical to the input for compact-JSON shapes (4,039 to 4,039 for tool history). With findings it shrinks, because `<SECRET_n>` is shorter than a 40-byte token (16,469 to 6,589 for 349 findings; 661 KB to 650 KB for 384). It can grow when a finding is shorter than its placeholder (a short secret, or `1e3` written `1000.0`); growth is never truncated: the body is held to `min(max_body_bytes, reservation)` and over the bound is `413 limit_exceeded`. Growth for short findings was not measured.
- Every forwarded body reached the provider free of the synthetic token prefix (provider counters: 0 in every pass), every request returned `200`.

**Provisional budgets and limitations** (observations for the maintainers, not defaults, not claims):

- A planning figure for this host class, not quiet: parse at most about 4 us/KiB and inspection at most about 22 us/KiB for tool history, tool definitions and metadata; label-dense schemas about 110 us/KiB, bounded by `max_nodes`. A 1 MiB tool-history body therefore occupies one inspection worker for about 20 ms and a worker-saturating label-dense body for about 110 ms; ADR 0028's admission and queue behavior applies unchanged.
- Sequential single-client figures only; combined-load behavior is the #58 dataset. No default limit changed.
- macOS arm64, loaded host, Node 22.16.0 harness, release profile of the qualification build (not the shipped artifact), fake provider on loopback. The CI `measure` job also runs `--incremental` and uploads a Linux dataset as a workflow artifact; it is not committed.
- No budget becomes a default without a quiet-host record (ADR 0008).

## Outstanding gaps

- Real provider: none. Whether a provider accepts the sanitized shapes (for example `tool_choice` plus `parallel_tool_calls` plus a strict schema) is unverified.
- Detection: `common` has no GitHub-token detector; the pinned core has no Korean national-ID or phone detector; PII selectors are context-gated; a short value that no detector recognizes can ride in a label position (residual risk in the contract).
- Schema subset: see "SDK helper output". Accepting more (defaults, nested models, `format`, `pattern`) needs its own decision and evidence.
- Numbers are canonicalized; integers beyond `u64` are written as floats (D10).
- The exact shipped binary was not driven with an accepted Alpha 2 request.
- Streaming responses, `tool_calls` in streamed deltas, and response redaction remain relay-only and unredacted by design.

## Acceptance reconciliation (#57)

| Criterion | Status | Evidence |
| --- | --- | --- |
| Node.js/TypeScript and Python demonstrate supported tool round-trips and policy outcomes against the qualified subset | Met on the fake-upstream build | Round-trip tests (one and parallel calls), 15 policy rows, both SDKs; CI run below |
| Tests verify expected sanitized body and retained provider structures, not merely `200` | Met | Recorded-request deep comparison per forwarded row |
| All negative cases prove zero upstream body and request-state isolation | Met | 92 rejected rows per SDK with zero provider connections and requests; the 24-way concurrency tests |
| Performance regression results identify provisional budgets and limitations; no unsupported claims | Met, provisional | Incremental cost section, dataset `alpha2-incremental-cost-2026-10-03.json` |
| README and contracts report supported features and outstanding gaps | Met | README "Alpha 2 coverage, SDK compatibility and known gaps"; contract "SDK helper output"; this report |

CI evidence: see the pull request (Qualification workflow, both Node legs and Python; CI; Candidate artifacts smoke).

## Reproduce

```sh
sh qualification/build.sh                       # the non-release test build (separate crate)
(cd qualification/sdk/node && npm ci --ignore-scripts)
(cd qualification/sdk/python && python -m venv .venv && .venv/bin/pip install --require-hashes --no-deps -r requirements.txt)
sh qualification/run-suites.sh --suites node,python
sh qualification/build.sh --release && QUAL_COMMAND='node qualification/perf/run.mjs --incremental --runs 5' \
  sh qualification/run-suites.sh --binary qualification/target/release/redact-secret-gateway-qualification --suites none
```
