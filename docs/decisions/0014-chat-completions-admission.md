# ADR 0014: Chat Completions request admission and provisional limits

Status: Accepted. Implementation status: implemented for admission, strict budgeted parsing, and field classification (#18); forwarding of the approved request is implemented in #20 ([ADR 0017](0017-json-forwarding-deadlines-and-cancellation.md)); `stream: true` is relayed as SSE by #21 ([ADR 0018](0018-sse-relay-termination-and-stream-bounds.md)). Date: 2026-10-02. Implements the #3 design contracts for issue #18: [ADR 0002](0002-request-states-and-forwarding-authority.md), [ADR 0003](0003-resource-admission-and-lifetime.md), [ADR 0005](0005-protocol-expansion-and-central-enforcement.md), [ADR 0007](0007-parsing-copying-and-plaintext-lifetime.md).

## Context

#18 turns the scaffolded owners into a working front half of `POST /v1/chat/completions`: route/method/header admission, bounded receipt, one strict parse, and a text-only field matrix. Several choices are open: how to reserve memory without trusting `Content-Length`, how waiting is bounded, which fields the Alpha 1 subset admits, what the typed result looks like, which status codes are used, and what numbers to use before a quiet-host measurement exists.

## Decision

1. **Admission order.** Method, target, and headers are checked before anything is reserved; then `Admission::begin_body_receipt` takes the `ReceiptPermit` and a `MemoryReservation`; then the body is collected under a deadline and a hard byte bound; then `protocol::validate_with` parses and classifies. Each step fails closed with a fixed safe code, and the connection is closed on every rejection.
2. **Reservation does not trust `Content-Length`.** The reservation is a function of the configured limits and the bound `B` (`4*B + min(max_nodes, B)*128` bytes, in 1 KiB units). A declared length can only lower `B`, and collection refuses more bytes than the reservation covers, so unknown-length, chunked, slow, and lying bodies cannot exceed aggregate reservations. The per-request body limit is clamped to what the global budget can ever cover, so the two limits compose.
3. **Bounded waiting.** If capacity is not free, a request joins a bounded queue (`admission_queue`) and waits at most `admission_wait_ms`; a full queue or timeout is `503 overload` with `Retry-After`. Waiters hold only the receipt permit while waiting for memory (acquisition order receipt then memory), so the order cannot deadlock. The semaphore is FIFO, so large reservations are not starved. The body has a total deadline (`body_deadline_ms`), so a stalled or trickling client cannot hold a reservation indefinitely.
4. **One working structure.** `protocol::json::parse_budgeted` builds one owned tree with duplicate-key rejection after decoding and enforces depth, node, and string budgets while building. `protocol::chat::classify` consumes that tree and moves its strings into the typed `ChatRequest`, so there is no body-sized clone. The original buffer is released as soon as the parse ends; the reservation stays with the `ValidatedRequest`. Borrowed or SIMD parsing stays deferred until measured; any replacement must pass the shared conformance table.
5. **Text-only matrix.** Documented in [chat-completions-request](../contracts/chat-completions-request.md). Allowed: `model`, `messages` (roles system/developer/user/assistant, string or text-part content), `stream`, `stream_options.include_usage`, six sampling/length controls, `n == 1`, `seed`, `stop`, `user`, `response_format` of `text` or `json_object`. Everything else is rejected, including all tool forms, non-text parts, `name`, `metadata`, and unknown fields at any depth. Free text is inspected text, never a "control" field; the only free-form structural strings are `model` (a tightly constrained identifier) and numbers.
6. **Typed boundary value.** `ValidatedRequest` holds `ChatRequest` with private fields and text reachable only through `for_each_text(_mut)` in a fixed order. `chat_route::Admitted` pairs it with the operator-defined `RouteId` from the static table (`openai.chat_completions`), never from the request. `SanitizedRequest` stays boundary-only (#19).
7. **Status mapping** (documented in [errors-and-telemetry](../contracts/errors-and-telemetry.md)): retryable-by-SDK statuses (`408`, `503`, `5xx`) are used only for conditions the caller cannot fix by changing the request; everything else is a non-retried `4xx`. A new safe code `not_implemented` (`501`) marks the temporary end of the path; #19/#20 replace it.
8. **Provisional numbers.** The defaults in [resource-limits](../contracts/resource-limits.md) are finite and conservative with written rationale, and are explicitly provisional pending quiet-host measurement. The only available benchmark ([core probe](../probes/core-bridge-probe.md)) was run on a loaded host. They are configuration (`resources.limits`, all optional), not constants.

## Owner

`admission` (limits, reservation, bounded wait), `protocol` (parse, matrix, typed result), `chat_route` (HTTP admission and the router mount).

## Invariants

1. No allocation proportional to the body, and no body byte is read, before the receipt permit and memory reservation are held.
2. Live reservations never sum above `memory_units`.
3. A request that fails admission, parsing, a budget, or the matrix leaves nothing sent upstream and returns every permit.
4. Responses and `Debug`/`Display` output of every involved type contain no request text, header values, field names, or parser messages.
5. No path from `ReceivedRequest` or `ValidatedRequest` to transport exists; the route ends in `not_implemented`.

## Failure behavior

Every admission, receipt, parse, budget, and matrix failure is a fixed local response (`400`, `404`, `405`, `408`, `413`, `415`, `422`, `503`) with the connection closed. Client disconnect drops the handler future and releases its permits.

## Verification

`tests/chat_admission.rs` (real HTTP on loopback with a fake upstream asserted empty after every case, response-marker scans, reservation-before-first-byte, aggregate budget, bounded queue, deadlines, trickled body, disconnect), unit tests in `admission.rs`, `protocol/json.rs`, `protocol/chat.rs`, `chat_route.rs`, `config.rs`, and the shared `tests/parser_conformance.rs` table run against the budgeted parser.

## Deferred measured choices

All values in `resources.limits` (ADR 0008): quiet-host measurement of parse time and peak memory per body size, the node cost and buffer-copy factors, whether parsing needs `spawn_blocking`, and queue/wait values under the concurrency workloads. A header-read timeout and a pre-header connection bound belong with transport hardening (#25).

## Consequences and limits

Strict unknown-field rejection can break SDK calls that send extra fields. Retry behavior of the SDKs for `408`/`5xx` is taken from their documented defaults and is verified in #22. Nothing here claims detector coverage; inspection is #19.
