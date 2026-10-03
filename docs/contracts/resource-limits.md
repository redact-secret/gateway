# Contract: resource limits

Status: categories approved. **Per-request body, parse, and receipt limits have numeric values (#18) that are provisional pending quiet-host measurement.** Capacity counts (receipt, memory, inspection, upstream, stream) still have no defaults and come from configuration. **Upstream connect, response-header, and total deadlines, response header and body byte bounds, and the shutdown drain deadline have provisional values (#20, below).** **Stream idle and lifetime deadlines, the write-stall deadline, and the per-stream relay buffer bound have provisional values (#21, below).** **The accepted-connection bound has a provisional value (#40, below).** Mechanism: [ADR 0003](../decisions/0003-resource-admission-and-lifetime.md); measurement gate: [ADR 0008](../decisions/0008-performance-measurement-gate.md); decision record: [ADR 0014](../decisions/0014-chat-completions-admission.md).

Every category below is finite, configurable through the validated static configuration, and tested near and across its boundary. A finite per-request limit is never enough alone: there is also an aggregate budget.

| Category | Capacity owner / permit | Notes |
| --- | --- | --- |
| Request bytes | `ReceiptPermit` | Do not trust `Content-Length` |
| Concurrent body receipt | `ReceiptPermit` | |
| Accepted connections | connection slot (`max_connections`, accept time) | Separate budget; immediate close over the bound, no queue (#40, [ADR 0022](../decisions/0022-connection-bound-at-accept.md), below) |
| JSON depth and node count | `MemoryReservation` | Parsed structure budget |
| Decoded string bytes / inspected text | `MemoryReservation` | |
| Findings | `boundary` / `core_bridge` | Over-limit rejects (`limit_exceeded`); request-wide, summed across texts |
| Transformed output size | `MemoryReservation` | |
| Aggregate original/parsed/transformed memory | `MemoryReservation` | Held while buffers are live |
| Inspection concurrency and wait queue | `InspectionPermit` | Held until real completion |
| Upstream in-flight requests | `UpstreamPermit` | |
| Active response streams and buffers | `StreamPermit` | Owned by the response body from the headers until the stream ends or is dropped; the provider connection closes first (#21) |
| Response header and buffer bounds, total bytes | `StreamPermit` | Do not accumulate an entire SSE stream: the relay holds at most one provider chunk (`stream_buffer_bytes`) |
| Deadlines: admission, body receipt, upstream, idle, total/stream lifetime | `admission` / `transport` | |
| Shutdown deadline | `admission` | ADR 0004 |

## Rules

- Reserve before allocating or scheduling.
- No unbounded queue, detached task, stream buffer, or body collection.
- Overload fails before unbounded allocation, with a safe error.
- A number is published as a default only with a recorded measurement and pins. The values below are the exception the issue (#18) asks for: finite, conservative, justified, and **explicitly provisional**. They are not measured on a quiet host, and the only available data (the [core probe](../probes/core-bridge-probe.md)) was taken on a loaded host. Treat them as ceilings chosen to be safe, not as tuned values or performance claims.

## Request limits (provisional pending quiet-host measurement)

Configured under optional `resources.limits` ([configuration](../configuration.md)); an absent field keeps the value below. Each has a hard ceiling that validation enforces so every product stays computable.

| Field | Provisional value | Ceiling | Rationale |
| --- | --- | --- | --- |
| `max_body_bytes` | 1,048,576 (1 MiB) | 16 MiB | Text-only chat prompts: 1 MiB is about 250k tokens of English, above common context windows, yet small enough that the worst-case reservation below is a few MiB. Images and files are rejected, so large bodies have no legitimate use in this subset. |
| `max_depth` | 16 | 64 | The supported shape nests five containers deep. 16 leaves slack without letting recursion grow. |
| `max_nodes` | 16,384 (values plus keys) | 1,048,576 | A 256-message conversation with array content uses on the order of 3,000 nodes. About 5x headroom. Node count drives tree memory, so it is budgeted separately from bytes. |
| `max_string_bytes` | 524,288 (512 KiB) | 16 MiB | Half the body bound: one string cannot consume the whole request, and a per-string cap makes a single oversized message fail early. |
| `max_messages` | 256 | 4,096 | Long agent histories fit; unbounded arrays do not. |
| `admission_wait_ms` | 250 | 60,000 | How long a request may wait for capacity before `overload`. Short, so a local caller sees backpressure quickly and the SDK retry decides. `0` means never wait. |
| `admission_queue` | 16 | 1,024 | Most requests waiting at once for capacity. A full queue fails immediately, so waiters are bounded in number and time. `0` means no queue. |
| `body_deadline_ms` | 10,000 | 300,000 | Time to receive the whole body after capacity is reserved. Since #25 it is also the absolute deadline for the request head, from accept to the blank line (`head_guard`, [ADR 0019](../decisions/0019-request-head-guard-and-one-request-per-connection.md)). A 1 MiB body takes milliseconds on loopback; ten seconds tolerates a slow client while bounding how long a stalled one holds its reservation. |

## Upstream deadlines and response bounds (#20; provisional pending quiet-host measurement)

Same configuration object and the same status as the table above: finite, validated against ceilings, justified, **not measured**. Mechanism and rationale: [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md).

| Field | Provisional value | Ceiling | Rationale |
| --- | --- | --- | --- |
| `upstream_connect_ms` | 5,000 | 60,000 | TCP plus TLS to a public provider; long enough for a slow path, short enough to free the permit. Fixed on the client at startup. |
| `upstream_header_ms` | 120,000 | 3,600,000 (and at most `upstream_total_ms`) | A non-streamed completion is answered only when generation ends; reasoning-heavy requests can take minutes. A deadline, not a latency goal. |
| `upstream_total_ms` | 300,000 | 3,600,000 | Send to the last buffered response byte. |
| `max_response_header_bytes` | 32,768 | 262,144 | Provider headers are small (hundreds of bytes); 32 KiB is far above any legitimate set. |
| `max_response_body_bytes` | 4,194,304 (4 MiB) | 67,108,864 | An ordinary chat completion is tens of KiB; 4 MiB leaves room for large `n` or long outputs. Buffered memory is bounded by `upstream` permits times this value. |
| `shutdown_drain_ms` | 10,000 | 600,000 | Time in-flight requests may finish after a termination signal before they are cancelled. `0` cancels immediately. |

The buffered response is not part of the request `MemoryReservation` (its size is unknown until read). It is bounded instead by the `UpstreamPermit` it holds until the body is written or abandoned: at most `resources.capacity.upstream * max_response_body_bytes` bytes are buffered at once. The HTTP parser caps response header fields at 64 and has its own buffer ceiling; `max_response_header_bytes` is enforced after parsing.

## Stream limits (#21; provisional pending quiet-host measurement)

Same configuration object, same status: finite, validated against ceilings, justified, **not measured**. Mechanism and rationale: [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md).

| Field | Provisional value | Ceiling | Rationale |
| --- | --- | --- | --- |
| `stream_idle_ms` | 120,000 | 3,600,000 (and at most `stream_lifetime_ms`) | Longest wait for the next provider chunk, counted only while waiting on the provider. Reasoning models can be silent for a long time before the first token; the deadline detects a stalled provider, it is not a latency goal. |
| `stream_lifetime_ms` | 900,000 (15 min) | 3,600,000 | Longest total stream life. Infinite streams are a non-goal; this also bounds a consumer that reads just fast enough to avoid the write stall. |
| `stream_write_stall_ms` | 30,000 | 600,000 | Longest a pending response write may make no progress before the connection is closed. A local consumer that is alive reads in milliseconds; thirty seconds tolerates a paused one while bounding how long a dead one holds a stream permit and a provider connection. Applies to every accepted connection. |
| `stream_buffer_bytes` | 1,048,576 (1 MiB) | 16,777,216 | Most provider bytes the relay holds at once (one provider chunk). Above the HTTP client library's maximum read buffer (about 400 KiB), so ordinary streams never reach it. |

Aggregate relay memory is at most `resources.capacity.stream * stream_buffer_bytes`; startup validation rejects a combination above 4 GiB (`invalid_combination`). Stream occupancy is independent of the other classes: an open stream holds one `UpstreamPermit` and one `StreamPermit` and nothing else (the request's memory reservation ends at the response headers and the inspection permit at the end of inspection), so long-lived or slow streams cannot exhaust inspection, receipt, or memory capacity. The relay spawns no task: the tasks that exist per stream are the HTTP server's connection task and the HTTP client's connection task, bounded by the stream and upstream permits.

Total decoded string bytes are bounded by `max_body_bytes` (decoded text cannot exceed the wire bytes it came from).

## Memory accounting and composition with the global budget

`resources.capacity.memory_units` is the aggregate budget; one unit is 1 KiB (`admission::MEMORY_UNIT_BYTES`). Before the first body byte is read, a request reserves, for a body of at most `B` bytes:

```
reservation_bytes = 4 * B + min(max_nodes, B) * 128
units             = ceil(reservation_bytes / 1024)
```

The factor 4 covers the received buffer, the decoded strings, the transient duplicate-key sets, and the transformed output reserved for #19 (`admission::BUFFER_COPIES`). 128 bytes per node is a conservative cost for a parsed value or key plus its vector and hash overhead (`admission::NODE_COST_BYTES`); the node count cannot exceed `B` because every node needs at least one input byte. For the 1 MiB maximum this is 6,144 units (6 MiB). `B` is the declared `Content-Length` when one is sent (collection then refuses more bytes than declared), otherwise the effective maximum.

Composition rule: the effective maximum body is the largest `B <= max_body_bytes` whose reservation fits `memory_units` (`RequestLimits::effective_max_body`). A per-request limit larger than the global budget is therefore clamped, never able to exceed it, and a body over the effective maximum is `413 limit_exceeded` rather than a permanent `overload`. A budget too small for any real request makes every request `413`. At runtime the sum of live reservations never exceeds `memory_units` because each is a semaphore permit acquired before collection and held until the parsed request is dropped, including on every error path.

Waiting follows the fixed acquisition order (receipt, then memory) with a bounded queue and `admission_wait_ms`; tokio's semaphore is first-in-first-out, so a large reservation at the head of the queue is not starved by later small ones, and small ones wait behind it for at most the admission wait.

Parsing runs on the request task (no `spawn_blocking`), bounded by `max_body_bytes` and the node/depth budgets. If #5 measurement shows this blocks the reactor too long, parsing moves to the inspection pool; the limits above are unaffected.

## Connection bound (#40; provisional pending quiet-host measurement)

Same configuration object, same status: finite, validated, justified, **not tuned**. Mechanism and rationale: [ADR 0022](../decisions/0022-connection-bound-at-accept.md).

| Field | Provisional value | Ceiling | Rationale |
| --- | --- | --- | --- |
| `max_connections` | 256 | 65,536 | Most connections served at once. One connection per in-flight request (every response closes), so legitimate use is the receipt, upstream, and stream capacities plus connections still sending a head. Measured cost on a loaded host (rerun five times in #42, one-minute load about 6 on 10 cores, same figures): 1 descriptor, about 5 to 7 KiB idle, about 71 KiB with a nearly full held head, one connection task. At 256 that is about 1.4 to 17 MiB. |

Behavior. The bound is checked at accept, before a byte is read: with a free slot the connection is served, otherwise it is closed immediately with no response and the accept loop moves on. Nothing waits and nothing queues, apart from the OS listen backlog, which the accept loop drains continuously. The slot belongs to the connection's IO and is returned only when the server drops it (peer close, head deadline, framing refusal, write stall, `Connection: close`, or shutdown), after the socket is closed. Request outcomes below the bound are unchanged.

Interaction with `resources.capacity.receipt` and the other classes. Connections are a separate budget, not a capacity class. A connection holds no receipt, memory, inspection, upstream, or stream permit until its request head is complete and admission runs, and then acquires them in the existing fixed order (receipt, then memory, ...). The slot is taken first, before any other permit, and nothing waits on it, so there is no lock-order cycle. The two bounds are independent: `max_connections` below `receipt` makes part of the receipt capacity unreachable, and a `receipt` far below `max_connections` leaves connections that are refused at admission with `overload` instead of at accept. Size `max_connections` at least at `receipt + admission_queue + stream` plus headroom for connections still sending a head and for probes, and the process descriptor limit above `max_connections + resources.capacity.upstream` plus a small constant (13 at baseline). The gateway does not read or raise the descriptor limit. Every connection holds one task and up to 64 KiB of head while its head is incomplete.

Health. Not exempt, and not reserved: the decision happens before any byte is read, so a reserve for health would be open to any peer. At the bound a probe is closed without a response and succeeds as soon as a slot frees. Per-peer limits are not implemented: the listener is loopback and every peer shares one address (ADR 0021).

## Request head bounds (#41; measured SDK sizes, provisional caps)

Mechanism, measurements, and rationale: [ADR 0023](../decisions/0023-header-size-measurement-and-size-classes.md). The values were measured against the pinned SDKs (openai Node 7.27.0, Python 3.24.0) and modelled intermediary headers, and **confirmed unchanged**; they stay provisional in the sense of ADR 0008 because intermediary sizes are modelled, not captured.

| Limit | Value | Where | Measured need |
| --- | --- | --- | --- |
| Header names plus values | 16,384 bytes | route (`431 limit_exceeded`) | 437 to 1,013 (SDK alone), about 2.5 KiB typical deployment, 14.9 KiB stacked extreme |
| One header value | 8,192 bytes | route (`431 limit_exceeded`) | 519 largest SDK value (512-byte token), 8,192 for W3C `baggage` at its limit |
| Header fields | 100 | head guard (fixed `431`); equals the HTTP server's own limit | 17 to 19 SDK alone, 31 in the stacked cases |
| Head, request line through blank line | 65,536 bytes | head guard (fixed `431`) | 543 to 1,127 SDK alone, 15,086 stacked extreme; the largest head the route can admit is under 17 KiB |

Memory. A connection holds at most the head bound plus one read chunk (4 KiB) before its head is judged; that is the figure measured in ADR 0022 (about 71 KiB per connection with a nearly full head, about 17 MiB at 256 connections), unchanged here. No permit is held until the head is complete and judged.

## Known gaps

- Connection count is bounded since #40 (`max_connections`, above); the remaining gaps are that the default is provisional, that a refused connection is neither counted nor logged yet, that a local peer can still occupy every slot until its silent connections hit the head deadline (the bound makes that finite, not impossible), and that there is no per-peer bound (loopback peers share an address). The request head is bounded since #25: absolute head deadline, 64 KiB hold bound, then the route's 16 KiB total / 8 KiB per value header limits (`431`). The header sizes are not measured values (ADR 0008).
- Stream limits (#21) and upstream deadlines and response bounds (#20) are provisional.
- The write-stall deadline bounds a response write that stops making progress; read-side stalls are bounded separately: the head by the head deadline (#25) and the body by `body_deadline_ms` (#18); the connection count by `max_connections` (#40). The HTTP server's own write buffer per connection is bounded by the library and is not part of `stream_buffer_bytes`.
- Idle connection pooling to the provider is off, and every local response closes its connection, so every request pays a connection setup on both legs. Measured and decided (#42, [ADR 0024](../decisions/0024-connection-reuse-measurement-and-decision.md)): both stay off; the cost is tens of microseconds locally and about 230 us of CPU for TCP plus TLS 1.3 on a loopback fake (plus about two round trips on a real path, not measured), on a host that was not quiet. No limit changed.
- The request-wide finding bound (`content.max_findings`, default 1024, ceiling 50,000) and the inspection pool sizing (workers `min(inspection permits, CPUs, 16)`, queue `min(inspection permits, 1024)`) are provisional (#19), not measured on a quiet host (ADR 0008). The transformed-output bound is `min(max_body_bytes, bytes covered by the request's reservation)`; the reservation already budgets one output copy.

## Status

Request body, parse, memory-composition, admission wait/queue, and body-deadline limits are implemented in #18 with the provisional values above and boundary tests (`tests/chat_admission.rs`, unit tests in `admission.rs`, `protocol/json.rs`, `chat_route.rs`). Upstream in-flight capacity, response bounds, upstream deadlines, and the shutdown deadline are implemented in #20 with the provisional values above and tests in `src/transport/tests/forward_tests.rs`. Stream capacity wiring and the stream limits are implemented in #21 with the provisional values above and tests in `src/transport/tests/stream_tests.rs` and `src/write_stall.rs`.
