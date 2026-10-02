# ADR 0004: Cancellation and synchronous core work

Status: Accepted (design); worker strategy deferred to #5 measurements. Implementation status: implemented for inspection in #19 (dedicated worker pool whose jobs own the `InspectionPermit` and the request's `MemoryReservation` until real completion; cancelled results discarded; [ADR 0015](0015-core-inspection-and-request-transformation.md)); the pre-upstream cancellation check and the shutdown drain deadline are planned with #20. Covers issue #3 section C.

## Context

Dropping an async future, or timing it out, does not stop synchronous work that already started. Tokio `spawn_blocking` work that has started cannot normally be aborted. If an HTTP future held the CPU and memory permits, dropping it would return capacity while the work still runs, and repeated cancelled requests could exceed the real limits.

## Decision

- Queued jobs may be removed on cancellation.
- A started synchronous job may be non-interruptible. Dropping its awaiter does not stop it.
- The worker execution, not the HTTP future, owns the `InspectionPermit` and `MemoryReservation` for that job. Capacity returns only when the job really finishes.
- A cancelled job's result is disposed. It never becomes an upstream request.
- Check cancellation (client disconnect, deadline, shutdown) immediately before initiating a new upstream request.
- Document the races: bytes already transmitted cannot be retracted, and a cancellation that arrives after the upstream request starts is propagated to the upstream operation but cannot undo delivered bytes.
- Remaining work is bounded by accepted input and by inspection concurrency.
- Shutdown behavior: stop accepting new work, reject queued work, wait for started work up to a bounded shutdown deadline, then exit. The gateway does not promise immediate interruption of started core calls.
- Execution mode (inline, bounded `spawn_blocking`, or a dedicated pool) is chosen with #5 measurements. If core lacks cooperative cancellation, the HTTP API does not invent it.

## Owner

`admission` (permit ownership) and `core_bridge` (job execution). #5 owns the measurement and recommendation.

## Invariants

1. Permit ownership transfers to the worker at job start and ends at real job completion.
2. A cancelled result is never forwarded.
3. Repeated cancelled requests cannot exceed inspection concurrency or the memory reservation.
4. No detached task without a cancellation and cleanup owner.
5. No claim of cooperative cancellation unless the pinned core exposes it and #5 verified it.

## Failure behavior

Cancelled before start: job removed, permits released. Cancelled after start: job runs to completion holding its permits, result dropped, permits released, no upstream call. Shutdown deadline expiry: process exits and in-flight work is abandoned; this is documented, not hidden.

## Implementation handoff

- #5: verify core initialization, completion, and cancellation capabilities for the exact pin. Compare bounded inline inspection with a bounded offload candidate. Record shutdown behavior.
- #6: controllable non-interruptible job and cancellation test showing a dropped waiter does not free reservations early.
- #18: pre-upstream cancellation check.

## Verification

#6 tests: capacity not returned while a simulated non-interruptible job runs; repeated cancellations stay within bounds; no result forwarded after cancel.

## Deferred measured choices

Execution mode, worker count, pool size, shutdown deadline. Owner: #5 recommends, maintainer approves. Criteria: reactor responsiveness under load, overhead versus inline, bounded p99 latency on the ADR 0008 workloads. No value is chosen here.
