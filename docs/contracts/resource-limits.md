# Contract: resource limits

Status: categories approved. **Per-request body, parse, and receipt limits have numeric values (#18) that are provisional pending quiet-host measurement.** Capacity counts (receipt, memory, inspection, upstream, stream) still have no defaults and come from configuration. Deadlines for upstream, idle, and total stream lifetime remain planned (#20, #21). Mechanism: [ADR 0003](../decisions/0003-resource-admission-and-lifetime.md); measurement gate: [ADR 0008](../decisions/0008-performance-measurement-gate.md); decision record: [ADR 0014](../decisions/0014-chat-completions-admission.md).

Every category below is finite, configurable through the validated static configuration, and tested near and across its boundary. A finite per-request limit is never enough alone: there is also an aggregate budget.

| Category | Capacity owner / permit | Notes |
| --- | --- | --- |
| Request bytes | `ReceiptPermit` | Do not trust `Content-Length` |
| Connections / concurrent body receipt | `ReceiptPermit` | |
| JSON depth and node count | `MemoryReservation` | Parsed structure budget |
| Decoded string bytes / inspected text | `MemoryReservation` | |
| Findings | `boundary` | Over-limit means incomplete inspection (reject) |
| Transformed output size | `MemoryReservation` | |
| Aggregate original/parsed/transformed memory | `MemoryReservation` | Held while buffers are live |
| Inspection concurrency and wait queue | `InspectionPermit` | Held until real completion |
| Upstream in-flight requests | `UpstreamPermit` | |
| Active response streams and buffers | `StreamPermit` | Held through stream cleanup |
| Response header and buffer bounds, total bytes | `StreamPermit` | Do not accumulate an entire SSE stream |
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
| `body_deadline_ms` | 10,000 | 300,000 | Time to receive the whole body after capacity is reserved. A 1 MiB body takes milliseconds on loopback; ten seconds tolerates a slow client while bounding how long a stalled one holds its reservation. |

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

## Known gaps

- Connection count is not yet limited before headers are parsed, and there is no header-read timeout, so a connection that never finishes its headers holds a socket but no reservation. Header and idle-connection bounds belong with transport hardening (#25, Alpha 2 #10).
- Findings, transformed-output, upstream, idle, and stream-lifetime limits have no values yet.

## Status

Request body, parse, memory-composition, admission wait/queue, and body-deadline limits are implemented in #18 with the provisional values above and boundary tests (`tests/chat_admission.rs`, unit tests in `admission.rs`, `protocol/json.rs`, `chat_route.rs`). Everything else in the table is planned.
