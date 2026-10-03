# ADR 0034: Responses qualification and the Beta 1 endpoint matrix

Status: Accepted, by maintainer delegation to the #88 author within the Beta 1 decomposition authorized for epic #13. Records how the Responses subset is qualified and what that qualification does and does not say. No gateway behavior changes; the findings below correct documentation and test fixtures only. Builds on [ADR 0020](0020-sdk-qualification-test-build.md), [ADR 0025](0025-alpha2-field-contract.md), [ADR 0028](0028-aggregate-load-qualification.md), [ADR 0031](0031-responses-stateless-text-contract.md).

## Context

#82 to #87 implemented the stateless Responses text subset and its route and relay. Its contract was written from a type-surface review of the pinned SDKs. Beta 1 needs a reviewed, measured endpoint matrix next to the Chat one, with the same evidence discipline: the fake provider's recorded request as the oracle (a `200` is not an oracle), zero upstream traffic on every rejection, and separate evidence layers.

## Decisions

1. **One field-matrix-driven case table for both SDKs.** `qualification/responses-cases.json` (278 cases, 358 case-by-gateway rows per SDK) is read by both harnesses. Each case carries the slots it exercises; the harnesses fail when a text slot of the contract lacks a redaction row, a label slot lacks a rejected-secret row, or a structural field lacks a rejected-secret row. Expected sanitized bodies are derived from the case parameters, so every retained structure is compared, not only the status.
2. **Evidence layers stay separate and named.** In-process contract tests; the non-release fake-upstream build with pinned SDKs; README examples; the exact candidate binary (startup, configuration, authentication ordering and rejection probes only, now including the Responses route); real provider (none). No report row may be attributed to a layer that did not produce it, and the candidate manifest says the exact artifacts accept no request in any test.
3. **Fake-provider fidelity.** The scripted Responses streams now include `response.output_item.added` and `response.content_part.added` before the first delta, and `output_item.done` before the terminal event, because the SDKs' accumulating `responses.stream()` helpers need them ("missing output at index 0" otherwise). Scenarios were added for function-call output items, a structured-output JSON reply, a slow reply, a hang, retry sequences and retry-hint headers. The fake serves only synthetic data and only loopback.
4. **Measured SDK-helper table replaces the type-surface review** (contract section "SDK helper output"). Two corrections: Python `pydantic_function_tool` through `responses.parse` writes `description: null` without a docstring and is therefore rejected (the contract rejects `null` descriptions; this is not relaxed, because `null` is rejected on every field except the two required-nullable tool keys); Node `zod` helpers accept nested objects once `$schema` is removed. No keyword or field was admitted because of a helper: the compatibility tradeoff stays "reject until reviewed".
5. **Both SDKs must send `store: false`.** Neither SDK sets it, so an unconfigured call is `422`. This is documented as the first thing an SDK user must do, not worked around.
6. **Mixed endpoint load is a functional isolation test, not a capacity result.** Two waves of 28 concurrent requests per SDK alternate Chat and Responses with distinct provider keys and mixed outcomes; the assertion is on the provider's record. Combined-load capacity (#58) was not repeated: both endpoints are one handler over one admission path.
7. **Provisional cost record.** The #57 harness gained the `resp_*` shapes (same sizes and finding counts as the Chat shapes) and runs them in the same passes. Planning figures are recorded in the Beta 1 report as provisional because the host was not quiet; no default changed (ADR 0008).
8. **Candidate manifest capabilities.** `capabilities.proxy.endpoints` lists both endpoints with their subsets. The manifest `status` string keeps its name (`alpha1-release-candidate-unpublished`); renaming it is a release-process decision outside this issue.

## Consequences

- The Beta 1 claims are restricted to the reviewed Responses subset on the fake-upstream build with two pinned SDK versions and the local runtimes named in the report. Nothing claims generic OpenAI compatibility, provider acceptance, retention behavior, or production support.
- A newer SDK that emits a new field is refused until the field is reviewed; the helper table must be re-measured when a pin changes (the suites fail first).
- Runtime-dependent SDK behavior (what a stream helper raises on a cut) is recorded as evidence, not asserted.

## Verification

`qualification/sdk/node/test/responses-*.test.ts`, `qualification/sdk/python/tests/test_responses_*.py`, `qualification/responses-cases.json`, `qualification/fake-provider/responses.mjs`, `qualification/perf/run.mjs --incremental`, `scripts/probe-endpoints.sh`, and the [Beta 1 qualification report](../qualification/beta1-qualification-report.md).
