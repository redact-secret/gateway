# ADR 0003: Resource admission and lifetime

Status: Accepted (design); numeric settings deferred to measurement. Implementation status: five permit owners scaffolded in #2 (`try_*` only, no numbers). Waiting, deadlines, and wiring remain #18; capacity tests #6. Covers issue #3 section B. Categories: [resource-limits contract](../contracts/resource-limits.md).

## Context

A finite per-request limit is not enough. Aggregate memory, CPU, and connection use must be bounded, and capacity must be reserved before allocation or scheduling.

## Decision

The `admission` module owns separately bounded capacities for:

1. connections and body receipt (`ReceiptPermit`)
2. inspection CPU and its wait queue (`InspectionPermit`)
3. original / parsed / transformed memory (`MemoryReservation`)
4. upstream in-flight requests (`UpstreamPermit`)
5. active response streams and buffers (`StreamPermit`)

Rules:

- Acquire capacity before allocating or scheduling. Do not trust `Content-Length`. The initial reservation may conservatively cover a bounded request, and a per-request quota cannot evade the aggregate budget.
- Permit ordering is fixed so reservations cannot deadlock. Acquisition order follows the request state order: receipt, then memory, then inspection, then upstream, then stream. A request never waits for an earlier-class permit while holding a later-class one. Memory is reserved up front for the bounded worst case rather than grown incrementally while holding other permits. The exact ordering is verified by #6 tests.
- Waits are bounded by an admission deadline and a bounded queue. Overload rejects with a safe error rather than waiting without limit.
- The inspection slot is released when actual inspection completes, not at SSE completion.
- Memory reservations are held while the associated buffers stay live (including original buffer lifetime extended by borrowed parsed values; see ADR 0007).
- Response-stream reservations last through stream completion and cleanup.
- Weighted reservations must account for fairness and head-of-line blocking (a large request must not starve small ones indefinitely, and small ones must not starve a large one). The queueing discipline is a measured choice.

Implementation note (verify against exact pins): Tokio `Semaphore` is a candidate primitive. Confirm its fairness and permit semantics in the selected version, not only latest docs.

## Owner

`admission` module. #2 scaffolds it; #18 wires it into the MVP route.

## Invariants

1. No allocation or task scheduling happens before the matching permit is held.
2. Permits are RAII values tied to the resource they guard, not to the HTTP future.
3. No unbounded queue, detached task, stream buffer, or body collection.
4. Aggregate reserved memory never exceeds the configured aggregate budget.
5. A completed inspection does not hold an inspection slot while its response streams.

## Failure behavior

Overload, admission timeout, and deadline expiry produce a safe overload/limit error before any upstream transmission. Reservations are released when the guarded resource ends, including on error paths.

## Implementation handoff

- #2: `admission` module with the five capacity owners and the permit ordering documented in code.
- #6: independent capacity tests, aggregate memory test, bounded overload, deadlock-prone acquisition detection, stream occupancy not holding a completed inspection slot.
- #18: wire admission into receipt and forwarding.

## Verification

Tests in #6 (listed above). Overload tests show rejection with bounded memory, not growth.

## Deferred measured choices

Every number: capacities, queue depths, deadlines, reservation weights, queueing discipline. Owner of the measurements: #5 for CPU/memory inputs, #6 for harness. Decision criteria: bounded memory under the concurrency workloads in ADR 0008 with acceptable p99 admission wait. No numbers are recorded here.
