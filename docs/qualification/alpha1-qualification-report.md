# Alpha 1 qualification report

Status: evidence for issue #22 (Alpha 1, epic #8; scaffolding epic #1). Candidate commit: the pull request that carries this file; CI run links are in "Evidence index". Everything here is synthetic: invented prompts, revoked-looking tokens, a scripted fake provider on loopback, no provider key, no provider network access. **This report does not authorize publication, does not claim a stable-grade audit or support, and lists what is unresolved.**

## What is claimed, and what is not

Claimed (each with the evidence below):

- A pinned OpenAI Node.js/TypeScript SDK and a pinned OpenAI Python SDK drive the gateway's `POST /v1/chat/completions` text subset end to end: supported text forms, planted secrets removed before the provider (placeholder present, structure preserved), rejected inputs delivering **zero** connections and **zero** request bytes to the provider, JSON relay and provider/gateway error handling, SSE relay (fragmented, multibyte, several events per chunk, slow, incremental, interrupted, idle-cut), client abort cancelling the upstream exchange, and repeated cancellation returning every permit.
- The observed retry behavior of those SDK versions, reconciled with the error contract (earlier unverified claims were wrong in two places and are corrected).
- The exact candidate binaries (Linux x86_64, macOS ARM64) and image (linux/amd64) run the documented smoke checks on their own platform and are recorded in a manifest with checksums and pins. The candidate binaries are proven free of the SDK test build's seam.
- The architecture and performance additions of #22 are reconciled with existing and new tests; a synthetic measurement of stage timings and peak memory was run on the qualification build, with the host state stated.

Not claimed:

- **No stable-grade audit, penetration test, or support.** No third-party review exists.
- **Detector recall is not claimed.** Detection quality is the pinned core's (`redact-secret =0.1.0-beta.12`). The suites plant one known synthetic token shape and assert it never reaches the provider; they generate no obfuscated variants.
- **Provider responses are not redacted** (JSON bodies, provider error bodies, SSE events) and can contain sensitive data.
- **Loopback is an address restriction, not authentication.** Any process on the host that can reach the port can use the gateway with its own provider key.
- **Egress bypass needs operator controls.** The gateway cannot stop an application, or another process, from talking to the provider directly. Resolver integrity, the trust store, TLS-intercepting proxies, and any intermediary in front of the gateway are operator responsibilities ([threat-control map](alpha1-threat-control-map.md)).
- **The real provider path was not exercised.** The SDK suites ran against a loopback fake behind a test-only destination; the shipped binary's HTTPS, platform-trust, and public-address path is covered by unit tests with throwaway certificates and by smoke checks that never reach a provider, not by an SDK against OpenAI. The examples have never been run against the real provider by this repository.
- **No performance, faster, lower-latency, or zero-copy claim.** Numbers are synthetic measurements with the host state stated, and none enters a default.
- **No exactly-once or at-most-once delivery.** SDK retries can duplicate provider-side work; bytes already sent cannot be retracted.
- **Signing, SBOM, provenance, and bit-for-bit reproducibility** are not produced or verified (planned for Beta 3).

## Method: a separate non-release test build

The destination policy forbids any seam to a fake provider in a release binary ([ADR 0013](../decisions/0013-fixed-https-destinations-and-outbound-authority.md)). The maintainer approved a separate non-release build ([ADR 0020](../decisions/0020-sdk-qualification-test-build.md)): `qualification/build.sh` generates a crate named `redact-secret-gateway-qualification` from a copy of `src/`, a five-file seam patch, and three overlay files; the shipped package is untouched. The suites run it against `qualification/fake-provider/server.mjs`, which scripts JSON responses, provider 4xx/5xx, fragmented/multibyte/multi-event/slow/gated/interrupted/hung SSE, and records exactly what reaches it. The pinned SDKs are installed from lockfiles with integrity hashes (`npm ci --ignore-scripts`; `pip install --require-hashes --no-deps`).

| Pin | Value |
| --- | --- |
| Node.js SDK | npm `openai` 7.27.0 (zero runtime dependencies), with TypeScript 5.9.3 and `@types/node` 24.19.1 for type-checking; Node.js 24 in CI (local runs also on 22.16 with `--experimental-strip-types`) |
| Python SDK | PyPI `openai` 3.24.0 with its hash-locked dependency set (15 packages, including `httpx2` 2.13.1 and `pydantic` 2.13.5); Python 3.13 in CI (local runs on 3.14.7) |
| Core | `redact-secret =0.1.0-beta.12`, profile `full` for the suites |
| Toolchain | Rust 1.98.1 (`rust-toolchain.toml`) |

Gateway instances used by the suites (all the qualification binary, same production request path): `standard` (receipt 8, memory 64 MiB, inspection 2, upstream 8, stream 8), `tight` (4 KiB body, 3 messages, 2 findings, 500 ms provider header deadline, 500 ms stream idle), `noupstream` (no `deployment.upstream`), `deadprovider` (provider address with nothing listening), and `overload` (one upstream and one stream permit).

## SDK results

Suite size: Node.js 70 tests (`node:test`, TypeScript type-checked), Python 33 tests (`unittest`); both pass in the CI run and were run repeatedly for determinism (see "Evidence index"). Waiting on the fake provider is event-driven (an admin endpoint resolves when an event happens; a deadline only converts a hang into a failure); the only timing assertions are lower bounds on provider-paced streams and on the honored `Retry-After`.

