# ADR 0027: Cumulative write budget and bounded response frames

Status: Accepted; implemented (#59). Amends the response-write behavior of [ADR 0017](0017-json-forwarding-deadlines-and-cancellation.md) and [ADR 0018](0018-sse-relay-termination-and-stream-bounds.md). The stage-by-stage contract is [request-lifecycle](../contracts/request-lifecycle.md). Date: 2026-10-03.

## Context

The lifecycle audit of #59 found two places where a response could be held longer, or counted less, than the contracts say.

1. **A slow reader is not a stalled reader.** The write-stall deadline restarts on every byte of progress ([ADR 0018](0018-sse-relay-termination-and-stream-bounds.md), `write_stall`). A consumer that reads a few bytes inside each stall window is never cut by it. For SSE the stream lifetime bounds that consumer. A buffered JSON response has no lifetime after its last upstream byte was buffered (`upstream_total_ms` ends there), so a trickle reader could keep one `UpstreamPermit`, a buffer of up to `max_response_body_bytes` (4 MiB provisional), and a socket for as long as it liked. ADR 0018 says this case "is bounded by the lifetime deadline instead", which is true for streams only.
2. **The permit was released before the buffer was gone.** `HeldBody` was one frame holding the whole buffered response. The HTTP server takes a frame, sees the body is at its end, and drops the body immediately, which returned the `UpstreamPermit` while up to 4 MiB still sat in the server's write queue behind the socket. A slow reader therefore held memory no capacity class accounted for, and the permit could be reused to buffer the next response meanwhile. ADR 0003 requires capacity to be held until the owning resource really ends. A test that blocks the socket and then looks at the upstream class showed the permit free while the write was in progress.

## Decision

1. **A cumulative write budget on every accepted connection.** `StallIo` sums the time its writes spend pending (blocked on a consumer that is not reading) over the whole connection. When the sum reaches the budget the next pending write fails with a timeout, exactly like the stall deadline, and the server drops the connection and the response. The stall timer is also never armed for longer than the budget has left, so the cut happens at the budget, not at the next multiple of the stall window. Time with nothing to write does not count. The budget equals `resources.limits.stream_lifetime_ms`: one connection carries one request ([ADR 0019](0019-request-head-guard-and-one-request-per-connection.md)), a stream's blocked time can never exceed its own lifetime (so streams are unaffected), and a buffered JSON response gets the same finite bound. No new setting. `server::guarded_listener` applies it through `StallListener::with_write_budget`.
2. **Bounded response frames.** `HeldBody` hands the buffered response to the HTTP server in frames of at most 16 KiB (`FRAME_BYTES`) and reports the remaining length exactly. The server asks for the next frame only when its write buffer has room, so the body, and with it the permit and the buffered bytes, lives until all but the last few frames were accepted by the socket. What can remain after the permit returns is the server's own bounded per-connection write buffer, the same category the resource-limits contract already excludes for streams.
3. **No other design change.** The lifecycle contract documents behavior that already existed and was untested: queued and running inspection under cancellation, the waiter versus the work, shutdown by stage, and the no-replay rule. Tests, not code, close those rows.

## Owner

`src/write_stall.rs` (`StallIo`, `StallListener`), `src/server.rs` (`guarded_listener`), `src/transport/relay.rs` (`HeldBody`).

## Invariants

- A write that is blocked for the stall deadline, or whose blocked time totals the budget, ends the connection. A response is never completed with a different status; the declared length is not met and the peer sees a truncated response.
- A flush or write that completes does not reset the cumulative total; only the pending interval itself is added to it.
- The upstream permit is released only when the body is dropped: after the last frame was taken by the server, or earlier by a cut, a disconnect, or the shutdown.
- Neither change touches what is sent upstream, inspection, or the termination contract of streams.

## Failure behavior

A consumer that reads too slowly is cut: no status, no marker, closed connection, permit and buffer returned. A consumer that reads at a normal pace is unaffected: the budget is the stream lifetime (900 s provisional), and the blocked time of a healthy local consumer is milliseconds.

## Verification

`src/write_stall.rs` unit tests (`steady_slow_progress_is_cut_by_the_cumulative_write_budget`, `a_budget_larger_than_the_transfer_changes_nothing`, `idle_time_with_nothing_to_write_does_not_spend_the_budget`); `src/transport/tests/lifecycle_tests.rs` (`a_consumer_that_reads_a_trickle_cannot_hold_a_json_response_past_the_write_budget`, `the_production_listener_cuts_a_trickle_reader_of_a_real_connection`, `a_buffered_body_hands_the_server_bounded_frames_and_keeps_the_permit_until_dropped`, `shutdown_during_a_blocked_json_write_returns_at_the_drain_plus_grace`). The first of these failed against the previous code (the trickle reader was still holding the response after 8 s) and the last failed with the permit already free while the write was blocked.

## Deferred measured choices

The budget value (the stream lifetime) and the frame size (16 KiB) are provisional and follow the existing measurement gate ([ADR 0008](0008-performance-measurement-gate.md)). A tighter budget for buffered JSON than for streams would need its own setting and is not added.

## Implementation status

Implemented (#59).
