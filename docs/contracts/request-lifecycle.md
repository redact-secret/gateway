# Request lifecycle: cancellation, deadlines, cleanup ownership, and shutdown

Status: **implemented and tested (#59)**. Rationale for the two changes this task made is in [ADR 0027](../decisions/0027-write-budget-and-bounded-response-frames.md). The mechanisms are [ADR 0003](../decisions/0003-resource-admission-and-lifetime.md) (capacity and lifetime), [ADR 0004](../decisions/0004-cancellation-and-synchronous-core-work.md) (synchronous core work), [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md) (JSON forwarding), [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md) (SSE), [ADR 0019](../decisions/0019-request-head-guard-and-one-request-per-connection.md) (head guard, one request per connection) and [ADR 0022](../decisions/0022-connection-bound-at-accept.md) (connection bound). Status codes and safe codes are in [errors-and-telemetry](errors-and-telemetry.md); numeric limits in [resource-limits](resource-limits.md).

This contract answers one question for every stage of a request: when something ends the request early (the caller leaves, a deadline passes, the process shuts down, the provider fails, the caller stops reading), what does the caller see, what is released and when, who owns the cleanup, and what has the provider already received.

## Rules that hold at every stage

1. **One owner, released by drop.** A request is one future (`ChatRoute::handle`) that nothing spawns. Every permit, reservation, buffer, and provider connection it holds is an owned value, so ending the future (a caller disconnect, hyper dropping it) or finishing it releases them in reverse acquisition order. After the response headers, the response body (`HeldBody` or `StreamBody`) is the owner. There is no relay task, no queue, and no cleanup that depends on a second actor remembering to run.
2. **The waiter is not the work.** A future that ends does not end work that already started on another thread. The inspection job (a synchronous, non-interruptible core call on a dedicated worker thread) owns its inspection permit and its memory reservation until the core call really returns, whatever happened to the waiter. A result nobody waits for is discarded and can never become a `SanitizedRequest`. See "Deadline of the waiter versus end of the work".
3. **Nothing leaves before the whole request is approved.** The first upstream request-body byte is written only after bounded parsing, complete inspection, transformation and revalidation succeeded (`SanitizedRequest`). Every failure, cancellation, and shutdown at a stage before "sanitized send" sends zero upstream bytes: no connection is opened. This is asserted by counting connections, requests, and send attempts at the provider and in telemetry.
4. **Data already sent upstream cannot be retracted.** From "upstream connect" onward the provider may hold some or all of the sanitized request. Cancelling closes the Gateway's side of the connection. It does not undo anything the provider already received or processed, and it does not stop a generation the provider already started. The Gateway never claims otherwise.
5. **No automatic retry, no replay, no fallback.** Exactly one send is attempted per request. A failure after the send was initiated (reset mid-body, no headers, a cut response) ends the request; the Gateway never resends the sanitized body and never falls back to the original body. SDK retries are the caller's, and a retried request is a new request through every stage again.
6. **After commitment nothing is added.** Once the response status line is written no second status exists. A later failure ends the response by closing the connection: a truncated JSON body (declared length not met) or a truncated SSE stream (no terminating chunk). The Gateway never writes a completion event, `[DONE]`, an error event, or any text into a stream, and never reports normal completion for a stream the provider did not complete.
7. **Errors and telemetry stay coarse.** Gateway-generated bodies are fixed `{"error":{"code":"<safe code>"}}` strings; counters, stage timings, and stream-end causes come from closed sets. No payload fragment, header value, credential, URL, or provider text enters an error, a metric, or a `Debug` rendering. Tests scan every observed Gateway output for synthetic markers.

## The stages

Legend for the test column (all paths are in this repository; `::` separates the file from the test):

- `L` = `src/transport/tests/lifecycle_tests.rs` (this task), `F` = `src/transport/tests/forward_tests.rs`, `S` = `src/transport/tests/stream_tests.rs`, `A` = `src/transport/tests/attack_tests.rs`
- `C` = `tests/chat_admission.rs`, `K` = `tests/connection_limits.rs`, `P` = `tests/permits_cancellation.rs`, `W` = `src/write_stall.rs`, `H` = `src/head_guard.rs`

"Upstream data" says what the provider can have received at that point. Cleanup owners are named by type or function.

### 1. Connection accept and head wait

Owners: the connection slot (`StallListener` accept loop; the slot is an `OwnedSemaphorePermit` held by `StallIo`, returned only after the socket is closed), the head deadline (`HeadGuardIo`, `body_deadline_ms`), and hyper's connection task.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Caller disconnects with an incomplete head | Connection dropped | At once, by dropping the IO | none | `L::disconnect_while_the_head_is_incomplete_returns_the_connection_slot`, `K::abandoned_connections_never_leak_slots` |
| Head deadline (silent or trickling head) | Connection closed, no response | At the close | none | `A::silent_partial_and_trickling_heads_are_cut_at_the_head_deadline`, `K::silent_connections_fill_the_bound_and_the_head_deadline_returns_it`, `H::a_silent_or_trickling_peer_is_cut_at_the_head_deadline` |
| Connection bound reached | New connection closed at accept, nothing queued | n/a | none | `K::slow_head_connections_hold_slots_until_they_close_and_leak_nothing` |
| Ambiguous or over-long head | Local `400`/`431`, close | After the refusal or the head deadline | none | `H::an_ambiguous_head_releases_nothing_to_the_reader`, `H::a_peer_that_never_reads_the_refusal_is_cut_at_the_head_deadline` |
| Shutdown | Idle and mid-head connections are closed by the server's graceful shutdown; accept stops | At the close | none | `L::shutdown_with_many_open_connections_and_streams_ends_within_the_drain`, `K::shutdown_with_the_bound_reached_and_exceeded_stays_bounded` |

### 2. Body receipt

Owner: `ReceiptTicket` (receipt permit plus the conservative `MemoryReservation`), reserved before the first body byte and owned by the handler future (`ChatRoute::admit`).

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Caller disconnects | Future dropped | At the drop | none | `L::disconnect_during_body_receipt_releases_reservation_and_forwards_nothing`, `C::client_disconnect_during_receipt_releases_capacity` |
| Body deadline (`body_deadline_ms`) | `408 limit_exceeded` | When the response is built | none | `L::body_deadline_during_receipt_is_a_408_and_returns_capacity`, `C::trickled_body_cannot_outlive_the_deadline`, `C::reservation_is_taken_before_the_first_body_byte_and_released_on_deadline` |
| Capacity wait elapses or queue full | `503 overload` + `Retry-After: 1` | Nothing was held | none | `C::admission_queue_is_bounded_and_waiters_proceed_when_capacity_returns`, `C::zero_wait_means_immediate_overload` |
| Shutdown after the drain deadline | `503 not_ready` | At the cancellation (the future is dropped by the `select`) | none | `L::shutdown_during_body_receipt_answers_503_and_returns_capacity` |
| Oversize or malformed framing | Fixed local rejection | At the rejection | none | `C::oversized_unknown_length_body_is_cut_off_at_the_reservation`, `A::ambiguous_and_malformed_framing_never_delivers_upstream_bytes` |

### 3. Parse and validate

Owner: the `ValidatedRequest` (keeps the reservation). The step is synchronous and has no await point between "body complete" and "validated", so a cancellation can never observe a half-validated request. Failures are fixed local rejections (`400`/`413`/`415`/`422`); the sealed type cannot be built from a failed parse. Tests: `F::every_pre_forward_failure_delivers_zero_bytes_upstream`, `L::pre_send_failures_across_the_lifecycle_deliver_zero_bytes_upstream`, `C::unsupported_payloads_are_rejected_with_zero_upstream_bytes`, `C::malformed_json_is_rejected_with_zero_upstream_bytes`.

### 4. Inspection queue (job submitted, not started)

Owner: the queued job inside `InspectionPool` (`QueuedJob`), which owns the inspection permit; the request and its `MemoryReservation` moved into the job closure. Queue plus running jobs never exceed the inspection capacity.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Caller disconnects | Waiter dropped; the job is marked cancelled | When a worker dequeues it and skips it (bounded by the queue ahead of it); the inspection permit and the memory return together; the core is never called | none | `L::a_queued_inspection_cancelled_by_disconnect_is_skipped_and_returns_its_capacity` |
| Shutdown | `503 not_ready` to the waiter; the job is skipped the same way | At the skip | none | `L::shutdown_while_inspection_is_queued_answers_503_and_the_job_is_skipped` |
| No inspection permit free | `503 overload`, nothing queued | Nothing was held | none | `F::every_pre_forward_failure_delivers_zero_bytes_upstream`, `L::a_new_request_is_refused_not_queued_while_running_jobs_hold_every_permit` |

### 5. Inspection running and 6. sanitize and serialize (one synchronous job)

Owner: the job on its worker thread (`InspectionPool` / `inspect_request`). The scan, the in-place replacement, and the bounded serialization are one job; the permit and reservation are released together when it returns, after the core call really ended.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Caller disconnects | Waiter dropped; the job keeps running to its end; its result is discarded and never becomes a `SanitizedRequest` | When the job returns (not when the waiter ends) | none | `L::a_started_inspection_keeps_cpu_and_memory_after_the_waiter_is_dropped`, `P::dropping_the_http_waiter_does_not_release_running_job_permits`, `P::a_cancelled_result_never_becomes_an_upstream_request` |
| Shutdown after the drain deadline | The waiter answers `503 not_ready` at once; the job still runs to its end | When the job returns | none | `L::shutdown_deadline_of_the_waiter_is_not_the_end_of_the_synchronous_work` |
| Many abandoned requests | New requests are refused, not queued, while jobs hold every permit; no counter exceeds its capacity | As each job returns | none | `L::a_new_request_is_refused_not_queued_while_running_jobs_hold_every_permit`, `P::repeated_cancellations_stay_within_inspection_and_memory_bounds`, `A::a_flood_of_abandoned_requests_returns_every_permit_and_task` |
| Repeated start/stop at every stage | Every counter and the task count return to baseline after each cycle | Each cycle | only the cycles that reached the send | `L::repeated_start_stop_cycles_return_every_counter_and_task_to_baseline` |
| Core failure, incomplete result, output over the bound | Fixed `4xx`/`5xx` safe code; no sealed value | At the job's return | none | `F::every_pre_forward_failure_delivers_zero_bytes_upstream`, `tests/inspection_route.rs` |

There is no inspection deadline: the core cannot be interrupted, so a timeout could only abandon the waiter while the job kept its capacity. The bound on this stage is the input bound (`max_body_bytes`, parse budgets) and the inspection capacity, not a clock. See below.

### 7. Upstream and stream permit wait

Owners: `UpstreamPermit` and `StreamPermit`, acquired with `try_*` (no wait) after inspection released its permit, held by the handler frame and then moved to the body. A full class is an immediate `503 overload` with `Retry-After: 1`.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Upstream class full | `503 overload` | Inspection result dropped, memory returned | none | `F::overload_is_immediate_bounded_and_returns_all_capacity`, `L::pre_send_failures_across_the_lifecycle_deliver_zero_bytes_upstream` |
| Stream class full (`stream: true`) | `503 overload`; the upstream permit is released | Both | none | `L::pre_send_failures_across_the_lifecycle_deliver_zero_bytes_upstream`, `S::stream_capacity_bounds_streams_upstream_occupancy_and_tasks` |
| Disconnect or shutdown while the waiter is awaiting an earlier step | The later steps never run | n/a | none | `F::a_cancelled_waiter_never_starts_an_upstream_request`, `S::a_cancelled_waiter_never_starts_an_upstream_request_or_a_stream` |

### 8. Upstream connect (DNS policy, TCP, TLS)

Owner: the `Upstream::forward` future (it owns the sealed request, the permit, and the connection attempt). Deadline: `upstream_connect_ms`. The sealed request body is not written until the connection (and TLS) is established.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Refused, unreachable, or policy-denied address | `502 upstream_unavailable` | The future ends | none (no request) | `F::connect_failure_is_unavailable_and_not_retried`, `tests/destination_policy.rs` |
| TLS failure | `502 upstream_tls_failure` | The future ends | none (no request) | `F::tls_failure_is_a_distinct_safe_code_and_sends_no_request` |
| Connect deadline | `504 upstream_timeout` | The future ends | none | covered by the same deadline code path as the header deadline (`L::stuck_upstream_headers_end_in_a_504_once_and_close_the_exchange`) |
| Disconnect or shutdown while connecting | Future dropped, connection attempt abandoned | At the drop | none | **Gap by construction**: a loopback connect completes at once, so a deterministic "connect in progress" cannot be produced without a production seam. The cancellation is the same drop of the same future as in stage 10, which is tested. |

### 9. Sanitized send

Owner: the same `Upstream::forward` future. The send attempt is counted (`upstream_attempts`) when initiated; exactly one is made.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Provider resets or closes mid-request | `502` (a coarse upstream code) before any response; no second attempt | The future ends; the connection is closed | **Part of the sanitized body may have been delivered.** Never replayed, never the original body | `L::a_partial_upstream_send_is_never_replayed_and_never_falls_back` (JSON and `stream: true`), `F::post_forward_failures_terminate_safely_with_one_attempt_and_no_fallback` |
| Disconnect or shutdown during the send | Future dropped; the connection is closed | At the drop | Possibly part or all of the sanitized body; not retractable | `F::downstream_disconnect_cancels_the_upstream_exchange_and_releases_everything` |

### 10. Upstream headers wait

Owner: the `forward` / `forward_stream` future; deadline `upstream_header_ms` (a non-streamed completion is answered only when generation ends, so this is long).

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Header deadline (provider silent) | `504 upstream_timeout`; connection closed; no retry | The future ends | The whole sanitized request was sent; the provider may still be working | `L::stuck_upstream_headers_end_in_a_504_once_and_close_the_exchange` |
| Caller disconnects | Future dropped; provider connection closed (observed at the provider) | At the drop | Whole request sent; the provider's work is not stopped by the Gateway | `L::disconnect_while_waiting_for_upstream_headers_cancels_the_exchange`, `F::downstream_disconnect_cancels_the_upstream_exchange_and_releases_everything` |
| Timeout and disconnect together | Whichever wins, the request ends once, the counters return, exactly one request reached the provider per trial | One release | Whole request sent | `L::simultaneous_header_timeout_and_disconnect_settle_exactly_once` (24 trials across the deadline) |
| Shutdown after the drain deadline | `503 not_ready`; provider connection closed | At the cancellation | Whole request sent | `L::shutdown_while_waiting_for_upstream_headers_answers_503_and_closes_the_exchange`, `F::shutdown_cancels_in_flight_requests_and_returns_every_permit` |
| Malformed, over-large, or redirecting provider headers | `502 upstream_invalid_response` (or the provider's redirect relayed unfollowed) | The future ends | Whole request sent | `F::post_forward_failures_terminate_safely_with_one_attempt_and_no_fallback`, `A::provider_redirects_are_returned_not_followed_on_json_and_sse_paths` |

### 11. JSON relay (buffer, then write)

Two phases with different commitment.

**Buffering** (before the response is committed): the `forward` future reads the body under `max_response_body_bytes` and the `upstream_total_ms` deadline. Failures are ordinary Gateway errors with a real status (`504 upstream_timeout`, `502 upstream_response_too_large` / `upstream_invalid_response`). Disconnect and shutdown drop the future (the shutdown answers `503 not_ready`). Tests: `F::post_forward_failures_terminate_safely_with_one_attempt_and_no_fallback`, `F::shutdown_cancels_in_flight_requests_and_returns_every_permit`.

**Writing** (after the status line is committed): the owner is `HeldBody`, which holds the `UpstreamPermit` and the buffered bytes and hands them to the HTTP server in frames of at most 16 KiB. The server asks for the next frame only when its own write buffer has room, and drops the body when it has taken the last frame, so the permit is held until all but the last few frames (a window bounded by the server's per-connection write buffer, not counted in the capacity classes) were accepted by the socket.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Consumer does not read | Write makes no progress for `stream_write_stall_ms`: the connection is closed, the response is truncated against its declared length (no second status) | The server drops the response and the IO | Whole request sent, provider response read | `L::a_buffered_json_response_to_a_consumer_that_never_reads_is_cut_and_returns_its_permit`, `W::a_write_with_no_progress_for_the_deadline_fails_with_timed_out` |
| Consumer reads a trickle, never stalled | The cumulative blocked-write time reaches the write budget (`stream_lifetime_ms`): the connection is closed | As above | as above | `L::a_consumer_that_reads_a_trickle_cannot_hold_a_json_response_past_the_write_budget`, `L::the_production_listener_cuts_a_trickle_reader_of_a_real_connection`, `W::steady_slow_progress_is_cut_by_the_cumulative_write_budget` |
| Permit lifetime | The permit stays held while the response is being handed to the socket, not only until the server took the single buffer | At the body drop | n/a | `L::a_buffered_body_hands_the_server_bounded_frames_and_keeps_the_permit_until_dropped`, `L::shutdown_during_a_blocked_json_write_returns_at_the_drain_plus_grace` |
| Refusals (local error bodies) to a consumer that does not read | The same stall deadline applies to every response write | Same | none | `L::a_refusal_to_a_consumer_that_never_reads_is_cut_by_the_write_stall` (in-memory pipe, because a refusal is smaller than a socket buffer) |
| Caller disconnects mid-write | Write fails, server drops the body | At the drop | Whole request sent | `F::downstream_disconnect_cancels_the_upstream_exchange_and_releases_everything` |
| Shutdown mid-write | The cancellation does not interrupt a committed write (no second status exists); see "Shutdown and drain" | When the write ends, the consumer leaves, or the process exits | Whole request sent | `L::shutdown_during_a_blocked_json_write_returns_at_the_drain_plus_grace` |

### 12. SSE relay

Owner: `StreamBody`, which owns the `UpstreamPermit`, the `StreamPermit`, and the provider connection from the committed headers until the stream ends, closing the provider connection first. The relay holds at most one provider chunk (`stream_buffer_bytes`) and spawns nothing.

| Trigger | Outcome | Released when | Upstream data | Tests |
| --- | --- | --- | --- | --- |
| Provider ends cleanly | Terminating chunk; `Completed` | Body end | n/a | `S::a_clean_end_on_the_wire_has_the_terminating_chunk_and_a_cut_does_not` |
| Idle deadline (`stream_idle_ms`, counted only while waiting on the provider) | Abrupt end: no terminating chunk, no marker, no second status; `IdleTimeout` | At the failure (provider connection closed first) | Whole request sent | `L::silence_after_trickle_is_cut_at_the_idle_deadline_without_a_marker_or_second_status`, `S::failures_after_the_headers_end_the_stream_abruptly_without_a_completion_event` |
| Provider trickles just under the idle deadline | The stream lives (the idle clock restarts on each chunk) until the lifetime | Body end | n/a | `L::trickling_just_under_the_idle_deadline_survives_and_completes` |
| Lifetime deadline (`stream_lifetime_ms`) | Abrupt end; `LifetimeExceeded` | At the failure | Whole request sent | `S::failures_after_the_headers_end_the_stream_abruptly_without_a_completion_event` |
| Provider failure, malformed or over-large chunk | Abrupt end; `UpstreamError` / `BufferExceeded` | At the failure | Whole request sent | same |
| Consumer stops reading (abandoned SSE) | Write stall closes the connection; `Abandoned` | At the body drop | Whole request sent | `L::an_abandoned_sse_connection_that_is_never_read_is_cut_and_its_permits_return`, `S::slow_consumer_is_backpressured_with_bounded_memory_then_cut_by_the_stall_deadline` |
| Consumer disconnects | Body dropped; provider connection closed; `Abandoned` | At the drop | Whole request sent | `S::downstream_disconnect_cancels_the_upstream_stream_and_returns_everything` |
| Idle timeout and disconnect together | Each stream ends exactly once; started equals ended; counters and tasks return to baseline (20 trials across the deadline) | One release | Whole request sent | `L::simultaneous_idle_timeout_and_disconnect_end_every_stream_exactly_once` |
| Shutdown after the drain deadline | Abrupt end; `Shutdown`; provider connection closed | At the failure | Whole request sent | `S::shutdown_cancellation_ends_open_streams_and_closes_upstream`, `L::shutdown_with_many_open_connections_and_streams_ends_within_the_drain` |

### 13. Shutdown and drain

Owner: `BoundServer::serve` and `ChatRoute::cancel_in_flight`. On the shutdown signal the Gateway reports not ready, stops accepting, and lets in-flight work finish for at most `shutdown_drain_ms`. At the drain deadline it cancels what remains and waits a fixed grace (1 s) for connections to close before `serve` returns; nothing waits beyond `drain + grace`.

| Work at the drain deadline | Termination | Tests |
| --- | --- | --- |
| Receipt, queue, running inspection, permit wait, connect, send, header wait, JSON buffering (no response committed) | The waiter answers `503 not_ready` with `Connection: close`; provider connections are closed; capacity is returned (inspection capacity when the job really ends) | `F::graceful_shutdown_is_bounded_by_the_drain_deadline`, `L::shutdown_with_many_open_connections_and_streams_ends_within_the_drain` |
| Open SSE streams | Abrupt end (no terminating chunk, no marker); provider connection closed first; `Shutdown` counted | `S::graceful_shutdown_with_an_open_stream_is_bounded_by_the_drain_deadline`, `L::shutdown_with_many_open_connections_and_streams_ends_within_the_drain` |
| Idle, mid-head, and new connections | Closed by the graceful shutdown; accept stopped | `L::shutdown_with_many_open_connections_and_streams_ends_within_the_drain`, `K::shutdown_with_the_bound_reached_and_exceeded_stays_bounded` |
| A JSON response already committed and being written | Not interruptible; bounded by the stall and budget deadlines; `serve` still returns at `drain + grace` and the remaining connection ends with the process | `L::shutdown_during_a_blocked_json_write_returns_at_the_drain_plus_grace` |
| A started inspection job | Not interruptible (the core has no cancellation); its waiter has already been answered; the worker finishes its call, or is abandoned by the process exit | `L::shutdown_deadline_of_the_waiter_is_not_the_end_of_the_synchronous_work`, `P::*` |
| Requests arriving after cancellation | `503 not_ready` at once | `L::pre_send_failures_across_the_lifecycle_deliver_zero_bytes_upstream` |

After the drain, nothing admitted keeps running except the two non-interruptible cases above, and both are bounded by a stated deadline or by process exit. No request is retried or resumed after the shutdown.

## Deadline of the waiter versus end of the work

Two different things end at different times, and the Gateway keeps them apart:

- The **waiter** is the HTTP handler future. A caller disconnect or the shutdown cancellation ends it immediately, and it answers `503 not_ready` where an answer is still possible.
- The **work** is the synchronous inspection job on a worker thread. It ends when the core call returns. Until then the job owns the inspection permit (the CPU slot) and the memory reservation. A new request that needs that capacity is refused as `overload` rather than queued behind work nobody is waiting for, so abandoned requests cannot raise real concurrency or memory above the configured limits.

Consequently the inspection capacity returns later than the waiter ends, by at most the remaining time of one bounded core call (or, for a still-queued job, until a worker dequeues and skips it). Any time bound the Gateway reports for the waiter is not a promise about the CPU or the memory of the work. At process exit a worker still inside a core call is abandoned, not interrupted.

## Shutdown order and what is bounded

`serve` returns at most `shutdown_drain_ms` plus a fixed 1 s grace after the signal. Resources owned by a connection that outlives `serve` (a committed JSON write, a started inspection job) end when their own deadline fires or when the process exits, which is the last step after `serve` returns. The memory in the HTTP server's per-connection write buffer and in kernel socket buffers is not part of the capacity classes (see [resource-limits](resource-limits.md)). Bytes already in a kernel buffer when a connection is closed are still delivered to the peer; the Gateway cannot retract them.

## Evidence and method

The tests above gate on events and counters (a request received by the scripted upstream, a peer closed, a job parked on a barrier by a `cfg(test)`-only gate, capacity counters back at baseline), never on a sleep as synchronization. A wait is used only where a deadline is itself the subject, with wide margins, or as a negative observation window ("nothing else arrives"). Race cases (timeout against disconnect) run many trials with the cancellation moved across the deadline and assert the same invariants in every trial. The suite is run repeatedly in CI and was run at least ten times locally without a failure when this contract was written.

Not covered, by design: the connect stage cancellation (see stage 8), and kernel-level behavior (socket buffer sizes differ by host, so the real-socket tests assert server-side release and rely on a response larger than any socket buffer).
