# ADR 0028: Aggregate load qualification: measurement aids, findings, and no change to defaults

Status: Accepted; measured, decision recorded (#58). No default numeric limit, no reservation formula, and no scheduling behavior changed. Follow-up of ADR 0003 (admission), ADR 0004 (non-interruptible inspection), ADR 0008 (measurement gate), ADR 0014 (reservation formula), ADR 0022 (connection bound), ADR 0024 (connection reuse) and ADR 0027 (write budget). Owned by the transport hardening epic (#10). Date: 2026-10-03.

## Context

Alpha 2 added request fields whose cost differs from the Alpha 1 text shape: tool history with JSON-string arguments, tool definitions and schemas, and bounded metadata (#53, #54, #55). #58 asks whether the admission design stays bounded and recovers when large and many-finding bodies, queued inspections, concurrent provider calls, slow stream readers and a finite socket budget occur together, how reserved memory relates to observed bytes, and what capacities to recommend.

## Decision

1. **Extend the existing harness; add no second one.** `qualification/perf/run.mjs` gains `--aggregate`; `tests/support/workloads.rs` and `examples/perf_workloads.rs` gain the Alpha 2 shapes and a retention-based memory accounting mode (`--memory`). No file was added to the qualification seam allowlist and `tests/destination_policy.rs` is unchanged. The fake provider gained three scenarios and a no-body-retention mode (stats only), in an existing file.
2. **Two measurement aids in the shipped crate, counts only.** `Admission::load()` returns the number of permits and memory units held per class and the wait-queue length (relaxed reads, no request data). `Metrics::histogram()` exposes a fixed-size, log-bucketed distribution per stage (4 buckets per power of two, 252 buckets, at most 25% wide; a percentile read from it is the bucket's upper edge). Both are read only by the qualification build's loopback metrics listener and by tests. Cost: one relaxed atomic increment per recorded stage and about 20 KiB of counters.
3. **No default or formula changes.** The criteria that would justify a change are written here, per ADR 0008: a default or the reservation formula changes only when, on a host whose one-minute load average is at most 25% of its cores for the whole pass, at least five runs agree (the whole spread, not the median) that (a) observed resident growth exceeds 90% of the bytes reserved for a request shape (formula unsafe), or (b) a class reaches its capacity while the others stay below 50% under the combined load and requests are refused that the host could have served (capacity too tight), or (c) a stage exceeds a user-visible deadline. No pass in this task met the quiet-host condition, so no change was eligible.
4. **No scheduling change.** Large and small requests were both fully served under a tight memory budget (no starvation), queues stayed at their bounds, and no detached or unbounded job exists. The inspection class keeps its immediate-refusal behavior (no waiting for a permit).
5. **Capacities are published as recommended, provisional values** in the resource-limits contract, with the failure behavior at each limit. They are guidance, not defaults.

## Consequences

- The reservation formula `4*B + min(max_nodes,B)*128` held for every Alpha 2 shape measured: the worst resident growth was 0.68 of the reserved bytes for eight held 825 KB tool-definition bodies (spread 0.61 to 0.72 over five runs), and at most 0.60 of the sampled reservation peak in the combined-load passes (growth over a warm baseline, below). The headroom is 1.4x to 9x per shape, narrower than the constants suggest because allocator slack and connection buffers are in the observed figure. The factor and the per-node constant stay.
- There is a fixed cost outside `memory_units`: the first use of the core on each inspection worker thread raised resident memory by about 7 MiB per worker (cold 8 MiB, warm 36 MiB with four workers, before any measured load). Measured from a cold start, combined-load growth looked like 2x the reservation; measured from the warm baseline it is 0.31 to 0.60. Capacity planning adds about 7 MiB per inspection worker.
- Provider response buffering is **outside** `memory_units` (ADR 0017) and is the largest unreserved term: 16 held replies of about 4 MB cost about 4.4 to 4.6 MiB each. Capacity planning must add `upstream x max_response_body_bytes` to `memory_units`.
- Parse runs on the single request thread; for 660 to 825 KB Alpha 2 bodies it takes about 1.3 to 1.6 ms (p99 up to 8 ms on a loaded host). The trigger written in the resource-limits contract for moving parsing to the pool (a measured stall that matters) was not met on quiet-host terms; it stays open.

## Invariants

1. Measurement output carries counts, sizes and timings, never payloads, headers, credentials or finding text.
2. Provider-side counters (not gateway logs) prove that every pre-forward rejection delivered zero body and that no body with a synthetic credential shape reached the provider.
3. All figures are provisional until a pass meets the quiet-host condition. Nothing is labelled quiet that was not.

## Failure behavior

If the measurements cannot be produced, the documented bounds stay as they are (ADR 0008).

## Verification

`cargo test --locked` (`tests/inspection_waiter_timeout.rs`, `tests/perf_scaffold.rs`, `src/admission.rs` and `src/telemetry.rs` unit tests), and the commands in the Alpha 2 aggregate load qualification subsection of the qualification report.

## Deferred measured choices

Final capacities, whether parsing moves to the pool, and any default change: a quiet host, a Linux repeat, and the application's real concurrency.

## Implementation status

Implemented (#58).
