# ADR 0022: Connection bound enforced at accept time

Status: Accepted; implemented (#40). Closes the "connection count is not limited" residual recorded by ADR 0019 (#25). Wider transport hardening (header-size measurement, HTTP/2 policy) remains #10. Date: 2026-10-02.

## Context

After #25 every connection is bounded in time (head deadline, write-stall deadline, body deadline) but not in number. A peer that opens many sockets holds a file descriptor, one connection task, and up to 64 KiB of held request head each, until the head deadline closes it. No receipt or memory capacity is reserved for such a connection, so request admission is unaffected, but process resources are not bounded.

## Decision

1. **One configured bound.** `resources.limits.max_connections` (integer `1..=65536`, provisional default 256) is the most connections served at once.
2. **Enforced at accept, by immediate close.** `StallListener` (`src/write_stall.rs`), the innermost layer of the production listener (`server::guarded_listener`), takes one non-blocking `try_acquire` on a semaphore of that size for every accepted socket. No slot: the socket is dropped (closed) on the spot, the accept loop yields and accepts the next. There is no wait, no queue, and no task. The only queue left is the kernel's listen backlog, which is bounded by the OS and holds connections that completed the TCP handshake but were not yet accepted; the accept loop drains it continuously, so nothing waits there under a flood.
3. **The slot is held for the connection's real lifetime.** The permit is a field of the connection's IO object (`StallIo`), declared after the socket so it is returned after the socket closes. The HTTP server owns the IO until the connection ends, whatever ended it: peer close or reset, head deadline, the head guard's fixed 400/431 refusal (ADR 0021; the slot outlives the refusal write and is returned when the connection actually closes), write stall, `Connection: close` after the response, or shutdown. No code path returns a slot early or late by hand, so no path can leak one.
4. **A separate budget.** Connections are not a capacity class and do not consume `resources.capacity.receipt`, memory, inspection, upstream, or stream permits. Acquisition order is connection slot (at accept, before any byte is read) then receipt, memory, and the rest in the existing fixed order (ADR 0003) once a request head is complete. The slot is never acquired while holding any other permit and nothing waits for a slot, so there is no cycle and no deadlock. A connection does not hold receipt or memory until its head is complete (ADR 0019).
5. **Health is not exempt.** The decision to refuse is made before a byte is read, so health traffic cannot be told from any other. A reserved slot for health would be reachable by any peer that sends `GET /healthz`, which only moves the attack. At the bound a probe is refused like every other connection (closed without a response) and succeeds as soon as a slot frees; a refused probe is a failed probe, so a supervisor treats a gateway at its connection bound as unready, which is accurate: it is not accepting. Size the bound so ordinary traffic plus probes sit well below it. Operators who need an unconditional liveness signal should use a process-level check, not the HTTP listener.
6. **No per-peer bound.** The listener is loopback unless the operator acknowledges non-loopback exposure (ADR 0009, unsupported), and every loopback peer shares one source address, so a per-address count would be the global bound under another name. A sidecar's peers are indistinguishable by address; per-peer limits would need a caller identity, which is a Beta 1 item (local caller token). Revisit with that.
7. **Default (provisional).** 256. Rationale below. It is a conservative ceiling chosen so the worst case is small in absolute terms, not a tuned or recommended value.

## Rationale for the default and the measurement

Legitimate connections are one per in-flight request, because every response is `Connection: close` (ADR 0019): at most receipt-waiting requests, upstream calls, and streams, plus connections still sending a head. For the configurations shipped here (capacities of 8 at most, `admission_queue` 16) that is well under 100; 256 leaves large headroom while keeping the worst case bounded. Cost per connection, measured with `examples/conn_cost.rs` (spawns the release binary, opens N connections, samples the gateway process with `ps` and `lsof`):

| Phase | Descriptors per connection | Resident memory per connection | Tasks | Threads |
| --- | --- | --- | --- | --- |
| idle (connected, nothing sent) | 1 | about 5 to 7 KiB | 1 (axum spawns one connection task; by construction, not observable externally) | unchanged (3 in total) |
| fat head (60 KiB of an unfinished head sent; the guard holds at most 64 KiB) | 1 | about 71 KiB | 1 | unchanged |

At 256 connections: 256 descriptors, about 1.4 MiB idle and about 17 MiB (17,840 KiB measured) with every connection holding a nearly full head. At the ceiling of 65,536 the worst case would be about 4.5 GiB held head bytes, which is why the ceiling is documented as a fd-space bound, not a recommendation, and why the default is far lower. The process descriptor limit must exceed `max_connections` plus a small constant (13 were open at baseline) plus the provider connections (`resources.capacity.upstream`); the gateway does not read or raise the limit.

Host and conditions (honest): macOS arm64 (Darwin 25.5.0), release build of commit 2e724db plus this change, `ulimit -n` 1048576, **host load average 38.90 / 37.88 / 23.17 (heavily loaded, not a quiet host)**, one run, `ps` RSS (point-in-time, whole process, page-granular). The descriptor counts are exact; the memory numbers are coarse and not a bound. Rerun on a quiet host with `cargo build --locked --release && cargo run --locked --release --example conn_cost` and replace the default with a measured one then (ADR 0008). Until then the default is provisional like the other limits. Rerun for #42 (five runs, [ADR 0024](0024-connection-reuse-measurement-and-decision.md), one-minute load about 6 on 10 cores, so still not quiet): idle 5.1 to 6.4 KiB and fat head 70.9 to 73.0 KiB per connection, one descriptor each, three threads, 17,888 KiB at 256 fat heads; the figures reproduce and the default stays 256, provisional.

## What this does not claim

- It does not distinguish peers; one local process that opens `max_connections` sockets can occupy every slot until the head deadline (`body_deadline_ms`, provisional 10 s) closes the silent ones. The bound turns an unbounded resource drain into a bounded, self-healing denial of service for that period; it does not prevent denial of service by a local peer. Loopback is not caller authentication (README).
- A refused connection receives no response (no `503`); an SDK sees a connection error and its own retry policy applies (SDK retries are status-based and connection errors are retried by default, see errors and telemetry).
- Connections still inside a request that has been admitted (receipt, upstream, streams) are bounded by their own capacity classes; this bound does not replace them.
- There is no counter or log for refusals yet; adding one needs the telemetry design in #10.

## Owner

`transport`-adjacent connection handling in the server module; the maintainer approves changes to the bound's semantics.

## Invariants

1. A connection is either admitted (holds exactly one slot until its IO is dropped) or closed at accept; nothing waits for a slot.
2. A slot is never held by anything that is not a live connection, and never released while its socket is open.
3. No receipt, memory, inspection, upstream, or stream permit is acquired by, or depends on, the connection slot.
4. Below the bound, request outcomes are identical to an unbounded server (`tests/connection_limits.rs`).

## Verification

`tests/connection_limits.rs` (real loopback sockets through `bind` and `serve`): the bound reached with silent connections and returned by the head deadline; slow-head connections holding slots until closed; abandoned connections (64 per round, three rounds) never leaking a slot, checked by an exact "at least N and at most N slots" probe; health and readiness at the bound and after; identical outcomes below the bound; shutdown with the bound reached and exceeded returns within the drain plus the fixed grace. Unit tests in `src/write_stall.rs` for the permit lifetime through a write stall. Config tests in `src/config.rs` (default, ceiling, strictness).

## Deferred measured choices

The default number (quiet-host measurement, ADR 0008); a refusal counter; per-peer limits (needs caller identity).
