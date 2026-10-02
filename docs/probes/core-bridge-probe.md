# Core bridge probe: complete-inspection semantics and CPU scheduling

Issue: #5 (epic #1). ADR gate: #3 (ADR 0004, ADR 0008). Contract: [core-completeness](../contracts/core-completeness.md).

## Subject and method

| Item | Value |
| --- | --- |
| Core | `redact-secret =0.1.0-beta.12` (crates.io sha256 `2cc951e8b991e9ec27343a872627f53a25b252cc8148cb4ec7160192ed9d856d`, tag `v0.1.0-beta.12`, commit `4227160c4dac402d7add53d3f8fe990f693912c1`, as recorded in ADR 0011). Verified against the unpacked crate source, not only its docs. |
| Profiles | `full` (110 built-in detectors) and `common` (6). `full` plus PII selector `pii:family:global:email` and `pii` for setup cost. |
| Core APIs used | Public only: `DetectorRegistry::with_built_in_and_pii` / `with_common_built_in_and_pii`, `PiiSelection::parse`, `Profile::from_name`, `WholeInputLimits::new`, `scan_and_redact_with_limits`, `DefaultPolicy`, a custom `PlaceholderFormatter`. Custom `Detector` / `Policy` / formatter implementations are used only inside tests to trigger failure paths. |
| Input | Synthetic only: `ghp_SYNTHETICREVOKED` + a 20-digit counter (the core's own documentation fixture shape), invented PII on a non-reserved invented domain, deterministic generated prose. No real credential exists anywhere in the probe. |
| Evidence | `tests/core_probe_semantics.rs` (27 tests), `tests/core_probe_scheduling.rs` (11 tests), `examples/core_probe_bench.rs`. |
| Bridge code | `src/core_bridge.rs` (`InspectorSpec`, `Inspector`, `RequestScope`, `CompleteInspection`, `map_core_error`) and `src/core_bridge/pool.rs` (offload candidate). |

## Verified completeness semantics

1. **`Result` is the only completeness signal, and it is sufficient.** `scan_and_redact_with_limits` returns `Ok` only when every registered detector inspected the whole input, every candidate validated, and the policy and formatter succeeded. Every other outcome is an `Err` with a fixed code that carries no input, matched value, or placeholder. There is no partial-result type, no `truncated` flag, and no per-detector status in the whole-input API. The probe proves this at the boundary: input exactly at `max_input_bytes` is fully inspected, one byte over is `InputLimitExceeded`; two findings against `max_findings = 1` is `FindingLimitExceeded`, not one redaction; the finding limit discards the redacted text, so the gateway can only block, not redact.
2. **Reachable outcomes and the gateway mapping.** Mapping lives in `core_bridge::map_core_error`. Anything not named is `Incomplete`, so an unknown future code can never read as success.

   | Core outcome (code) | Triggered by | Gateway error | Safe code (provisional) |
   | --- | --- | --- | --- |
   | `Ok` | whole input inspected | proof minted via `RequestScope::finish` | none |
   | `InputLimitExceeded`, `FindingLimitExceeded` (also incremental-only `BufferLimitExceeded`, `TokenLimitExceeded`, `MultilineLimitExceeded`) | bytes or findings over the bound | `LimitExceeded` | `limit_exceeded` |
   | `InvalidLimits`, `InvalidOptions` | zero limits | `InvalidConfiguration` (startup) | `unsupported_input` |
   | `PiiSelectorInvalid`, `PiiSelectorUnsupported` (`pii:kr`), `PiiSelectorUnavailable`, `PiiActivationConflict` | bad selector; unknown profile name (`Profile::from_name` returns `None`) | `UnsupportedProfile` (startup) | `unsupported_input` |
   | `DetectorFailure` | custom failing detector | `Incomplete` | `incomplete_inspection` |
   | `InvalidCandidate` | custom detector returning an out-of-range candidate | `Incomplete` | `incomplete_inspection` |
   | `PolicyFailure` | failing policy | `Incomplete` | `incomplete_inspection` |
   | `PlaceholderFailure`, `InvalidPlaceholder` (empty, over `MAX_PLACEHOLDER_LENGTH`, or reproducing a matched value) | failing/hostile formatter | `Incomplete` | `incomplete_inspection` |
   | `InvalidFindings` | hand-built findings outside the input (`redact` only) | `Incomplete` | `incomplete_inspection` |
   | policy action `Block` (private key) | `DefaultPolicy` | `Blocked` | `unsupported_input` (provisional; #4/#18 may add a code) |

   The built-in detectors never produce `DetectorFailure`, `PolicyFailure`, or `PlaceholderFailure` with `DefaultPolicy` and the bridge formatter; those paths exist only through custom implementations, which the gateway does not use. They are tested anyway so the mapping is proven rather than assumed.
3. **Nothing partial is ever minted.** `RequestScope` is poisoned by any failing leaf (including limit and `Blocked`). `RequestScope::finish` returns `Incomplete` once poisoned, so `CompleteInspection` can be minted only after every leaf returned `Ok`. A poisoned scope also refuses further leaves. Limits are request-wide: the core applies its limits per call, so the scope sums bytes and findings across leaves (each leaf can fit while the request does not; tested).
4. **`Ok` is not "nothing sensitive remains".** `DefaultPolicy` leaves medium-confidence findings (`Warn`) in the text, for example `password=hunter2xyz` is a `contextual_secret` `Warn` and comes back unchanged. The bridge counts these in `InspectionSummary::unredacted_findings`. Whether a `Warn` finding must reject a request is a policy decision for #19 (open item below); the core-completeness contract is about inspection, which is complete.
5. **No cancellation, no deadline, no work budget.** No public function takes a token, deadline, or callback, and the crate contains no cancel/deadline/timeout identifier outside a detector keyword list. Calls are synchronous and uninterruptible. `Policy::compile()` does not exist.
6. **Thread safety.** `DetectorRegistry` is `!Send + !Sync` (a compile-time tripwire in `spec_is_shareable_and_the_registry_owner_is_not` fails the build if a core upgrade changes that). `InspectorSpec` (profile, PII selection, limits) is `Send + Sync`. Each owning thread builds its own `Inspector` from the spec. A request needs no registry sharing, and no global lock exists.
7. **Profile fact.** The `common` profile has no GitHub token detector: a synthetic `ghp_...` token passes `common` unchanged and is redacted by `full`. This is not an incomplete inspection (every detector of the chosen profile ran) but #4 must not present `common` as a lighter `full`. Reserved example domains (`example.com`, `.invalid`) are not reported as email PII; probe PII uses an invented non-reserved domain. PII email also needs an `email:`-style context word.

## State scope, traversal, Unicode, pre-redacted input

| Topic | Verified behavior | Gateway rule (implemented in the bridge, tested) |
| --- | --- | --- |
| Placeholder numbering | The core restarts at `<SECRET_1>` on every call. Two leaves holding different secrets both become `<SECRET_1>`. | `RequestScope` offsets numbering by replacements already made in the request (custom formatter). A second request has its own scope and restarts at 1. Numbering is request-wide, never process-wide. |
| Registry state | `DetectorRegistry` is immutable during a call. No per-request state survives a call. | Immutable shareable `InspectorSpec` at startup; mutable per-request state only in `RequestScope` (owned, not `Clone`). |
| Traversal | The core scans one `&str`. It has no notion of JSON structure or keys (core's key-aware scanning is an adapter convention, not a Rust API). | Reference traversal for #18/#19, tested for determinism and independence from source key order: depth-first; arrays in index order; objects in the key order of the gateway's JSON model (`serde_json` default map, lexicographic by key bytes; no `preserve_order`); string values only; keys are structural and are not inspected here (open item). |
| JSON-decoded text | A JSON `\u` escape inside a token in the raw serialized JSON is **not** detected (the probe fixture `SYNTHETIC` stays unredacted); the decoded leaf is detected. | Always decode once, then inspect the decoded string; never scan or replace raw serialized JSON (field-classification contract). The core catches some escape shapes in raw text, so raw scanning is also not a safe shortcut: its coverage is partial and undocumented. |
| Invisible code points | A zero-width space inside a token cannot split it; the core scans a copy with invisible code points removed and maps ranges back, preserving surrounding text. | Nothing to do; covered by a test. |
| Unicode text | NFC and NFD Hangul, emoji, combining marks without findings come back byte-for-byte. Ranges are UTF-8 byte offsets into the original input. | Callers pass `&str` (valid UTF-8) only; `RANGE_UNIT` is `utf8-bytes`. |
| Pre-redacted input | `<SECRET_1>` is not a finding. Re-inspecting redacted output yields no findings and identical text (idempotent). | A client-supplied `<SECRET_1>` is ordinary text. A real token beside it is still redacted, and the literal and the generated placeholder can be equal strings, so placeholder numbering is not a unique mapping back to values (security unaffected; nothing is trusted from it). |
| "Already scanned" claims | Not a core concept. | No code reads a client claim; a test scans `src/` for `scanned`, `x-sanitized` and similar and fails if any code mentions them. Request-state contract rule 6 stands. |

## CPU scheduling and cancellation probe (ADR 0004 gate)

### Capability facts

- Initialization: stateless free functions plus an explicit `DetectorRegistry`. Building one has a one-time process cost (shared prefilter in a `OnceLock`) and then a small steady cost (below). There is no global init and no compiled-policy object.
- Completion: the `Result` of the call (above). Cancellation: none. An in-flight call can only be waited for.
- Panics in core code are not expected (`clippy::panic` denied in core), but custom callbacks could unwind through it. The pool catches a panicking job (`catch_unwind`), returns its capacity, keeps the worker alive, and the awaiter sees `Incomplete`.

### Offload candidate implemented for the probe (`core_bridge::pool::InspectionPool`)

Dedicated worker threads, each building its own `Inspector` at start, fed by a bounded `sync_channel`. A job owns its `InspectionPermit` and `MemoryReservation` (from `admission`), never the awaiting future. `submit_with` / `submit_inspect` return a `JobHandle` (`Future`). Worker count and queue capacity are required parameters with no default. The queue receiver sits behind a `Mutex` held only to dequeue, never around a core call. `spawn_blocking` was not implemented or measured: it would need tokio's `rt` feature in the normal dependency set (currently `sync` only), and each blocking thread would need a thread-local registry cache; the dedicated-thread design already shows the semantics. It stays a candidate for #18/#19 if the maintainer prefers it.

### Proven by tests (`tests/core_probe_scheduling.rs`, synthetic gated job, repeated 12 times without failure)

- Dropping the awaiter of a started, uninterruptible job does **not** return the `InspectionPermit` or `MemoryReservation`; both return only when the job really completes (capacity of 1 inspection permit and 10 memory units, asserted `Overload` while the job runs).
- A cancelled job's result is discarded: the produced value is dropped by the worker and never delivered to any awaiter.
- A job cancelled while queued never runs. Its permits return when a worker next dequeues it, not at the instant of cancellation (the queue is bounded, so this delay is bounded).
- 200 immediate cancel-and-retry attempts against 2 inspection permits and 20 memory units: exactly 2 are accepted, 198 are refused by admission, the observed high-water mark of concurrently running core jobs is 2, and no reservation beyond the budget is ever granted. Everything returns only after the jobs are released.
- A full queue refuses with `Overload` and drops the never-started job's permits immediately.
- A panicking job returns its capacity and the same worker serves the next job.
- Shutdown (`InspectionPool::shutdown(deadline)`): stops accepting work; queued jobs are rejected (awaiter sees `Incomplete`, job never runs); started jobs are waited for up to the deadline; workers still inside a core call at the deadline are reported as `abandoned_workers` and keep running holding their capacity (the core cannot be interrupted, so the process exit abandons them, documented not hidden). An idle pool drains immediately.
- Async: a `tokio::time::timeout` around the handle returns while the job still runs, and capacity is still held afterwards.
- N workers released together all run concurrently: no shared registry or lock serializes inspection.

### Measurements (`cargo run --locked --release --example core_probe_bench`)

Platform: Apple M4, 10 logical CPUs, macOS 26.5.2 (Darwin 25.5.0, arm64), rustc 1.98.1, release profile, core `=0.1.0-beta.12`, gateway commit of this PR, profile `full` unless stated. Timing is `std::time::Instant` per call. Inputs are generated synthetic prose from a 32-word vocabulary (no credential-shaped text) plus, for many-findings cases, one distinct synthetic token about every 85 bytes (the input prose is not representative of real prompts). Percentiles are nearest-rank over the listed sample count.

**Measurement quality warning.** The machine was shared with other builds and tests during the probe (load average between 25 and 54 on 10 CPUs, measured with `uptime` before and after each run). Absolute values are pessimistic and noisy, and p99 and max in particular are scheduling noise. Two full runs are kept: run 1 at load about 34 to 54, run 2 at load about 25 to 27. Run 2 (load about 25 to 27) is shown below; run 1 had the same ordering but slower and wider values (for example no findings 64 KiB p50 2.9 ms, p95 29 ms; 1 MiB p50 59 ms, p95 151 ms; 8 MiB p50 801 ms). No decision should rest on these numbers; they should be re-taken on a quiet, representative host before any default is set (ADR 0008).

Core setup (registry construction, one thread):

| case | n | p50 us | p95 us | p99 us | max us |
| --- | ---: | ---: | ---: | ---: | ---: |
| `full` first build in process (one-time prefilter included) | 1 | 340.6 | - | - | - |
| `full` build, steady | 2000 | 14.6 | 40.8 | 167.8 | 2724.6 |
| `common` first build in process | 1 | 33.0 | - | - | - |
| `common` build, steady | 2000 | 0.7 | 0.8 | 1.3 | 81.8 |
| `full` + `pii` build, steady | 2000 | 14.5 | 16.0 | 34.8 | 84.1 |

Steady-state inspection, one thread, reused inspector (`full`), per call:

| case | n | p50 us | p95 us | p99 us | max us |
| --- | ---: | ---: | ---: | ---: | ---: |
| no findings 1 KiB | 2000 | 18.2 | 51.1 | 185.8 | 1878.0 |
| no findings 64 KiB | 300 | 1692.3 | 3713.9 | 6992.9 | 8966.4 |
| no findings 1 MiB | 40 | 29045.8 | 48282.8 | 52472.2 | 52472.2 |
| no findings 8 MiB | 10 | 311501.7 | 451035.5 | 451035.5 | 451035.5 |
| many findings 64 KiB (754 findings) | 100 | 2873.6 | 6594.9 | 11145.4 | 14087.2 |
| many findings 1 MiB (12053 findings) | 10 | 51152.1 | 90113.4 | 90113.4 | 90113.4 |

Inline on the reactor thread versus the dedicated pool. Current-thread tokio runtime, closed loop of 8 concurrent request tasks, plus a 1 ms ticker. "Tick lateness" is how long past 1 ms the ticker woke: the stall imposed on everything else on the reactor. Tokio's timer has about 1 ms granularity, so a lateness floor near 1000 us is the idle baseline, not stall.

| input | mode | req/s | latency p50 us | p95 us | p99 us | tick lateness p50 us | p99 us | max us |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 KiB no findings | inline | 33063 | 186 | 573 | 2283 | 1008 | 3046 | 3046 |
| 1 KiB no findings | pool(1) | 27091 | 213 | 671 | 1604 | 1001 | 4425 | 4425 |
| 1 KiB no findings | pool(4) | 43679 | 113 | 488 | 1373 | 1012 | 5067 | 5067 |
| 64 KiB no findings | inline | 304 | 18695 | 83113 | 162032 | 16617 | 113867 | 113867 |
| 64 KiB no findings | pool(1) | 381 | 17470 | 39371 | 74458 | 1319 | 11393 | 30253 |
| 64 KiB no findings | pool(4) | 1151 | 5333 | 15791 | 24074 | 1172 | 7451 | 8197 |
| 64 KiB no findings | pool(10) | 2030 | 2263 | 16896 | 21229 | 1097 | 15904 | 15904 |
| 1 MiB no findings | inline | 16 | 348756 | 1389965 | 1492661 | 372738 | 1149031 | 1149031 |
| 1 MiB no findings | pool(1) | 22 | 286577 | 741494 | 746123 | 1531 | 14070 | 85604 |
| 1 MiB no findings | pool(4) | 83 | 86488 | 126937 | 179398 | 1672 | 15956 | 28962 |
| 1 MiB no findings | pool(10) | 128 | 44368 | 133996 | 161541 | 1520 | 8097 | 16628 |
| 64 KiB many findings | inline | 243 | 26958 | 88731 | 126775 | 21934 | 73474 | 73474 |
| 64 KiB many findings | pool(1) | 278 | 26382 | 45962 | 55473 | 1480 | 7493 | 9102 |
| 64 KiB many findings | pool(4) | 927 | 6347 | 22362 | 35158 | 1229 | 15756 | 15756 |

(The full table, including pool(2) and pool(10) for every case, is the output of the command above.)

### What the data supports, and what it does not

Supported by the data, despite the noise:

- Reactor responsiveness: inline inspection of 64 KiB or larger inputs stalls the reactor thread for the length of the call (tick lateness p50 17 to 373 ms at 64 KiB to 1 MiB, max over 1 s at 1 MiB), while offload keeps ticker lateness p99 at or below about 16 ms (worst single max 86 ms). At 1 KiB the call is about 20 us and no stall is visible in either mode.
- Core setup is small next to per-request work: about 15 us per `full` registry in steady state against about 2 ms for 64 KiB. Per-worker registries cost nothing meaningful. The one-time first build (0.3 to 0.55 ms) happens once per process.
- Inspection cost scales roughly linearly with input size at this host and load, with many-findings inputs costing more per byte at the same size class for the larger case.

Not supported, so not claimed:

- Any performance-superiority, throughput, or service-level claim. The 1 KiB inline versus pool differences are inside the noise; per-job offload overhead could not be separated from load and needs a quiet re-run.
- Any worker count, queue size, shutdown deadline, or size threshold. The data shows more workers lowering latency until the host saturates, as expected, and says nothing about the right number for a production host.

### Recommendation (not a decision; the maintainer approves, ADR 0004/0008)

1. Do not run core inspection on the reactor thread for any input beyond a small size, because the call is uninterruptible and its cost grows with input size. Prefer one code path: bounded offload to dedicated worker threads that own their registries, with permits owned by the job (the semantics above are tested). An inline fast path for tiny inputs is an optimization that needs a quiet-host measurement of offload overhead first, and a second path with the same cancellation guarantees.
2. Pick worker count, queue capacity, accepted body size, and shutdown deadline from a quiet-host re-run of this harness on the target hardware and the real accepted-size distribution; keep every one an explicit configuration value with no default until then.
3. Keep the per-request byte and finding bounds far below the core's 64 MiB / 50,000 defaults: 8 MiB took about 0.3 s per request here, and the core offers no time bound.
4. #6 should reuse the controllable gated-job approach for its fake-upstream no-forward tests.

## Blockers and open items

No completeness blocker: `Ok`/`Err` is a sound fail-closed signal for the pinned core. These remain, none silently treated as satisfied:

| Item | Status | Owner |
| --- | --- | --- |
| No cooperative cancellation, deadline, or work budget in core | Explicit limitation. The gateway holds capacity until real completion and bounds input size. Candidate core request below. | core issue (draft 1) |
| `DetectorRegistry` is `!Send + !Sync` | Worked around with a registry per owner thread. Not blocking. | core issue (draft 2) |
| `Ok` is implicit, "no partial success" not frozen as contract | Verified for beta.12 by reading and testing; not guaranteed to stay. | core issue (draft 3) |
| No request-scoped placeholder numbering / multi-leaf scan | Solved in the gateway (`RequestScope`). | core issue (draft 4, optional) |
| `Warn` findings remain in output under `DefaultPolicy` | Decided in #19: reject by default; forward only with `content.on_warn = "forward"` (deliberate operator choice). See [ADR 0015](../decisions/0015-core-inspection-and-request-transformation.md). | resolved (#19) |
| Object-key text inspection | Decided in #19: keys are fixed schema names in the supported subset, and every free-form-keyed object is rejected by the matrix, so no key text is inspected or needed. | resolved (#19) |
| Provisional safe code for `Blocked` | Kept as `unsupported_input` (`422`), now also used for rejected `Warn`. | resolved (#19) |
| Quiet-host measurement and `spawn_blocking` comparison | Not done on this loaded machine. | follow-up under #5 / #18 |

### Draft core-side issues (not filed)

1. **Contract: document and freeze cancellation/time-bound behavior for whole-input calls.** `scan_and_redact*` is synchronous and uninterruptible with no deadline or work budget; the only bounds are `max_input_bytes` and `max_findings`. Request either a documented per-byte worst-case cost bound with a published adversarial runtime cap (currently test-only), or an optional cooperative cancel/deadline check, and state in the stable contract that none exists otherwise. Context: gateway needs capacity to stay held until real completion. Relates to core #1001 and #1066.
2. **API: thread-shareable inspection handle.** `DetectorRegistry` is `!Send + !Sync` only because `Detector` has no `Send + Sync` supertrait; a built-in-only registry holds no custom detector. Request a `Send + Sync` handle for built-in-only construction (or an opt-in bound). Gateway currently builds one registry per worker thread (about 15 us each), which works, so this is a request, not a blocker. Relates to core #1097 reopen trigger and #1066.
3. **Contract: state "`Ok` means complete, no partial success" as a frozen guarantee.** Whole-input `Ok` currently means every detector ran over the full normalized input, and every limit, detector, policy, and placeholder failure is an `Err`. Ask core to state this once in the public contract and freeze it in beta.13 so a future detector time budget cannot produce a partial `Ok`. Relates to core #1066 and #1065.
4. **Feature: multi-leaf scan with request-scoped placeholder numbering.** Each call restarts at `<SECRET_1>`, so scanning N JSON leaves needs an offsetting formatter in the host. Optional helper (or documented formatter recipe) for request-wide numbering and key context. Gateway implements the offset itself, so this is optional.

## Wired evidence (#19)

The probe's bridge is now on the Chat Completions route (`boundary::Inspection`, `core_bridge::pool`). Executed on the pinned core (`redact-secret =0.1.0-beta.12`, `Cargo.lock` source `registry+https://github.com/rust-lang/crates.io-index`, checked by the existing `PINNED_CORE_VERSION` test), `cargo test --locked` on Apple silicon, macOS, rustc 1.98.1:

| Claim | Test (all passing; the suites below were also run 12 times in a row without a failure) |
| --- | --- |
| Synthetic `ghp_SYNTHETICREVOKED...` tokens are redacted under the `full` profile in every supported text field (string and part `content`, `stop` array and string, `user`), numbered 1..7 in traversal order | `tests/inspection_transform.rs` `synthetic_secrets_in_every_supported_text_field_are_redacted_in_order`, `single_string_stop_is_inspected` |
| Escaped (`\u0067\u0068\u0070...`) tokens are decoded before inspection; escapes, quotes, line separators, emoji, and Korean text stay valid JSON and equal after decode | `escaped_input_is_decoded_before_inspection_and_stays_valid_json`, `english_and_korean_text_survive_byte_for_byte_semantically` |
| Keys, value types, array form, and controls are preserved; the body is a fresh document | `keys_types_and_controls_are_preserved`, `serialization_is_a_fresh_document_not_the_original_bytes`, `protocol::chat` `serialization_round_trips_through_the_matrix_and_is_bounded` |
| `Block` rejects; `Warn` rejects by default; `Warn` forwards only with `on_warn = forward`; a finding in `model` rejects | `block_findings_reject_the_request`, `warn_findings_reject_by_default`, `warn_findings_forward_unchanged_only_when_the_operator_chose_forward`, `model_is_never_rewritten_and_any_finding_in_it_rejects`, and the HTTP equivalents in `tests/inspection_route.rs` |
| Finding limit, request-wide input limit, and transformed-output bound reject (no truncation); core `Err` paths leave zero upstream bytes (fake upstream asserted empty after every HTTP case) | `finding_limit_rejects_with_no_partial_result`, `request_wide_input_limit_rejects_even_when_each_text_fits`, `transformed_output_over_its_bound_is_rejected_not_truncated`, `tests/inspection_route.rs` |
| Detector, policy, and placeholder failures map to `incomplete_inspection` (unreachable with built-in detectors, proven through custom implementations) | `tests/core_probe_semantics.rs` (27 tests, unchanged), `core_bridge::map_core_error` |
| Pre-redacted input is ordinary text; numbering is request-local and restarts per request, also under concurrency | `pre_redacted_input_is_ordinary_text_and_numbering_is_request_wide`, `request_state_does_not_leak_across_requests` |
| Dropping the awaiting future does not release the inspection permit or memory; a cancelled queued job never runs; 200 immediate cancel-and-retry attempts against 2 permits return all capacity | `dropping_the_awaiting_future_does_not_release_capacity_early`, `repeated_cancelled_requests_stay_within_capacity_and_all_capacity_returns`, `tests/core_probe_scheduling.rs` `a_job_that_owns_its_memory_keeps_it_after_the_awaiter_is_dropped` |
| Readiness depends on core initialization (an unsupported PII selector fails `Services::init`, no listener is bound) | `readiness_depends_on_successful_core_initialization` |
| Completeness proof and raw bytes cannot reach approval or transport | compile-fail `tests/ui/fail_construct_complete_inspection.rs`, `tests/ui/fail_approve_raw_output.rs` and the existing `SanitizedRequest` cases |

Not measured here: throughput or latency of the wired path (ADR 0008 numbers are still owed from a quiet host), and the pool size and `max_findings` are provisional.
