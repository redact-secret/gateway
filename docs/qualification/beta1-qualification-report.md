# Beta 1 qualification report: frozen configuration, caller/provider credential separation, and the Responses subset

Status: evidence for issue #65 (epic #12, Beta 1) and issue #88 (epic #13, Beta 1). Everything here is synthetic: invented prompts, revoked-looking tokens, a scripted fake provider on loopback, no provider key, no network beyond 127.0.0.1. The candidate is unpublished and this report is not a release statement. Related: [Alpha 1 report](alpha1-qualification-report.md), [Alpha 2 report](alpha2-qualification-report.md), [Responses contract](../contracts/responses-request.md), [local caller auth](../contracts/local-caller-auth.md), [upgrade and rollback](../contracts/config-upgrade-rollback.md), [config schema](../contracts/config-schema.md), [ADR 0020](../decisions/0020-sdk-qualification-test-build.md), [ADR 0030](../decisions/0030-local-caller-auth-listener-health.md), [ADR 0031](../decisions/0031-responses-stateless-text-contract.md), [ADR 0033](../decisions/0033-config-upgrade-rollback.md), [ADR 0034](../decisions/0034-responses-qualification-and-beta1-endpoint-matrix.md).

This file is the Beta 1 endpoint matrix. #65 qualified the Chat Completions endpoint with local authentication; #88 added `POST /v1/responses` as a second column in every section. Claims are restricted to the reviewed Responses subset (a stateless text subset, `store:false` required) and to the layers named in section 3: the SDK results below come from a **non-release test build** with a fake provider, not from the candidate bytes and not from any real provider.

## 1. Pins

| Item | Value |
| --- | --- |
| Gateway source | #65: base `e0538dcdac5252ea4c659b9b133191d9bc16b222` plus the #65 change. #88: base `4f2c5012e99cd0a09363d7f6a009ef84ac4646f3` (`main` after #87) plus the #88 change; the merge commit is recorded on the pull request and the issue |
| Core | `redact-secret =0.1.0-beta.12` (direct dependency, `Cargo.lock` checksum `2cc951e8b991e9ec27343a872627f53a25b252cc8148cb4ec7160192ed9d856d`), unchanged by #65 and #88 |
| Node.js SDK | `openai` 7.27.0 (lockfile integrity-pinned), `zod` 4.6.5 |
| Python SDK | `openai` 3.24.0 (sha256-locked requirements; bundled `pydantic` 2.13.5) |
| Runtimes in CI | Node.js 24 (suites `node,python,examples`) and 22.16.0 (suite `node`); Python 3.13. Local reproduction for this report: Node.js 22.16.0 (undici 6.21.2), Python 3.14.7 (the suites are runtime independent; CI is the record). Observations that depend on the runtime (what a stream helper raises on a cut) are recorded per runtime, not asserted |
| Qualification build | separate generated crate from the shipped `src/` plus `qualification/seam.patch` (ADR 0020), `qualification/build.sh`; its `[dependencies]` and lock are the shipped ones byte for byte. The fake provider is `qualification/fake-provider/server.mjs` with the Responses scenarios in `responses.mjs` |
| Case table | `qualification/responses-cases.json`: 278 cases, 358 case-by-gateway rows per SDK (42 forwarded, 316 rejected; 20 raw-body rows), shared by both harnesses; Chat keeps `qualification/alpha2-cases.json` (123 rows) |
| Gateway instances | the existing ten (`standard`, `tight`, `noupstream`, `deadprovider`, `overload`, `policyforward`, `policycommon`, `concurrent`, `authfile`, `authenv`); #88 added no instance and no configuration key |
| Exact-binary smoke | `scripts/smoke-binary.sh` (which runs `scripts/probe-endpoints.sh`, now with the Responses rejection probes, and ends with `scripts/smoke-local-auth.sh`), run in the `Candidate artifacts` workflow against the built candidate bytes |
| Candidate manifest | `scripts/candidate-manifest.sh`: `capabilities.proxy.endpoints` lists both endpoints and their subsets; `distributable: false` |
| Config schema | `schema_version` 1, [`docs/schema/gateway-config.v1.schema.json`](../schema/gateway-config.v1.schema.json) |
| Measurement host (section 4d) | Apple M4, 10 cores, 24 GiB, macOS (Darwin 25.5.0), Node.js 22.16.0, rustc 1.98.1, release profile of the qualification build; one-minute load average 9.0 to 13.7 during the five passes (**not quiet**; the quiet condition is at most 2.5) |

