# Contract: resource-limit categories

Status: categories approved; **no numeric defaults are set.** Numbers need measurement ([ADR 0008](../decisions/0008-performance-measurement-gate.md)). Mechanism: [ADR 0003](../decisions/0003-resource-admission-and-lifetime.md).

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
- A number is published as a default only with a recorded measurement and pins.

## Status

Planned. This file deliberately contains no numbers.