| Area | What the suites assert (Node and Python alike unless noted) |
| --- | --- |
| Supported text forms | Four roles (`system`, `developer`, `user`, `assistant`), string and text-parts content, sampling and limit controls, `stop`, `user`, `response_format` `json_object`, `n: 1`: the provider receives a body deep-equal to the request; only gateway-built headers arrive (SDK telemetry headers stripped, outbound `User-Agent` is the gateway's); the caller's credential reaches the provider (hash compare) |
| Secret removal and structure | Three distinct planted tokens (in a message, a text part, and `stop`): none reaches the provider, `ghp_` absent, exactly three `<SECRET_n>` placeholders, surrounding text and Korean text intact, and replacing the placeholders by the tokens restores the original request exactly (so only the secrets changed). Streamed requests behave the same and keep `stream_options` |
| Rejected inputs never deliver upstream bytes | 14 unsupported forms (unknown field, `tools`, `tool_choice`, `image_url` part, unknown field in a part, `metadata`, `logprobs`, message `name`, `tool` role, `null` content, `n: 2`, `json_schema`, secret beside an unknown field, streamed request with an unknown field), a finding in `model`, a `Warn` finding, finding-limit exhaustion, an over-limit body, too many messages, a missing credential, and a deployment without upstream (JSON and stream): each is the documented status and fixed safe code, the error carries no request text, and the provider's record shows **0 connections and 0 requests** |
| JSON and errors | Large completion intact; provider `400/401/403/404/409/422/429/500/503` relayed with the provider's own JSON error body (not the gateway envelope) and the SDK's typed error; gateway `502 upstream_response_too_large`, `502 upstream_invalid_response`, `502 upstream_unavailable`, `504 upstream_timeout`, `501 not_implemented` with their fixed codes |
| SSE | `ok`, `multi` (three events per chunk), `fragmented` (fragments of 1 to 7 bytes, splitting multibyte characters): the text `Hello 안녕하세요 🙂!` and eight events arrive intact and in order; incremental relay proven by a gated provider (the first event reaches the SDK while the provider still holds the stream and has not finished); slow stream arrives spread over time; provider error before the stream is a normal API error |
| Interrupted streams | Provider cut after two events and gateway idle-deadline cut: no completion is fabricated (`finish_reason` never appears) and the text is visibly incomplete. **Python raises `APIConnectionError`. Node.js raised on Node 24 (CI) and did not on Node 22.16.0** (finding below) |
| Cancellation | Aborting a JSON request while the provider holds it, and aborting an SSE stream mid-flight, both make the provider connection close; 12 aborted JSON requests and 12 aborted streams against 8 upstream and 8 stream permits, followed by normal JSON and SSE requests, prove every permit returned |
| Logs | Gateway stdout and stderr of every instance are captured and scanned for every synthetic marker (token prefix, key, prompt marker, provider reply and error text, Korean reply text): none found. The gateway emits no request logs at all (about one kilobyte per run: the startup lines) |
| README examples | `examples/node` (`npm start -- --demo-redaction`) and `examples/python` (`--demo-redaction`) run exactly as documented against the qualification build: two provider calls each, the synthetic token replaced by `<SECRET_1>`, stream flag preserved; without a key each exits 2 |

**Finding: whether a Node.js caller sees stream truncation depends on the Node.js version.** After the response headers the gateway ends a broken stream without the terminating chunk (ADR 0018). The Python SDK (httpx) raises. The Node SDK uses Node's built-in fetch (undici) and every gateway response carries `Connection: close` (ADR 0019): on **Node 24.21.0 in CI the SDK raised** (provider cut and gateway idle cut), but on **Node 22.16.0 (undici 6.21.2, local runs) it ended normally**, treating the close of the chunked `Connection: close` body as a clean end; a control in the suite shows the same Node 22 fetch raises for the same truncation on a keep-alive response. In every case no completion is fabricated, so a Node caller that requires `finish_reason` is correct on any version, and the shipped Node example does (it also requires Node 24 or newer). Documentation corrected ([errors and telemetry](../contracts/errors-and-telemetry.md), ADR 0018). Whether the gateway should signal truncation more forcefully for older undici (for example an abortive close) is a follow-up for the maintainer, not decided here. The first draft of this finding, written from Node 22 runs only, said "Node cannot see truncation"; the CI run on Node 24 contradicted it and it was corrected.

### Observed SDK retry behavior

Defaults (`maxRetries` / `max_retries` = 2), counted with SDK-side hooks and the provider's record; identical for both SDKs on every row. The full table and guidance are in [errors and telemetry](../contracts/errors-and-telemetry.md#sdk-retry-guidance-observed-in-22).

- Not retried (1 attempt): provider `400`, `401`, `403`, `404`, `422`; gateway `422`, `413` (0 provider requests); a stream cut after the headers.
- Retried (3 attempts): provider `408`, `409`, `429`, `500`, `502`, `503`, `504` (3 provider requests each); gateway `501` (0), `502 upstream_unavailable` (0), `502 upstream_response_too_large` and `502 upstream_invalid_response` (3 each), `504 upstream_timeout` (3), `503 overload` (0; the SDK waited the relayed `Retry-After: 1` between attempts), connection refused (0); a provider `429` before a stream starts (3).
- **Corrections made to the earlier text:** (1) the SDKs obey `x-should-retry` and `retry-after-ms`, but the gateway's response-header allowlist drops both, so a provider that says "do not retry" is retried through the gateway (observed with a fake that sends them); (2) the earlier claims that SDKs "may retry" and "raise" on a cut stream were wrong in part: neither retried it, and Node raised only on Node 24 (not on 22.16.0).

## Acceptance reconciliation

### Issue #22: deliverables and acceptance criteria

| Item | Status | Evidence |
| --- | --- | --- |
| Candidate-specific end-to-end tests: supported text forms, core completeness failures, no-forward negative paths, JSON/SSE relay, cancellation, limits, safe diagnostics | Met (qualification build, not the shipped bytes) | SDK suites above; `https://github.com/redact-secret/gateway/actions/runs/37044335074` |
| Pin Node/Python SDK versions; real baseURL examples; Compose and static config; SDK retry guidance | Met | `examples/node`, `examples/python`, `examples/compose/compose.yaml`, `examples/config.openai.json`; lockfiles with integrity hashes; guidance in errors-and-telemetry |
| Build and execute the exact Linux x86_64 / macOS ARM64 binaries and the Linux amd64 image; record source/toolchain/core/config/SDK pins and checksums | Met for the unpublished candidate | `Candidate artifacts` run `https://github.com/redact-secret/gateway/actions/runs/37044335173`; manifest v2 and `SHA256SUMS` in the bundle (values below) |
| Verify license and private reporting; document loopback trust, egress bypass, pre-Gateway exposure, response pass-through, redaction semantics | **Met** | License MIT (`LICENSE`, `Cargo.toml`); private reporting API `enabled: true` (checked 2026-10-02, ADR 0010). Documentation of the trust assumptions: README, SECURITY.md, threat-control map, this report |
| Reconcile Alpha 1 blockers; reviewable release evidence; no publication authorized | Met (this document and the manifest) | "Unresolved blockers" below |
| AC: exact candidate artifacts pass documented startup and MVP smoke checks on supported environments | Met | Smoke matrix in [docs/artifacts.md](../artifacts.md); `https://github.com/redact-secret/gateway/actions/runs/37044335173` |
| AC: rejected inputs never deliver upstream body bytes; auth/destination/diagnostic tests pass | Met | Provider-side zero-connection assertions (SDK suites); `src/transport/tests/forward_tests.rs`, `attack_tests.rs`, `tests/destination_policy.rs`, `tests/diagnostic_surface.rs` ([control map](alpha1-threat-control-map.md)) |
| AC: a clean user can execute verified examples against the supported endpoint subset | Met for the steps CI can run; the real-provider hop is not verified here | Root README "Try it" with a verification column; `qualification/run-examples.sh` in the qualification workflow |
| AC: README distinguishes implemented features from Alpha 2/Beta plans | Met | README "What works today and what is planned" |
| AC: release evidence identifies unresolved blockers and does not claim stable-grade audit or support | Met | This document |

### Issue #22: architecture and performance additions

| Item | Status | Evidence |
| --- | --- | --- |
| Sealed forwarding types and constructor restrictions; protocol modules with no independent transport; immutable startup plan; policy-authority separation | Met | Sealed types: `tests/api_boundary.rs` with `tests/ui/fail_forward_*`, `fail_construct_*`, `fail_mutate_sanitized_*`, `fail_clone_permit`, `fail_sanitized_*` and the positive controls `pass_forward_*`. No protocol-side transport and plan immutability: `tests/dependency_policy.rs::module_authority_direction_holds` and the new `tests/architecture_reconciliation.rs` (no network types in `protocol`/`boundary`/`core_bridge`; only `transport` builds HTTP clients; configuration read only by the CLI at startup; no `&mut self` or interior mutability on the plan; only the CLI writes output). Policy-authority separation: `src/transport/tests.rs::profile_variation_cannot_change_destination_or_tls`, `wire_tests.rs::profile_and_deployment_state_do_not_alter_header_policy`, `tests/destination_policy.rs` (no content or resource field reaches destination or client construction) |
| Separate receipt/CPU/memory/upstream/stream budgets and correct permit lifetimes under overload and repeated cancellation, including non-interruptible jobs | Met | `tests/permits_capacity.rs::each_class_is_bounded_and_independent_of_the_others`, `::overload_is_immediate_and_bounded_not_queued`, `::stream_occupancy_does_not_hold_a_completed_inspection_slot`; `tests/permits_cancellation.rs::dropping_the_http_waiter_does_not_release_running_job_permits`, `::repeated_cancellations_stay_within_inspection_and_memory_bounds`, `::early_release_check_detects_permits_tied_to_the_waiter` (negative control); `tests/core_probe_scheduling.rs`; `forward_tests.rs::overload_is_immediate_bounded_and_returns_all_capacity`; `stream_tests.rs::stream_capacity_bounds_streams_upstream_occupancy_and_tasks`; `attack_tests.rs::a_flood_of_abandoned_requests_returns_every_permit_and_task`. **New, through real SDKs:** 12 aborted JSON requests and 12 aborted streams against 8 and 8 permits, then normal requests succeed (a leak would have exhausted capacity); a `503 overload` with one upstream and one stream permit held by a stalled stream, nothing sent for the refused requests |
| Duplicate-key-rejecting parsing, decoded text, bounded original/parsed/output lifetime, no raw payload copies in error or log types | Met | `tests/parser_conformance.rs` (shared table, three parser paths), `src/protocol/json.rs` tests, `tests/chat_admission.rs::escapes_and_unicode_are_decoded_before_inspection`; lifetimes: ADR 0007 and `ValidatedRequest`/`SanitizedRequest` ownership tests; error and log types: `tests/diagnostic_surface.rs` (every error type `Copy`, fixed alphabet), `tests/ui/fail_serialize_error_types.rs`, and the new check that no error enum owns text and no code outside the CLI prints or logs; the SDK suites scan gateway stdout and stderr for planted markers |
| Record safe synthetic admission/parse/core/serialize/Gateway/first-upstream timing distributions and peak memory for no-findings, many-findings, large, concurrent workloads and slow SSE | Met, labelled by host state | "Performance" below; raw data in [alpha1-stage-timing.json](alpha1-stage-timing.json) |
| Document measured worker/budget/parser/pool choices; no faster or zero-copy claims without results | Met (choices remain provisional) | "Measured choices" below |

### Epic #8: acceptance direction and completion additions

| Item | Status | Evidence |
| --- | --- | --- |
| Supported synthetic requests preserve structure and transmit only the processed body | Met | SDK text-forms and secret-removal tests; `forward_tests.rs` exact-body test |
| Malformed, unsupported, oversized, unknown-field, incomplete-scan requests transmit zero upstream body bytes, with fake-upstream evidence | Met | SDK rejection suites (provider record), `tests/chat_admission.rs`, `forward_tests.rs`, `attack_tests.rs` |
| JSON/SSE happy paths and interrupted streams work with pinned Node/Python SDKs; logs and errors contain no synthetic secret markers | Met, with the Node truncation finding documented | SDK suites; log scan |
| Linux x86_64/macOS ARM64 binaries and amd64 image run the same qualified candidate; license/reporting prerequisites satisfied | Candidate built and smoke-tested once and checksummed (met); **private-report test not recorded (open)** | `https://github.com/redact-secret/gateway/actions/runs/37044335173`; ADR 0010 |
| Advertised request subset, residual risks, and response pass-through contract match behavior | Met | Field matrix tests; this report; README |
| Tests prove invalid/incomplete/cancelled-before-forward results cannot construct or send an approved request | Met | Compile-fail tests (`tests/ui`); `permits_cancellation.rs::a_cancelled_result_never_becomes_an_upstream_request`; `forward_tests.rs::a_cancelled_waiter_never_starts_an_upstream_request` |
| CPU work still running after cancellation retains capacity; long SSE responses use separate reservations | Met | `permits_cancellation.rs`, `core_probe_scheduling.rs`, `stream_tests.rs::open_streams_do_not_hold_inspection_memory_or_receipt_capacity` |
| Shared clients reuse connections while credentials remain request-local | Met in part by design | Credentials are request-local and concurrent distinct keys do not cross (`wire_tests.rs::concurrent_requests_with_different_keys_do_not_cross`, `attack_tests.rs::credentials_do_not_survive_into_the_next_request_on_the_shared_client`). The client is shared; **connection reuse is deliberately off** (`pool_max_idle_per_host(0)`, ADR 0017) so a request is never replayed on a stale connection; the cost is unmeasured against pooling (open follow-up #42) |
| Candidate performance report separates Gateway overhead from upstream model delay and records peak memory | Met, provisional | "Performance" below |

Sub-issues of #8: #18, #19, #23, #24, #25 merged; #20 and #21 are merged with their SDK boxes closed by this change (see the next table); #22 is this change. Whether #22 can be closed is the maintainer's call, because one acceptance item (private-report verification) is outside the repository.

### Issues #20 and #21: the SDK boxes this change closes

| Issue | Box | Evidence |
| --- | --- | --- |
| #20 | JSON response/status handling works with pinned Node/Python clients and safe error contracts | JSON and error rows of the SDK results (Node and Python), `https://github.com/redact-secret/gateway/actions/runs/37044335074` |
| #21 | Pinned Node/Python streaming clients work against fragmented synthetic SSE including multibyte text and multiple events per transport chunk | SSE rows (`fragmented`, `multi`, `ok`), `https://github.com/redact-secret/gateway/actions/runs/37044335074` |

The #21 box "slow consumer, upstream disconnect, malformed response and timeout paths match the documented termination contract" was already evidenced at the HTTP level in `stream_tests.rs`; the SDK runs add the client view (interrupted and idle-cut streams) and the documented Node difference.

### Epic #1: completion evidence (scaffolding)

| Item | Status |
| --- | --- |
| Linked sub-issues complete with validation evidence | #2, #4, #5, #6, #7 are checked on the epic; #3 (baseline documents and boundary ADRs) is unchecked on the epic and is for the maintainer to reconcile; the ADR set 0001 to 0020 exists in the repository |
| Exact dependency/core pins and toolchain recorded; core has no reverse dependency | Met (`Cargo.lock`, ADR 0011, manifest `core`) |
| Skeleton starts safely, validates config, rejects unsupported routes, no upstream body transmission | Met (and superseded by the proxy; the smoke probes still send only locally rejected requests) |
| Fake-upstream and core completeness probes run in CI without real credentials | Met (`cargo test`, `tests/core_probe_*`) |
| Build outputs match the documented target matrix and are labelled accurately | Met (manifest `status: alpha1-release-candidate-unpublished`, `distributable: false`) |
| Baseline docs, threat model, license decision, and private reporting readiness reconciled | License decided (MIT); private reporting enabled and verified via the API (2026-10-02) |

## Candidate artifacts

The `Candidate artifacts (Alpha 1, unpublished)` workflow builds each target once with `cargo build --locked --release` and smoke-tests those exact bytes on their own platform; the full check matrix is in [docs/artifacts.md](../artifacts.md). Run: `https://github.com/redact-secret/gateway/actions/runs/37044335173`.

The manifest and checksums below are from run `https://github.com/redact-secret/gateway/actions/runs/37044335173` (commit `441023e2ebf538e0275487442f82975c51dc7c40`, the pull request's merge commit; `working_tree_dirty: false`; rustc 1.98.1 `48a229cea 2026-09-01` on both build platforms). They describe the unpublished candidate bundle `alpha1-candidate-441023e...`:

| Artifact | Platform | sha256 | Bytes |
| --- | --- | --- | --- |
| `redact-secret-gateway-x86_64-unknown-linux-gnu` | linux-x86_64 (built and smoke-tested on `ubuntu-24.04`) | `c8dc31ea950993243b281c759f9c7689533391e0df4846b0849686ea00a89740` | 9,435,448 |
| `redact-secret-gateway-aarch64-apple-darwin` | macos-arm64 (built and smoke-tested on `macos-15`) | `94327b719c43185a36181a8061c1c36b9c9b218d428089e07139ea45946b408f` | 7,719,904 |
| `redact-secret-gateway-candidate-image-linux-amd64.tar` (image id `sha256:0cbbbbbfa3e0fce7c6b58c7803e554f82db9353b80092db2c0af24eb0d81cc7c`, runs as 65532:65532, contains the Linux binary above, byte-identical) | linux-amd64 (built from that binary, smoke-tested with Docker on `ubuntu-24.04`) | `b4cf0a3a90a57a85d022b5513f4b11f0889a8261165bc8083116ef915af7958b` | 38,630,400 |

Recorded in `manifest.json` (version 2): gateway `0.1.0-alpha.0`; toolchain channel `1.98.1`; core `=0.1.0-beta.12` with its `Cargo.lock` checksum `2cc951e8...ed9d856d`; `Cargo.lock` sha256 `b7079542fdafb6f9c81e7e7a7c3075fdb9d321083989ef5111193e51af8604f9`; config schema version 1; the sha256 of `examples/config.openai.json`, `examples/config.skeleton.json`, `container/config.container.json`, and `examples/compose/compose.yaml`; the SDK pins (npm `openai` 7.27.0 and PyPI `openai` 3.24.0 with lockfile sha256s); `capabilities.proxy`; `signing`, `sbom`, `provenance` all `"not produced"`; `distributable: false` with six blockers. Observations, not claims: an earlier run on the same sources produced byte-identical binaries (same sha256 for Linux and macOS), but bit-for-bit reproducibility is not verified or claimed, and the image archive differs run to run.

Smoke checks that passed on those bytes (per-platform logs are in the bundle's `evidence/`): the seam-absence check on the Linux binary, the macOS binary, and the binary extracted from the image (byte-identical to the Linux candidate; checked against its recorded checksum); `--version`; `validate-config` for the shipped config; startup on loopback; `/healthz` and `/readyz`; the seven locally rejected proxy-route probes with their status and safe code; SIGTERM with exit 0 and `shutdown complete`; for the image, non-root uid 65532, read-only root, all capabilities dropped, its own config valid, host-loopback publication; and the Compose example started and probed against the candidate image.

## Performance (ADR 0008): synthetic stage timing and peak memory

**Host state, stated plainly.** darwin 25.5.0 arm64, 10 CPUs, shared with other users and processes. One-minute load average 5.17 before and 5.27 after (five-minute 6.2, fifteen-minute 10.74). This is **not a quiet host**, so the run is labelled `provisional` in the data (`provisional: true`) and **every figure below is provisional**. Release profile of the qualification build (`redact-secret-gateway-qualification 0.1.0-alpha.0`; not the shipped artifact, not byte-identical to it), Node.js v22.16.0 as the load generator on the same host, scripted fake provider on loopback replying immediately, Rust 1.98.1, core `0.1.0-beta.12`, profile `full`, all limits at their provisional defaults, capacity `receipt 16`, `memory_units 262144`, `upstream 16`, `stream 16`, `inspection` as listed. A hosted CI runner is not a quiet host either; the `measure` job of the qualification workflow produces the same report there, also provisional.

Method: for sequential workloads the gateway's own stage counters are read before and after each request, so each delta is exactly that request (1 microsecond resolution; a stage under 1 microsecond reads 0). One warm-up request is not recorded. Payloads are invented filler; the many-findings payload is 349 distinct revoked-looking tokens, all replaced (349 placeholders in the forwarded body, none of the token prefix). All requests succeeded (`200`). Values are microseconds, `p50 / p95 / p99`.

| Stage (microseconds) | no findings, 4 KiB (200 requests) | many findings, 16 KiB (349 tokens) (100 requests) | large input, 384 KiB (20 requests) |
| --- | --- | --- | --- |
| Admission wait | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| Parse (receipt-to-validated) | 2 / 5 / 10 | 3 / 4 / 5 | 38 / 41 / 42 |
| Core inspection (inspection minus serialization, includes worker queueing) | 45 / 86 / 115 | 328 / 346 / 372 | 2735 / 2772 / 2812 |
| Serialization | 1 / 2 / 3 | 2 / 2 / 3 | 105 / 113 / 114 |
| Upstream first response (send to response headers, loopback fake) | 158 / 489 / 1009 | 150 / 234 / 335 | 521 / 957 / 1483 |
| Upstream total | 160 / 491 / 1021 | 152 / 237 / 337 | 524 / 961 / 1486 |
| Gateway total excluding upstream (client end to end minus upstream total; approximate) | 343 / 1215.7 / 2418.7 | 598.7 / 765.2 / 2022.7 | 3391.5 / 5820.2 / 6059 |
| Client end to end | 502.4 / 1718.6 / 2593.6 | 751.5 / 1002.2 / 2299.3 | 3902.5 / 6315.2 / 6763 |

Peak resident memory of the gateway process (sampled with `ps` every 25 ms during each workload, so a sampled maximum rather than a true peak on this OS; `VmHWM` is not available on macOS and would be the true peak on Linux, where the CI run reads it): no findings, 4 KiB 9632 KiB; many findings, 16 KiB (349 tokens) 10464 KiB; large input, 384 KiB 11120 KiB. The baseline process is about 9 MiB; the 384 KiB request adds about 1.5 MiB at the sampled maximum.

**Concurrency (8 simultaneous local clients, same payloads) and the effect of the inspection capacity.** Throughput is requests per second over the whole run; `503` counts are immediate `overload` rejections (queued plus running inspection jobs never exceed `inspection`, so a ninth in-flight inspection is refused, not queued).

| Workload | `inspection` | Requests | `200` / `503` | Throughput (req/s) | Client end to end p50 / p95 / p99 (us) | Mean / max inspection (us) | Peak RSS sampled (KiB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| no findings 4KiB | 1 | 200 | 140 / 60 | 2855.4 | 2285 / 5774.6 / 6583.7 | 105 / 781 | 9872 |
| many findings 16KiB | 1 | 80 | 20 / 60 | 4660.3 | 1333.7 / 2677.4 / 3380.7 | 480 / 781 | 10768 |
| no findings 4KiB | 2 | 200 | 169 / 31 | 4183.4 | 1615.7 / 3273 / 5705.3 | 100 / 520 | 10032 |
| many findings 16KiB | 2 | 80 | 38 / 42 | 4895.2 | 1166.2 / 3222.9 / 3839.2 | 481 / 791 | 11360 |
| no findings 4KiB | 4 | 200 | 196 / 4 | 4076.6 | 1806.5 / 2959.3 / 6308.7 | 113 / 414 | 10208 |
| many findings 16KiB | 4 | 80 | 63 / 17 | 3929.2 | 1950.8 / 3198.4 / 4242.7 | 577 / 1199 | 11936 |

**Slow SSE.** The scripted provider emits eight events 25 ms apart; the consumer either reads at once, or sleeps 20 ms after every read (a deliberately slow consumer); and eight consumers at once. Gateway means come from the stream counters (upstream wait is time spent waiting on the provider for the next chunk; downstream wait is consumer and socket backpressure; relay overhead is total minus both).

| Case | Streams | `200` / `503` | Client first event p50 / p95 / p99 (us) | Client stream total p50 (us) | Gateway mean: first byte / total / upstream wait / downstream wait (us) | Peak relay buffer (bytes) | Peak RSS sampled (KiB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| slow provider fast consumer | 20 (1 at once) | 20 / 0 | 1568.7 / 2138.6 / 2194.2 | 235614 | 517 / 234419 / 233919 / 9 | 193 | 11200 |
| slow provider slow consumer | 10 (1 at once) | 10 / 0 | 1656.2 / 4023.8 / 4023.8 | 234635 | 664 / 233370 / 232711 / 9 | 193 | 11200 |
| slow provider 8 concurrent streams | 40 (8 at once) | 38 / 2 | 2745.6 / 3588.4 / 3889.8 | 234608 | 778 / 232844 / 232077 / 3 | 193 | 11680 |

What the numbers say, and no more: on this loaded host the gateway's own work for a small request (admission, parse, core, serialization) is a fraction of a millisecond and is dominated by core inspection and by connection setup to the provider; the time for a 349-token request is about 7 times the no-findings core time and for a 384 KiB request about 60 times (core cost scales with text and findings); a slow stream's total is the provider's pacing, the relay overhead is a few hundred microseconds, the relay buffered at most one provider chunk (193 bytes), and a slow consumer's wait shows up as downstream wait (backpressure) without growing memory. These are observations of one synthetic run, not claims.

**Second dataset: the same run on a GitHub-hosted Linux runner.** linux 6.17.0-1022-azure x64, 4 vCPUs, Node.js v24.21.0, one-minute load 2.45 at the start. A shared VM is not a quiet host, so it is also `provisional: true`. On Linux the gateway reports its own `VmHWM`, a true peak. Raw data: [alpha1-stage-timing-ci-linux.json](alpha1-stage-timing-ci-linux.json) (workflow run `https://github.com/redact-secret/gateway/actions/runs/37044335074`, artifact `perf-evidence-*`). Microseconds, `p50 / p95 / p99`.

| Workload | Parse | Core inspection | Upstream first response | Gateway total excl. upstream (approx) | Peak RSS, true `VmHWM` (KiB) |
| --- | --- | --- | --- | --- | --- |
| no findings, 4 KiB (200 requests, statuses {'200': 200}) | 5 / 8 / 20 | 122 / 163 / 235 | 379 / 655 / 1248 | 1099.8 / 6920.6 / 8375.8 | 7468 |
| many findings, 16 KiB (100 requests, statuses {'200': 100}) | 7 / 9 / 11 | 652 / 882 / 986 | 364 / 461 / 486 | 1525.6 / 7971.5 / 8461.4 | 7892 |
| large input, 384 KiB (20 requests, statuses {'200': 20}) | 206 / 247 / 250 | 5220 / 5483 / 6165 | 1208 / 2048 / 5650 | 7874.7 / 13034.2 / 13585 | 9960 |

Concurrency on the runner (8 clients): no findings 4KiB at `inspection` 1: 65 of 200 refused; many findings 16KiB at `inspection` 1: 57 of 80 refused; no findings 4KiB at `inspection` 2: 26 of 200 refused; many findings 16KiB at `inspection` 2: 39 of 80 refused; no findings 4KiB at `inspection` 4: 3 of 200 refused; many findings 16KiB at `inspection` 4: 15 of 80 refused. The pattern matches the local run: the refusal rate falls as `inspection` rises. Slow SSE on the runner: slow provider fast consumer: first event p50 1741.2 us, stream total p50 228822 us, peak relay buffer 193 bytes; slow provider slow consumer: first event p50 1720.2 us, stream total p50 227913 us, peak relay buffer 193 bytes; slow provider 8 concurrent streams: first event p50 2513.2 us, stream total p50 229901 us, peak relay buffer 193 bytes.

### Quiet-host rerun (2026-10-02, #42, ADR 0024): not quiet, five runs, provisional

**Environment.** Apple M4, 10 cores, macOS arm64 (Darwin 25.5.0), Rust 1.98.1, release profile of the qualification build from commit `d85e07d`, core `0.1.0-beta.12`, Node.js v22.16.0 as load generator, scripted loopback fake provider (plain HTTP), profile `full`, all limits at their provisional defaults, capacity as in the first run. Five consecutive runs of the unchanged `qualification/perf/run.mjs`, each launched directly. **The host was not quiet:** the criterion for calling a run quiet is a one-minute load average at or below about 25% of the core count (2.5), and every run started above it. One-minute load before and after each run: 7.59 / 6.89, 6.58 / 6.20, 6.20 / 5.90, 5.75 / 5.71, 5.97 / 5.84. The busiest processes were macOS system services (`StorageManagementService`, `ApplicationsStorageExtension`, `syspolicyd`) and, in one sample, a browser helper, never the gateway. The data is labelled `provisional: true`. Raw data with per-run load and top consumers: [alpha1-stage-timing-quiet-host-rerun-2026-10-02.json](alpha1-stage-timing-quiet-host-rerun-2026-10-02.json). A first set of five runs, launched from a wrapper script, carried an extra constant of about 72 ms in every client-side figure (and none in the gateway's own stage counters); it was discarded and is described in ADR 0024.

Each cell: median over the five runs of the per-run percentile, then the range over the runs in brackets; the earlier single run (load 5.2) in parentheses. Microseconds.

| Stage | no findings, 4 KiB (200 requests per run) | many findings, 16 KiB, 349 tokens (100) | large input, 384 KiB (20) |
| --- | --- | --- | --- |
| Parse p50 / p95 | 2 [2 to 3] / 4 [4 to 8] (2 / 5) | 3 [3] / 5 [5 to 6] (3 / 4) | 40 [39 to 41] / 47 [42 to 88] (38 / 41) |
| Core plus queueing, inspection p50 / p95 | 47 [47 to 55] / 59 [57 to 101] (46 / 87) | 331 [328 to 336] / 366 [350 to 371] (328 / 346) | 2,853 [2,836 to 2,984] / 2,956 [2,860 to 3,262] (2,735 / 2,772) |
| Serialization p50 | 1 [1] (1) | 2 [2] (2) | 106 [105 to 107] (105) |
| Upstream first response p50 / p95 (loopback fake, no TLS) | 171 [153 to 255] / 357 [321 to 565] (158 / 489) | 160 [153 to 187] / 307 [258 to 367] (150 / 234) | 602 [530 to 623] / 1,000 [835 to 1,094] (521 / 957) |
| Gateway total excluding upstream p50 / p95 (approximate) | 355 [332 to 461] / 1,030 [770 to 2,362] (343 / 1,216) | 618 [614 to 634] / 821 [761 to 1,008] (599 / 765) | 3,499 [3,406 to 3,766] / 5,612 [4,601 to 5,801] (3,392 / 5,820) |
| Client end to end p50 / p95 | 532 [486 to 728] / 1,410 [1,176 to 2,808] (502 / 1,719) | 778 [765 to 826] / 1,154 [1,061 to 1,307] (752 / 1,002) | 4,126 [4,037 to 4,512] / 6,194 [5,088 to 6,855] (3,903 / 6,315) |
| Peak sampled resident memory (KiB) | 9,664 [9,616 to 9,696] (9,632) | 10,496 [10,432 to 10,640] (10,464) | 11,872 [11,104 to 12,448] (11,120) |

All sequential requests succeeded in all five runs. Slow SSE (eight 25 ms-paced events): client first event p50 1,615 us (1 stream, fast consumer), 1,682 us (slow consumer), 2,882 us (8 concurrent streams, one `503` in 40 in the median run); stream total p50 235,674 / 235,416 / 237,401 us, so the provider's pacing is the stream time; peak relay buffer 193 bytes in every run; sampled resident memory 9.8 to 12.7 MiB.

Concurrency (8 clients, `503` counts out of the stated requests, median over five runs [range]; throughput varies more than five-fold between runs on this host, so it is not given as a figure):

| Workload | `inspection` 1 | `inspection` 2 | `inspection` 4 |
| --- | --- | --- | --- |
| no findings, 4 KiB (200 requests) | 71 refused [51 to 73] (earlier 60) | 20 [16 to 33] (31) | 1 [1 to 4] (4) |
| many findings, 16 KiB (80 requests) | 56 [49 to 58] (60) | 44 [37 to 45] (42) | 20 [14 to 25] (17) |

What the rerun supports, and what it does not. The stage costs reproduce the first run within the spread (core inspection 47 us against 46, 331 against 328, 2,853 against 2,735), as do peak memory (about 9.7 / 10.5 / 11.9 MiB) and the refusal pattern (refusals fall as `inspection` rises). That is evidence that these figures are not very sensitive to moderate load (5 to 7 on 10 cores) on this machine, and nothing more: the host was never quiet, a single machine and one load generator are not a population, and the Linux dataset was not repeated. The provisional notes below are therefore **confirmed as measurements, still provisional as defaults**.

Connection cost and reuse (ADR 0024, same host, same load class; [connection-reuse-measurement-2026-10-02.json](connection-reuse-measurement-2026-10-02.json)). Median of five runs [range], per-run p50:

| Measurement | Value |
| --- | --- |
| Local leg, 4 KiB request, 1 client, one connection per request / connection reused (in-process surrogate on the same stack) | 55.4 us [55.0 to 56.0] / 18.8 us [16.8 to 21.5] |
| Same, 8 clients | 145 us [139 to 153] / 56.2 us [52.6 to 62.1] |
| Same, 384 KiB request, 1 and 8 clients | no distinguishable difference |
| Gateway binary, `GET /healthz`, one connection per request, 1 / 8 clients | 49.8 us [48.5 to 50.2] / 155 us [146 to 157] |
| Provider leg, loopback TCP connect plus one exchange / reused TCP | 57.5 us / 17.7 us |
| Provider leg, TCP plus full TLS 1.3 handshake plus one exchange / reused TLS | 256 us [246 to 257] / 24.6 us [17.0 to 27.6] |
| Resumed TLS 1.3 / full TLS 1.2 | 187 us / 239 us |
| Per-connection memory (idle / 60 KiB unfinished head), descriptors | 5.1 to 6.4 KiB / 70.9 to 73.0 KiB, 1 each (ADR 0022: 5 to 7 / about 71) |

### Measured choices

Nothing measured here is a default, and no choice below is promoted by this report. Each remains **provisional**, with what was and was not measured:

| Choice | Status after this run |
| --- | --- |
| **Inspection workers and queue (`resources.capacity.inspection`)** | Capacity is both the number of worker threads and the bound on queued plus running jobs. With 8 simultaneous clients on this host, capacity 1 refused 60 of 200 small requests, 2 refused 31 of 200, and 4 refused 4 of 200 (and 17 of 80 for the 16 KiB many-findings payload at 4). The example config's `2` therefore suits an application with about two requests in flight; an application that issues more in parallel needs a larger number or must expect `503 overload` (which SDKs retry after `Retry-After: 1`). The rerun (above, five runs, not-quiet host) reproduced the pattern (71, 20, and 1 refused of 200 at capacity 1, 2, and 4; range over runs 51 to 73, 16 to 33, 1 to 4). The right number still needs a truly quiet host and the application's real concurrency: **provisional, unchanged** |
| **Aggregate memory budget (`memory_units`)** | `65536` (64 MiB) was never exhausted in any run (peak process memory about 12 MiB even with a 384 KiB request; the reservation formula is conservative by design). The rerun again peaked at about 12 MiB (sampled maximum 12.2 MiB with a 384 KiB request). No tighter number is justified without a truly quiet host: **provisional, unchanged** |
| **Upstream and stream capacity, deadlines, response bounds** | Never limiting in these runs except the `stream` and `upstream` counts under deliberate overload in the SDK suites (one permit each); the numeric defaults are the documented ceilings chosen to be safe, not tuned values: **provisional** |
| **Parser** | The single strict, budgeted, duplicate-key-rejecting parser measured 2 to 10 microseconds for 4 KiB and 38 to 42 microseconds for 384 KiB. No alternative parser was benchmarked, so no parser-superiority, zero-copy, or fast-path claim is made. A different parser must pass the shared conformance table (`tests/parser_conformance.rs`): **unchanged, no claim** |
| **Provider connection pooling and local keep-alive** | **Decided in ADR 0024 (#42): both stay disabled.** Measured on a not-quiet host: a fresh TCP plus TLS 1.3 connection to a loopback TLS fake costs about 230 us more than a reused one in CPU of both ends (256 us against 24.6 us), plus about 40 us for TCP; on a real path it also costs about two round trips, which were not measured. Locally a connection costs about 37 us (1 client) to 90 us (8 clients) for a tiny request and nothing distinguishable for a 384 KiB body. Reuse would need a second framing implementation in front of the HTTP server (local) and a bounded connection age, an address re-check, and a proven no-replay path (provider); the measured benefit does not justify that. Figures provisional: not-quiet host |
| **Gateway overhead versus provider delay** | Separated by construction (gateway stages versus upstream stages, and for streams upstream wait versus downstream wait). The provider here replies immediately or at a scripted pace; real model latency is unmeasured |

## Dependency and supply-chain checks

| Check | Result |
| --- | --- |
| `cargo deny check` (advisories, bans, licenses, sources) | Passed locally and in CI; no new Rust dependency was added (the qualification crate's dependency block is copied from the shipped one and its lock is verified to be a subset of `Cargo.lock`) |
| `uvx zizmor .github/workflows/` (offline mode; three workflows) | No findings. Run locally; not a CI step |
| `osv-scanner` over `Cargo.lock`, both `package-lock.json` files, both `requirements.txt` files | No issues found (205 Rust, 4 npm, and 15 Python packages). Run locally on the date of this change; not a CI step |
| `npm audit signatures` (registry signatures and attestations) and `npm audit` | Verified signatures for all 4 packages in each lockfile and 0 vulnerabilities; the signatures check also runs in the qualification workflow |
| `pip-audit --require-hashes` over the Python requirements | No known vulnerabilities. Run locally with `uvx pip-audit`; not a CI step |
| Lockfile integrity | `npm ci --ignore-scripts` and `pip install --require-hashes --no-deps` in CI; `tests/shipped_examples.rs` checks that every locked npm package has a sha512 integrity and registry URL, and every Python line is an exact pin followed by sha256 hashes |
| Dependabot | npm and pip entries added for the SDK harness and examples (`openai` ignored on purpose: its version is an explicit qualification decision); docker and cargo entries unchanged |

`osv-scanner`, `pip-audit`, and `zizmor` online mode are not run in CI by this change; their results here are local observations and must be repeated at release time.

## Unresolved blockers and open items

| Item | Owner | Why it matters |
| --- | --- | --- |
| **Registry and image name** are not selected; nothing is pushed | Maintainer | No distribution possible |
| **Publication is not authorized**; no signing, SBOM, or provenance exists (Beta 3) | Maintainer | `distributable: false` |
| **Quiet-host measurement.** The figures were taken on a host that was not quiet (see the host line), and a rerun for #42 (ADR 0024) on 2026-10-02 was also not quiet (one-minute load 5.7 to 7.6 on 10 cores, system services); it reproduced the first run within the spread. The numeric limits and capacity remain provisional (ADR 0008). Worker count, inspection queue, `max_findings`, the memory budget, and `max_connections` were not changed | Maintainer (host) / Alpha 2 | No limit becomes a default without a recorded quiet-host measurement (one-minute load at or below about 25% of the core count) |
| Open gateway follow-ups from #25: **#40** bound concurrent and per-peer connections before the HTTP layer; **#41** measure the header caps and the 64 KiB hold bound; **#42** revisit local keep-alive and provider pooling after measurement (done: ADR 0024, both stay disabled); **#43** parser-level rejection of `Content-Length` plus `Transfer-Encoding` with a gateway-written `400`; **#44** deployment-chain evidence for egress and intermediary controls | Maintainer | Residual risks of Alpha 1; none blocks the SDK qualification |
| Core issues: **redact-secret/redact-secret #1177** (document and freeze cancellation and time-bound behavior for whole-input calls), **#1178** (a `Send + Sync` inspection handle for built-in-only registries), **#1179** (state that `Ok` means complete inspection, no partial success, and freeze it), **#1180** (optional multi-leaf scan with request-scoped placeholder numbering) | Core maintainers | The gateway pins core by exact version and does not rely on unfrozen behavior it cannot test; these would remove workarounds (worker pool, per-worker registries) |
| New findings from this change: on Node.js 22.16.0 a truncated stream is not reported as an error through the gateway (Node 24 raises; documented; a follow-up decision on abortive close); the gateway drops `x-should-retry` and `retry-after-ms` (documented; a follow-up decision on relaying them) | Maintainer | Behavior is documented, not changed, here |
| Epic #1 sub-issue #3 is unchecked on the epic | Maintainer | Bookkeeping |

## Residual risks

- Unbounded connection count until the HTTP layer sees a request (#40); header and hold caps are conservative and unmeasured (#41); every request pays connection setup (#42); the head guard is a structural scan, not a general parser (#43); egress, resolver, trust store, and intermediaries are environment controls (#44).
- SDK retries can send a failed request to the provider up to three times; the examples disable retries, applications may not.
- A Node.js caller that ignores `finish_reason` can treat a truncated stream as complete.
- Provider responses are unredacted by design; the gateway cannot recall bytes already sent.
- The fake-provider qualification does not exercise the real provider's behavior, headers (including whether it sends retry hints), TLS, or rate limits.
- Plaintext exists in process memory and TLS buffers; no secure erasure is promised (ADR 0007).
- Numbers are provisional: the limits were never tuned, and the performance run was not on a quiet host.

## Reproduce

```bash
cargo test --locked && cargo clippy --locked --all-targets -- -D warnings && cargo deny check
sh qualification/build.sh                    # the separate non-release crate; add --release for measurements
sh qualification/run-suites.sh               # Node suite, Python suite, README examples, log scan
sh qualification/build.sh --release && QUAL_COMMAND='node qualification/perf/run.mjs' \
  sh qualification/run-suites.sh --binary qualification/target/release/redact-secret-gateway-qualification --suites none
```

## Evidence index

| What | Link |
| --- | --- |
| Pull request | https://github.com/redact-secret/gateway/pull/46 |
| Ordinary CI (`CI passed`: format, lint, test, build and startup, dependency policy) | https://github.com/redact-secret/gateway/actions/runs/37044335215 |
| Qualification workflow (`Qualification passed`: pinned SDK suites, README examples, synthetic measurement; evidence artifacts `sdk-qualification-evidence-*` and `perf-evidence-*`) | https://github.com/redact-secret/gateway/actions/runs/37044335074 |
| Candidate artifacts workflow (Linux x86_64, macOS ARM64, image, Compose, manifest; bundle `alpha1-candidate-*`) | https://github.com/redact-secret/gateway/actions/runs/37044335173 |
| SDK suites run repeatedly for determinism | Four consecutive full local runs (Node 22.16.0 and Python 3.14.7: 70 and 33 tests, examples, log scan) and two earlier full runs, all passing; the two CI runs above on Node 24.21.0 and Python 3.13.15 |
| First CI run (kept as an honest record) | https://github.com/redact-secret/gateway/actions/runs/37043763494 failed one Node test, a control that asserted the Node 22 truncation behavior; Node 24 behaves differently. The assertion was removed and the docs corrected; see "SDK results" |
| Tests named in this report | `cargo test --locked` (`tests/destination_policy.rs`, `tests/shipped_examples.rs`, `tests/architecture_reconciliation.rs`, `tests/dependency_policy.rs`, `tests/api_boundary.rs`, the permit, parser, diagnostic, and in-crate transport suites) and the SDK suites under `qualification/sdk/` |
| Decision | [ADR 0020](../decisions/0020-sdk-qualification-test-build.md): the separate non-release build; allowlist of exactly eight files in `tests/destination_policy.rs`; no existing scan was relaxed |
| Raw measurement data | [alpha1-stage-timing.json](alpha1-stage-timing.json) (local, macOS arm64), [alpha1-stage-timing-quiet-host-rerun-2026-10-02.json](alpha1-stage-timing-quiet-host-rerun-2026-10-02.json) and [connection-reuse-measurement-2026-10-02.json](connection-reuse-measurement-2026-10-02.json) (#42, not-quiet host), [alpha1-stage-timing-ci-linux.json](alpha1-stage-timing-ci-linux.json) (GitHub-hosted Linux) |