## 2. Supported deployment and endpoint subsets (exactly what is qualified)

### Endpoint matrix

| Dimension | `POST /v1/chat/completions` | `POST /v1/responses` |
| --- | --- | --- |
| Reviewed subset | Alpha 2 Chat text subset: messages, `stop`, `user`, tool history, `tools`, `tool_choice`, `parallel_tool_calls`, `response_format` `json_schema`, `metadata` ([contract](../contracts/chat-completions-request.md)) | Stateless text subset ([contract](../contracts/responses-request.md)): `store:false` required, `instructions`, string or text-message `input`, app-submitted `function_call` / `function_call_output` items, function `tools`, flat `tool_choice`, `parallel_tool_calls`, `text.format` (`text`, `json_object`, flat `json_schema`) and `verbosity`, `metadata`, `stream`, `stream_options.include_obfuscation`, `temperature`, `top_p`, `max_output_tokens` |
| Rejected, never stripped | unknown fields, images, audio, files, legacy shapes | `previous_response_id`, `conversation`, `prompt`, `item_reference`, `include`, `background`, `reasoning` and reasoning items, hosted/custom/remote tools, image/file/audio parts, provider output items and their ids/status/annotations, `null` outside `tools[].parameters` and `tools[].strict`, `truncation`, `service_tier`, `user`, cache keys, and every unknown field at any depth |
| Local authentication | qualified with the SDKs and raw HTTP (#65) | qualified with the SDKs and raw HTTP (#88); one token and one provider key serve both |
| Provider credential | caller-supplied `Authorization`, request-local | the same, per request, independent across the two endpoints under concurrent load |
| Relay | JSON and SSE, unredacted; completion signal `finish_reason` / `[DONE]` | JSON and SSE, unredacted; completion signal is the provider's own `response.completed`, `response.failed` or `response.incomplete`, never synthesized |
| Provider | fixed `openai` route to a fake upstream (test build) | fixed `openai.responses` route to a fake upstream (test build) |
| Real provider | none | none |

### Deployment

| Dimension | Qualified | Not qualified |
| --- | --- | --- |
| Listener | loopback with `local_auth` absent, `disabled` or `token`; non-loopback with `allow_non_loopback: true` **and** `token` (the container and same-Pod shape, started and probed on the exact candidate, not a supported shared or remote deployment) | any shared, remote, multi-tenant or internet-facing gateway; inbound TLS |
| Token delivery | `file` (mode without "other" bits) and `env`, both through the real loader, resolved once at startup | inline value, CLI argument, hot reload, rotation without a restart, overlap of two tokens |
| Content policy | `full` (`on_warn` reject) and `common` (`on_warn` forward) instances, both authenticated; the full table of policy outcomes (redact, block, warn rejected, warn forwarded, finding bound) on both endpoints | every other profile and PII selection combination |
| Probes | `GET /healthz`, `GET /readyz` without the token | any admin, metrics or debug route (none exists) |

## 3. Evidence layers

The layers are kept separate on purpose: a result in one layer says nothing about the next.

| Layer | What it proves | Where | Not proven |
| --- | --- | --- | --- |
| Contract and unit tests (in process, no SDK) | Token syntax and comparison, ordering, startup failure kinds, schema fixtures, migration pairs; the Responses parser, every text and label slot, revalidation, limits and the route (`tests/responses_text.rs`, `tests/responses_tools.rs`, `tests/responses_route.rs`, `tests/responses_contract.rs`, `src/transport/tests/responses_route_tests.rs`, `src/transport/tests/responses_stream_tests.rs`); docs agree with fixtures | `src/`, `tests/` | anything about a real SDK |
| Fake-upstream test build, pinned SDK runs | Real Node and Python SDK requests, raw malformed requests and headers, and bounded load against the qualification gateway instances, with the **fake provider's own record** (deep body comparison, request and connection counts, headers, `local_token_seen`) as the oracle | `qualification/sdk/node/test/*.test.ts`, `qualification/sdk/python/tests/test_*.py`, `qualification/run-suites.sh` | exact candidate bytes accepting a request; a real provider |
| README examples, dual credential | `examples/node` and `examples/python` (Chat and Responses) run exactly as documented against the authenticated instance | `qualification/run-examples.sh` | a real provider |
| Exact candidate binary smoke | The shipped bytes validate the examples and shipped configs, start, serve readiness, enforce authentication ordering, change the token only at a restart, roll back, refuse unsupported combinations before readiness, and refuse the Chat and Responses probes locally. **No accepted request reaches a provider** (the binary can only reach the real HTTPS provider, and the scripts never send an acceptable body or a provider `Authorization` with one) | `scripts/smoke-local-auth.sh`, `scripts/probe-endpoints.sh` via `scripts/smoke-binary.sh` | accepted-request behavior of the shipped bytes |
| Real provider | none | none | everything about provider acceptance of the sanitized bodies |
| Measurement (provisional) | Incremental parse, inspection and serialization cost of the Responses shapes next to the Chat ones, one non-quiet host | `qualification/perf/run.mjs --incremental` | any capacity or production budget |

## 4. Results

### 4a. Local authentication on Chat Completions (#65)

Each row is executed for **both** SDKs and **both** authenticated instances (`authfile`: token file, `full` profile; `authenv`: token environment variable, `common` profile with `on_warn` forward). 41 Node tests and 28 Python tests, plus the example runs.

| Acceptance item | Evidence | Result |
| --- | --- | --- |
| Caller and provider secrets cannot substitute for each other | SDK with both credentials: `200`. Provider key sent as the local token: `401 local_auth_invalid`. Local token sent only as the provider key: `401 local_auth_required` (`Authorization` is never read for local authentication). Valid local token with no provider `Authorization`: `401 missing_credential`. A different well-formed token: `401 local_auth_invalid` | Met |
| Fake upstream never receives local auth | The fake provider flags any request in which either synthetic local token (valid or decoy) appears in a header value, the target or the body (`local_token_seen`), and lists the header names it received. Accepted calls (JSON, streamed, lowercase header name) show `local_token_seen: false`, no `x-gateway-local-token` header, and the provider key's `Authorization` hash unchanged | Met |
| Rejection sends no upstream body | Every rejection (no token, wrong token, provider key as token, 9 malformed shapes, `Connection`-nominated removal, missing provider credential) leaves the provider with **zero connections and zero requests**, including when the body carries a planted secret and a private-key marker (authentication precedes policy) | Met |
| Malformed headers | duplicate (both valid), `Bearer` prefix, comma list, embedded space, empty, 31 bytes, 129 bytes, quoted, characters outside the alphabet: `401 local_auth_invalid`, byte-identical fixed body `{"error":{"code":"local_auth_invalid"}}` | Met |
| Bounded unauthenticated load | 240 raw requests per instance and SDK in waves of 40, half without a token and half with a wrong one, each announcing a 4 MiB body that is never sent: all `401` with the two fixed bodies, provider untouched, and an authenticated request afterwards succeeds. This is a smoke of the pre-body ordering, **not a throughput or denial-of-service result** | Met, bounded |
| Logs and errors expose no secret values | SDK-visible errors (message, body, headers) scanned for both tokens, the provider key, planted secrets and prompt markers; the captured stdout and stderr of all ten gateway instances scanned for the same markers (`log_forbidden` now includes both local tokens); the evidence JSON the tests write is scanned too | Met (the gateway emits no request logs) |
| Health and readiness | `200` with no token and with a wrong one, no upstream contact | Met |
| Policy and profile variations | Authentication outcomes are identical under `full`/reject and `common`/forward; the authenticated `common` instance forwards a GitHub-style token unchanged (the Alpha 2 observation), the `full` one redacts it | Met on two profiles |
| Examples validate against the schema and the compiled loader | `scripts/smoke-local-auth.sh` runs `validate-config` on every `examples/config.*.json` and on generated Alpha, Beta, environment-token and rollback configs with the exact binary; `tests/shipped_examples.rs` and `tests/config_schema.rs` validate the shipped and schema fixtures in CI | Met |
| Dual-credential examples | `examples/node` and `examples/python`: both credentials succeed; provider key only, provider key as token and token as provider key are refused locally with the safe code, print neither credential, and leave the provider untouched | Met |
| Migration, rollback, readiness reproducible | See section 5 | Met on the exact binary |

### 4b. Responses subset through the pinned SDKs (#88)

Every row runs for **both** SDKs. Forwarded rows are compared with the fake provider's **recorded** request: the parsed body must equal the expected sanitized body (every retained structure, key and value type, with each synthetic secret replaced by the placeholder number the traversal order of the contract assigns), the placeholder count must match, `store:false` must be written, the destination must be `/v1/responses` with the caller's own provider `Authorization` and no local token, and exactly one request on exactly one connection must have been delivered. Rejected rows must show the documented status and safe code, no secret-bearing SDK error text, and **zero provider connections and zero requests**.

| Group (`responses-cases.json`) | Rows per SDK | Forwarded | Rejected | Provider requests on rejected rows |
| --- | --- | --- | --- | --- |
| `redact-text`, `redact-structured`, `redact-tool-defs`, `redact-tools-history`, `redact-everything`, `unicode-escaping`, `residual-risk` | 37 | 37 | 0 | n/a |
| `block-labels` (15 label slots and a `\u`-escaped key, on `standard` and `policyforward`) | 32 | 0 | 32 | 0 |
| `structural-not-text` (a secret in `role`, `type`, `phase`, `verbosity`, tool and format `type`, `tool_choice`, `stream_options` keys, unknown keys at three depths, a rejected field's value) | 38 | 0 | 38 | 0 |
| `stored-and-opaque-state`, `unsupported-items`, `unsupported-tools`, `unsupported-controls` (every state, reference, opaque, hosted, remote, Chat-shaped, null and range rule of the contract) | 194 | 0 | 194 | 0 |
| `malformed-arguments` (invalid JSON, empty, trailing bytes, duplicate and `\u`-escaped duplicate keys, non-object root, key outside the charset, integer beyond `i64`, depth 9) | 10 | 0 | 10 | 0 |
| `limits` (257 items, 65 parts, 65 tools, 4097-byte descriptions, and on `tight`: 4 items, 5000-byte body, 3 findings; plus 2 boundary rows that pass) | 11 | 2 | 9 | 0 |
| `policy` (block in instructions, input, part, function output, argument leaf, tool description, metadata, beside a redactable secret; warn rejected by default; warn forwarded and `common` profile pass-through on their instances) | 21 | 3 | 18 | 0 |
| `raw-body` (duplicate keys at five depths, lone surrogate, trailing bytes, BOM, invalid UTF-8, `NaN`, truncated document, array root) | 13 | 0 | 13 | 0 |
| `route-isolation` (Responses body on Chat, Chat body on Responses) | 2 | 0 | 2 | 0 |
| **Total** | **358** | **42** | **316** | **0** |

Coverage by slot, enforced by tests that read the case table (`responses-cases.test.ts`, `test_responses_cases.py`): 14 text slots each have a forwarded-and-redacted row; 15 label slots each have a rejected row that planted a synthetic secret in exactly that position; 11 structural fields each have a rejected row proving they cannot carry free text. Observations the cases pin and that are not tidy: the pinned core did not flag the synthetic token in a tool name written `n_<token>` (a word character directly before it), so such a label passes unchanged (`observation-label-with-word-character-before-secret-not-detected`; the bare token in the same slot is rejected); the `common` profile forwards the GitHub-style token unchanged; forwarded `arguments` strings are re-encoded compactly (`1e3` becomes `1000.0`, whitespace removed, `\u00e9` written as UTF-8).

| Acceptance item (#88) | Evidence | Result |
| --- | --- | --- |
| Each accepted text/label class has positive sanitization and negative zero-forward evidence in both SDKs | The groups above, slot-coverage tests, both harnesses | Met (fake-upstream build) |
| Request-local state and independent caller/provider credentials across endpoints | Mixed load: two waves of 28 concurrent requests per SDK on one instance alternating Chat and Responses (16 forwarded: 6 Chat, 10 Responses; 12 rejected), each with its own marker and **its own provider key**; the provider record must show each forwarded request once, on its own path and connection, with its own key's `Authorization` hash, numbering from `<SECRET_1>` and no neighbour's marker, and no record at all for the rejected ones (block, warn, label secret, missing `store`, Chat block and warn). Plus provider-declared `incomplete` (JSON and SSE) and streams in the mix. Authentication: the section 4a table repeated on `/v1/responses` for both instances and both SDKs, one token and one key across both endpoints, `local_token_seen` false on every delivery, 240 unauthenticated announced-4-MiB attempts alternating endpoints all `401` with the provider untouched | Met |
| Supported and rejected helper and replay shapes documented | Section 4c and the [contract table](../contracts/responses-request.md#sdk-helper-output-and-safe-replay-conversion-85-measured-in-88) | Met |
| Report records exact source/core/config/SDK/runtime/artifact pins and evidence layers | Sections 1 and 3 | Met |
| Existing Chat qualification still passes; Beta 1 claims restricted to the reviewed Responses subset | The same full run executed the Alpha 2 cases (123 rows per SDK), the Chat retries, cancellation, streaming, matrix and local-auth suites unchanged (Node 745 tests, Python 167, examples) | Met |

Manual tool round trips (both SDKs, one call and two parallel calls): request with a secret in the prompt, the provider answers with `function_call` output items carrying provider ids and `status`, the application rebuilds the history from `call_id`, `name`, `arguments` plus a `function_call_output` whose result contains a secret, and the second request reaches the provider with compact byte-identical `arguments`, the result redacted and exactly two deliveries on two connections. Replaying `response.output` unchanged is `422` with nothing further delivered.

Retries, cancellation, terminal events and truncation (details and the retry table in [errors-and-telemetry](../contracts/errors-and-telemetry.md#responses-relay-lifecycle-and-terminal-events-87)): with the SDK default the status table matches Chat row for row (three provider deliveries for `408`, `409`, `429`, `5xx`, one for `400`, `401`, `403`, `404`, `422`); with retries disabled every case is one SDK attempt; the gateway never adds a delivery; a stream cut after the headers is not retried; aborts close the provider exchange and 12 JSON plus 12 SSE aborts return every permit; provider-declared `failed` and `incomplete` are relayed as the provider's own terminal event and a clean end or cut never yields a `completed`.

### 4c. SDK helpers, measured against the Responses route

Corrects the contract's earlier type-surface review. Full table in the [contract](../contracts/responses-request.md#sdk-helper-output-and-safe-replay-conversion-85-measured-in-88); the 20 Node and 23 Python helper probes ran through the gateway with the provider record as the oracle.

| Finding | Node.js (`openai` 7.27.0, `zod` 4.6.5) | Python (`openai` 3.24.0) |
| --- | --- | --- |
| Neither SDK sets `store` | `422` unless the call passes `store: false` | the same |
| Structured output helper as emitted | `zodTextFormat` writes `$schema`: rejected; accepted once `$schema` is deleted (flat, nested objects, `z.enum`, `.nullable()`, arrays) | `responses.parse(text_format=Model)`: accepted for flat models, `Literal`, bounds, `list[str]`; rejected for nested models, `Enum` classes, defaults, `pattern`, `datetime`, `UUID` |
| Function tool helper | `zodResponsesFunction` writes `$schema`: rejected, accepted after deleting it; an absent description is omitted | `pydantic_function_tool(Model)` through `responses.parse` is converted to the flat tool but writes `"description": null` without a docstring or `description=`: **rejected** (this corrects the earlier review); accepted with either; through `responses.create` it is Chat-shaped and rejected |
| Typed results | `output_parsed` and `parsed_arguments` returned from the relayed reply | `output_parsed` and `parsed_arguments` returned |
| `responses.stream()` after a cut or a clean end without a terminal event | `finalResponse()` returns `status: "in_progress"` and does not raise on Node.js 22.16.0 | `get_final_response()` raises `APIConnectionError` on a cut and `RuntimeError` without `response.completed` (also for provider-declared `failed` and `incomplete`) |

### 4d. Incremental parse, inspection and serialization cost, next to the Alpha 2 datasets

Raw data: [beta1-responses-incremental-cost-2026-10-03.json](beta1-responses-incremental-cost-2026-10-03.json) (five sequential passes of `qualification/perf/run.mjs --incremental`, which reuses the #58 stage measurement and the #57 shapes and adds the `resp_*` shapes; the Chat shapes ran in the same passes, so the comparison is like for like). Release profile of the qualification build, **not a quiet host** (one-minute load 9.0 to 13.7 on 10 cores; every figure is provisional, ADR 0008). The Alpha 2 dataset ([alpha2-incremental-cost-2026-10-03.json](alpha2-incremental-cost-2026-10-03.json)) was taken on the same machine class with load 5.4 to 10.2; compare shapes within a pass, not across the two files. Median over five passes of each pass's p50; inspection includes serialization; every request returned `200` and no forwarded body held the synthetic token prefix.

| Shape (Responses) | Input bytes | Output bytes | Findings | Parse p50 us | Inspection p50 us | Serialization p50 us | Parse us/KiB | Inspection us/KiB | Chat counterpart: parse / inspection us/KiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `resp_small_4KiB` | 4,150 | 4,150 | 0 | 2 | 58 | 2 | 0.5 | 14.3 | 0.5 / 11.3 |
| `resp_tool_history_4KiB` | 3,981 | 3,981 | 0 | 17 | 79 | 4 | 4.4 | 20.3 | 3.8 / 17.5 |
| `resp_tool_defs_4KiB` | 3,371 | 3,371 | 0 | 13 | 76 | 3 | 4.0 | 23.1 | 3.9 / 19.1 |
| `resp_metadata` | 7,712 | 7,472 | 8 | 7 | 167 | 3 | 0.9 | 22.2 | 0.9 / 19.2 |
| `resp_many_findings_16KiB` | 16,457 | 6,577 | 349 | 3 | 409 | 2 | 0.2 | 25.5 | 0.6 / 42.1 |
| `resp_tool_history_16KiB_findings` | 16,849 | 15,912 | 32 | 47 | 445 | 15 | 2.9 | 27.0 | 3.5 / 27.9 |
| `resp_tool_defs_16KiB_findings` | 13,913 | 12,512 | 48 | 39 | 361 | 10 | 2.9 | 26.6 | 2.9 / 27.9 |
| `resp_format_schema_16KiB_findings` | 14,198 | 13,493 | 24 | 17 | 321 | 7 | 1.2 | 23.2 | (no Chat shape) |
| `resp_large_500KiB` | 512,054 | 512,054 | 0 | 63 | 3,928 | 164 | 0.1 | 7.9 | 0.1 / 7.4 |
| `resp_tool_history` (240 items, 640 KB) | 637,639 | 630,811 | 240 | 1,020 | 12,725 | 384 | 1.6 | 20.4 | 1.7 / 19.8 (Chat: 384 findings, 661 KB) |
| `resp_tool_defs` | 825,847 | 818,571 | 256 | 1,061 | 10,352 | 435 | 1.3 | 12.8 | 1.5 / 14.3 |
| `resp_node_dense` (about 12,800 enum labels) | 84,985 | 84,985 | 0 | 713 | 11,942 | 212 | 8.6 | 143.9 | 8.4 / 140.6 |
| `resp_items_dense` (256 items, 640 slots) | 31,266 | 31,266 | 0 | 282 | 730 | 27 | 9.2 | 23.9 | (new: text-slot dense) |
| `resp_args_dense` (16 calls x 64 string leaves) | 26,960 | 26,960 | 0 | 299 | 2,313 | 75 | 11.4 | 87.9 | (new: argument-leaf dense) |

What the figures support (and no more):

- The Responses shapes cost the same as their Chat counterparts within pass-to-pass noise: parse and inspection per KiB agree to within about 30% in every like-for-like pair, in both directions (Responses is higher for the 4 KiB shapes, lower for `many_findings_16KiB`; the inspection p50 range across passes for one shape is up to about 2.7 times, larger than the Chat-to-Responses difference). Nothing suggests a Responses-specific cost problem.
- Cost follows the number of inspected slots, not bytes: 256 short items or 1,024 argument leaves cost 24 and 88 us/KiB of inspection and 9 to 11 us/KiB of parse against 8 us/KiB for one 500 KiB text, and label-dense schemas stay at about 140 us/KiB, the same as in Chat. The parse stage is on the request thread and is largest per KiB for the item and argument forms (a typed tree plus a second strict parse of each `arguments` string).
- Output growth: without findings the sanitized body is byte-identical to the input for compact shapes; with findings it shrinks (`<SECRET_n>` is shorter than the 40-byte token). It can grow when a finding is shorter than its placeholder; growth is never truncated, the body is held to the output bound and over it is `413 limit_exceeded`. Growth for short findings was not measured.
- Item counts count against `max_messages` (256 by default): a function call and its output are two items in Responses where Chat folds the calls into one message, so a 240-item history is the Responses analogue of Chat's 48-round history.

**Provisional limits for planning, from a not-quiet host** (observations for the maintainers, not defaults, not claims; no default changed): parse at most about 12 us/KiB and inspection at most about 30 us/KiB for item, history, tool-definition, schema and metadata shapes (up to about 42 us/KiB for finding-dense text, 349 findings in 16 KiB, a Chat shape in this pass); about 90 us/KiB for argument-leaf-dense bodies; about 145 us/KiB for label-dense schemas, bounded by `max_nodes`. A 1 MiB Responses history body therefore occupies one inspection worker for about 20 to 30 ms and a label-dense body for about 150 ms; the admission, queue and overload behavior of ADR 0028 applies unchanged and the combined-load results of #58 (Chat) were not repeated for Responses because both endpoints share one handler, one admission path and one set of permits, and the mixed-load test above exercises them together functionally only. No budget becomes a default without a quiet-host record (ADR 0008).

## 5. Configuration lifecycle on the exact binary

`sh scripts/smoke-local-auth.sh <binary> <dir>` (also run by `scripts/smoke-binary.sh`) walks, with `401 missing_credential` as the "authentication passed, stopped before the provider" oracle:

1. Alpha-shaped config (no `local_auth`): starts; `/healthz` and `/readyz` are `200`; a request needs no local header.
2. Upgrade by adding a `token.file` reference and restarting: no header `401 local_auth_required`; wrong or malformed `401 local_auth_invalid`; the right token passes authentication; the local token in `Authorization` does not.
3. Restart activation: after the token file is rewritten, the running gateway still accepts the old token and refuses the new one; after the restart the reverse holds. There is no hot reload and no overlap.
4. Token from the environment.
5. Rollback: the config without `local_auth` starts on loopback with authentication disabled (a reviewed security change, see the contract).
6. Refused before readiness (exit 1, no listener, nothing echoed): non-loopback with acknowledgement and no token (the container shape rolled back), non-loopback without acknowledgement, missing token file, token file readable by other, token under 32 bytes, unset variable, `env` and `file` together, an inline value, a newer `schema_version`.

The pairs and fixtures are those of [config-upgrade-rollback](../contracts/config-upgrade-rollback.md) and `tests/fixtures/config/migration/`. `scripts/probe-endpoints.sh` (run by `smoke-binary.sh`, the image smoke and the Compose smoke) additionally posts ten Responses probes that are all refused locally on the exact bytes: no local token `401`, no credential `401`, `{}` `422`, `store` omitted with a synthetic token `422`, `previous_response_id` `422`, a hosted `web_search` tool `422`, malformed function-call arguments `400`, `text/plain` `415`, `GET` `405`, and the trailing-slash path `404`, none echoing the planted token. This is rejection evidence only.

## 6. Reproduce

```sh
sh qualification/build.sh
(cd qualification/sdk/node && npm ci --ignore-scripts)
(cd qualification/sdk/python && python3 -m venv .venv && .venv/bin/pip install --require-hashes --no-deps -r requirements.txt)
(cd examples/node && npm ci --ignore-scripts)
(cd examples/python && python3 -m venv .venv && .venv/bin/pip install --require-hashes --no-deps -r requirements.txt)
sh qualification/run-suites.sh                       # Chat and Responses suites, local-auth tests, dual-credential examples
cargo build --locked && sh scripts/smoke-local-auth.sh target/debug/redact-secret-gateway evidence/
# provisional incremental cost (release profile; not a quiet host unless you make it one)
sh qualification/build.sh --release && AGG_QUIET_WAIT_MS=0 QUAL_COMMAND='node qualification/perf/run.mjs --incremental --runs 5' \
  sh qualification/run-suites.sh --binary qualification/target/release/redact-secret-gateway-qualification --suites none
```

The Node run writes `local-auth-node.json`, `responses-auth-node.json`, `responses-cases-node.json`, `responses-tools-observations-node.json`, `responses-retries-node.json`, `responses-stream-helper-node.json`, `responses-mixed-node.json` and the Python run the `-python` equivalents into `qualification/evidence/` (status codes, safe codes, counts and helper outcomes only; no token value).

## 7. Known limits

- The gateway cannot tell whether an `Authorization` value is a provider key: an application that sends the local token as its provider key sends it to the provider. Only the SDK configuration prevents that.
- Same-host and same-Pod processes that can read the token source, or share the user, are trusted. The local hop is plain HTTP. Loopback without a token is not authentication. No lockout, per-caller identity or rate limiting.
- A token change is a restart followed by an application update; between the two every proxy request is `401`. No seamless rotation.
- Constant-time comparison is a design property (reviewed code and unit tests); this report measures no timing.
- The unauthenticated load is a bounded ordering check on a loaded developer host, not a capacity claim; no number here is a budget (ADR 0008).
- The exact binary is not driven with an accepted request. Accepted-request behavior is established on the qualification build only.
- Detection remains the pinned core's; no statement here promises that every secret is found. Responses from the provider are relayed unredacted.
- Responses specific: `store:false` is a request-shape rule, not a retention guarantee; the gateway cannot observe provider retention. A client that depends on provider-stored state, reasoning-item replay, hosted tools or newly added fields cannot use the subset. Forwarded `arguments` are re-encoded compactly. Label slots other than call ids are scanned detect-only by the core and, at revalidation, only charset-checked; a secret placed directly after an identifier character in a label was not detected (observed), so labels are not secret-free channels.
- The SDK helper table is for the two pinned versions and the two local runtimes (Node.js 22.16.0, Python 3.14.7); CI runs Node.js 24 and Python 3.13. What a stream helper raises depends on the runtime and is recorded, not asserted.
- The Responses incremental-cost figures are sequential single-client measurements on a loaded macOS host with the release profile of the qualification build; no combined-load, memory or starvation run was repeated for Responses (the handler, admission and permits are shared with Chat, whose #58 dataset stands), and the `measure` CI job runs the same `--incremental` script on a shared Linux runner without committing a dataset.

## 8. Unresolved release gates

- Real-provider evidence: none, and none is planned in this repository. No statement here depends on a provider accepting the sanitized Chat or Responses bodies (for example the combination of `store:false`, a strict `text.format` schema and `tool_choice`).
- Exact candidate bytes accepting a request on either endpoint: not exercised (the binary can only reach the real HTTPS provider). Accepted-request behavior is established on the qualification build only; the candidate bytes are covered for startup, configuration, authentication ordering and rejection.
- Quiet-host performance record: not done; every figure is provisional (Beta 2 scope). Third-party security review, publishing, signing, SBOM and provenance: not done (Beta 2 and Beta 3 scope). The candidate manifest remains `distributable: false`.
- Other SDK versions, other runtimes, and any HTTP client other than the two pinned SDKs: not qualified.
- Nothing here calls schema v1, the limits, the Responses subset or any pair stable or supported for production (checked by `tests/config_migration.rs`).

## 9. Epic exit criteria reconciliation (#12 and #13)

| Epic | Criterion | Evidence | Status |
| --- | --- | --- | --- |
| #13 | All supported Responses fields use complete inspection, bounded transformation and revalidation; stored/opaque/unknown forms cannot bypass | Protocol/boundary tests (`responses_text`, `responses_tools`, slot coverage and revalidation tests), 14 text slots redacted and 15 label slots blocking in the SDK cases, 194 state/opaque/unsupported rows and 38 structural rows rejected with zero upstream | Met on the fake-upstream build |
| #13 | All pre-forward rejection cases deliver zero upstream body; Chat regressions and route/credential isolation still pass | 316 rejected rows per SDK with zero provider requests and connections; Alpha 2 and Chat suites unchanged and green; route-isolation rows; mixed-load and dual-credential tests | Met |
| #13 | Pinned Node/Python qualification covers text/tool/schema round trips, actions, streaming terminals/truncation, retries and cancellation | Sections 4b and 4c; `responses-lifecycle`, `responses-retries-cancel`, `responses-stream-helper`, `responses-tools` suites in both SDKs | Met |
| #13 | README/contracts/examples accurately describe the subset, storage/opaque-state limitations and evidence layers; no generic compatibility or real-provider claim without evidence | README "Beta 1 endpoint matrix", the contract, ARCHITECTURE, ADR 0031 and 0034, the candidate manifest capabilities, section 3 here | Met |
| #12 | Owned checklist #61 to #65 complete; contracts, tests and evidence agree; remaining release or environment limitations stated | #61 to #65 closed with merged PRs; section 4a for Chat, section 4b for Responses; section 8 | Met |

These reconciliations are about repository evidence. They do not make the product releasable.
