# Contract: resource limits

Status: categories approved. **Per-request body, parse, and receipt limits have numeric values (#18) that are provisional pending quiet-host measurement.** Capacity counts (receipt, memory, inspection, upstream, stream) still have no defaults and come from configuration. **Upstream connect, response-header, and total deadlines, response header and body byte bounds, and the shutdown drain deadline have provisional values (#20, below).** **Stream idle and lifetime deadlines, the write-stall deadline, and the per-stream relay buffer bound have provisional values (#21, below).** **The accepted-connection bound has a provisional value (#40, below).** **Aggregate load, memory accounting against the reservation, failure behavior at each limit, and recommended provisional capacities are in "Aggregate load qualification" (#58, below); no default or formula changed.** **Aggregate load, memory accounting against the reservation, failure behavior at each limit, and recommended provisional capacities are in "Aggregate load qualification" (#58, below); no default or formula changed.** Mechanism: [ADR 0003](../decisions/0003-resource-admission-and-lifetime.md); measurement gate: [ADR 0008](../decisions/0008-performance-measurement-gate.md); decision record: [ADR 0014](../decisions/0014-chat-completions-admission.md).

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

Implemented for tool definitions and schemas (#54: the same derived counters, schema depth 8, 256 schema objects per schema, 64 tools, 64 properties, 64 `required`, 64 enum entries, 8 `anyOf` entries) and for tool-call arguments (#53: request-wide derived node and decoded-byte counters, argument depth 8, 32 tool calls per message); and for metadata (#55: 16 entries, 64 byte keys, 512 byte values, checked at parse and again after redaction, charged against the same derived counters); Alpha 2 contract, #52, [ADR 0025](../decisions/0025-alpha2-field-contract.md)): request-wide derived budgets for strings decoded out of tool arguments and for schema trees (nodes, decoded bytes, depth 8), at most 64 tools, 32 tool calls per message, 64 schema properties, 64 enum entries, 8 `anyOf` entries, 16 metadata entries; all provisional.

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
| `stream_write_stall_ms` | 30,000 | 600,000 | Longest a pending response write may make no progress before the connection is closed. A local consumer that is alive reads in milliseconds; thirty seconds tolerates a paused one while bounding how long a dead one holds a stream permit and a provider connection. Applies to every accepted connection. Since #59 the total time a connection's writes may spend blocked is also capped at `stream_lifetime_ms` ([ADR 0027](../decisions/0027-write-budget-and-bounded-response-frames.md)), so a consumer that reads just fast enough to avoid this deadline is bounded on a buffered JSON response too, not only on a stream. |
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

## Aggregate load qualification (#58): capacities and behavior at each limit

Evidence: the [Alpha 2 aggregate load qualification](../qualification/alpha1-qualification-report.md#alpha-2-aggregate-load-qualification-2026-10-03-58) subsection and the datasets [alpha2-aggregate-load-2026-10-03.json](../qualification/alpha2-aggregate-load-2026-10-03.json) and [alpha2-memory-accounting-2026-10-03.json](../qualification/alpha2-memory-accounting-2026-10-03.json); decision: [ADR 0028](../decisions/0028-aggregate-load-qualification.md). **Every figure is provisional**: no pass met the quiet-host condition (one-minute load average at most 25% of the cores; it was 5.2 to 44.0 at the start of each of five passes on a 10-core machine). Nothing here is a default; no default numeric limit and no formula changed.

### Failure behavior at each limit (verified under combined load)

| Limit | What happens at the limit | Caller sees | Upstream body |
| --- | --- | --- | --- |
| `max_connections` (accept time) | Connection closed at accept, nothing read or queued; the slot returns when the connection closes | Closed or reset connection (SDK connection error, retried by default) | none |
| `max_body_bytes`, `max_findings`, depth, nodes, strings, messages, tool and schema bounds | Fixed local rejection before the send (the whole request on the finding bound) | `413 limit_exceeded`, `422 unsupported_input`, `400 malformed_input` | none |
| `receipt` and `memory_units` | Join the wait queue (at most `admission_queue`, 16) for at most `admission_wait_ms` (250); a full queue or an expired wait fails | `503 overload`, `Retry-After: 1` | none |
| `inspection` permits (queued plus running jobs) | Refused at once, no waiting; the pool queue is `min(inspection, 1024)` and cannot fill past the permits | `503 overload`, `Retry-After: 1` | none |
| `upstream` / `stream` permits | Refused at once (`try_*`) after inspection returned its permit | `503 overload`, `Retry-After: 1` | none |
| Deadlines, shutdown, caller leaves | See [request-lifecycle](request-lifecycle.md); a started inspection keeps its permit and reservation until the core call really returns | per stage | none before the send |

Measured under combined load (24 clients mixing eight body shapes, a third of provider calls held 150 ms, 6 stream readers that stop reading, 40 idle sockets, finite capacities): every occupancy peaked at its capacity and never above it (inspection 4 of 4, receipt 8 of 8, memory 16,384 of 16,384 units, upstream 8 of 8, stream 4 of 4, wait queue 16 of 16, sockets 74 to 75 against a bound of 64 plus listeners and provider connections); every counter was back to zero within 1 to 2 ms of the load ending; ten of ten probe requests then succeeded; the provider received no call that a forwarded request did not explain and no body holding a synthetic credential shape; five kinds of pre-forward rejection (finding bound, body bound, unsupported field, malformed JSON, duplicate key; 50 requests per run) produced zero provider calls in all five runs.

### Memory: reservation against observed bytes

The formula `4*B + min(max_nodes,B)*128` (1 KiB units) held for every Alpha 2 shape. Resident growth as a fraction of the reserved bytes (median over five runs, range in brackets), eight requests held at the provider at once, release build, growth measured after a warm-up request:

| Shape | Body | Reserved per request | Held growth / reserved | True peak growth / reserved |
| --- | --- | --- | --- | --- |
| Tool definitions (64 tools, 2048 properties) | 826 KB | 5.1 MiB | 0.68 [0.61 to 0.72] | 0.68 [0.62 to 0.72] |
| Tool history (48 turns, 4 calls each, nested JSON arguments) | 661 KB | 4.5 MiB | 0.60 [0.56 to 0.62] | 0.61 [0.57 to 0.62] |
| Node-dense schemas (about 15k parsed nodes) | 85 KB | 2.3 MiB | 0.50 [0.49 to 0.51] | 0.51 [0.50 to 0.52] |
| Large text (500 KiB) | 512 KB | 4.0 MiB | 0.33 [0.32 to 0.42] | 0.34 [0.32 to 0.42] |
| Metadata (16 x 512 byte values) | 8 KB | 1.0 MiB | 0.14 [0.12 to 0.14] | 0.16 [0.15 to 0.17] |
| Many findings (16 KiB, 349 tokens) | 16 KB | 2.1 MiB | 0.12 [0.10 to 0.12] | 0.12 [0.11 to 0.13] |

By stage (retained products, one fresh process each, median of five): the received body costs 1.0x its bytes; the parsed typed request with decoded text 1.0x for plain text, 1.3x for tool history, 1.9x for tool definitions and 11.4x for node-dense schemas (about 66 bytes per node against the 128 charged); the sealed transformed output 1.0x to 1.25x. Body plus parsed plus output is at most 0.62 of the reservation in these shapes. Findings are transient inside the worker and are covered by the end-to-end peak.

Combined load, growth over a warm baseline (the core warmed on every worker before measuring), as a fraction of the sampled peak of reserved bytes: 0.52 [0.33 to 0.60] with the tight 16,384-unit budget, 0.40 [0.31 to 0.51] with the 262,144-unit budget. The 0.60 is the smallest headroom seen (1.7x). The factor 4 and the 128 bytes per node stay.

**Not covered by `memory_units`; add when sizing a host:**

- First use of the core on each inspection worker thread: about 7 MiB per worker, one time (cold process 8 MiB, 36 MiB with four warmed workers). Measured from a cold start, the combined-load growth looked like twice the reservation; this fixed cost was the difference.
- Buffered provider responses: `upstream x max_response_body_bytes` (16 held replies of about 4 MB cost 4.3 to 4.5 MiB each).
- Stream relay buffers: at most `stream x stream_buffer_bytes` (193 bytes observed).
- Connections: about 5 to 7 KiB idle, about 71 KiB with a nearly full head ([ADR 0022](../decisions/0022-connection-bound-at-accept.md)).
- Allocator retention: resident memory did not return to its pre-load level after any pass.

### Recommended provisional capacities (one application, up to a few agents; not defaults)

| Setting | Recommended (provisional) | Why, and what the data does not show |
| --- | --- | --- |
| `inspection` | 4 (workers `min(permits, CPUs, 16)`) | One job per worker, no waiting. Single-job time on this host: 11 ms median for 500 KiB with 900 findings, 13 to 18 ms for slot-dense Alpha 2 bodies (tool definitions, node-dense schemas, tool history), p99 up to 82 ms under combined load. At 4, the extreme load served about 600 of 17,000 requests and refused the rest at once. Size to the application's real parallelism; the excess is `503` by design. |
| `memory_units` | 65,536 (64 MiB) at least; 262,144 where eight maximum-size bodies (6,144 units each) must be in flight | Eight maximum bodies need 49,152 units. A 16,384 budget exhausted under 24 clients and queued or refused as designed. Resident growth stayed below the reserved bytes. |
| `receipt` | 16 | At least 2 to 4 times `inspection`, so bodies arrive while jobs run. |
| `upstream` | 16 | Add 16 x 4 MiB of unreserved response buffering in the worst case. |
| `stream` | 16 | Streams hold no other permit; 193-byte buffers observed, bound 16 MiB. |
| `max_connections` | 256 (unchanged) | Above receipt plus upstream plus stream plus connections still sending a head. At 64 with 40 idle holders, about 13,000 of 17,000 legitimate connections were closed at accept: do not set it near the expected concurrency. |
| `content.max_findings` | 1,024 (unchanged) | 900 findings in 500 KiB took 11 ms median; 1,300 were rejected (`413`) with no upstream call. |

Large against small requests: with a 6,000-unit budget, six clients sending 500 KiB bodies and ten sending 4 KiB bodies, every request of both classes was served (592 of 592 large, 1,173 of 1,173 small, no `503`); small requests waited behind the first-in-first-out reservation queue (median 41 ms against 3.9 ms alone; admission wait p95 65 ms, bounded by the 250 ms wait). Alone, the small class was refused for 7% of requests by the immediate inspection limit; mixed with large ones it was not, because the memory queue paced arrivals. No scheduling change.

Test limitations: one machine; load generator and fake provider on the same host; loopback only, no TLS; immediate or fixed-delay provider replies; a single-thread gateway runtime; macOS allocator behavior; no pass met the quiet-host condition; the Linux dataset comes from a shared runner only. Stage percentiles under load are histogram bucket edges (at most 25% above the true value).

## Known gaps

- Connection count is bounded since #40 (`max_connections`, above); the remaining gaps are that the default is provisional, that a refused connection is neither counted nor logged yet, that a local peer can still occupy every slot until its silent connections hit the head deadline (the bound makes that finite, not impossible), and that there is no per-peer bound (loopback peers share an address). The request head is bounded since #25: absolute head deadline, 64 KiB hold bound, then the route's 16 KiB total / 8 KiB per value header limits (`431`). The header sizes are not measured values (ADR 0008).
- Stream limits (#21) and upstream deadlines and response bounds (#20) are provisional.
- The write-stall deadline bounds a response write that stops making progress; read-side stalls are bounded separately: the head by the head deadline (#25) and the body by `body_deadline_ms` (#18); the connection count by `max_connections` (#40). The HTTP server's own write buffer per connection is bounded by the library and is not part of `stream_buffer_bytes`. A buffered JSON response is handed to the server in frames of at most 16 KiB, so its upstream permit is held until all but that bounded window has been accepted by the socket (#59, [request-lifecycle](request-lifecycle.md)).
- Idle connection pooling to the provider is off, and every local response closes its connection, so every request pays a connection setup on both legs. Measured and decided (#42, [ADR 0024](../decisions/0024-connection-reuse-measurement-and-decision.md)): both stay off; the cost is tens of microseconds locally and about 230 us of CPU for TCP plus TLS 1.3 on a loopback fake (plus about two round trips on a real path, not measured), on a host that was not quiet. No limit changed.
- The request-wide finding bound (`content.max_findings`, default 1024, ceiling 50,000) and the inspection pool sizing (workers `min(inspection permits, CPUs, 16)`, queue `min(inspection permits, 1024)`) are provisional (#19), not measured on a quiet host (ADR 0008). The transformed-output bound is `min(max_body_bytes, bytes covered by the request's reservation)`; the reservation already budgets one output copy.

## Status

Request body, parse, memory-composition, admission wait/queue, and body-deadline limits are implemented in #18 with the provisional values above and boundary tests (`tests/chat_admission.rs`, unit tests in `admission.rs`, `protocol/json.rs`, `chat_route.rs`). Upstream in-flight capacity, response bounds, upstream deadlines, and the shutdown deadline are implemented in #20 with the provisional values above and tests in `src/transport/tests/forward_tests.rs`. Stream capacity wiring and the stream limits are implemented in #21 with the provisional values above and tests in `src/transport/tests/stream_tests.rs` and `src/write_stall.rs`.
