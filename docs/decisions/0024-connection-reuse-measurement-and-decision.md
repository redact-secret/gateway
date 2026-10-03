# ADR 0024: Connection reuse on the local and provider legs: measured, both stay disabled

Status: Accepted; measured, decision recorded (#42). No behavior change. Follow-up of ADR 0017 (provider client has no idle pool) and ADR 0019 (every response is `Connection: close`); uses the cost figures of ADR 0022 and the measurement gate of ADR 0008. Owned by the transport hardening epic (#10). Date: 2026-10-02.

## Context

Since #25 a connection serves one request on the local leg (client to gateway), and since #20 the provider client keeps no idle connections, so every request pays connection setup on both legs. #42 asks for a measurement of that cost and a decision on whether safe reuse is worth it. Reuse would only be re-enabled with the guarantees tested in #25 intact.

## Measurement

Tools (all outside the product binary and outside the qualification seam; the seam allowlist is untouched and `tests/destination_policy.rs` is unchanged):

- `examples/keepalive_cost.rs`: local leg. A raw HTTP/1.1 client against (a) an in-process surrogate server built from the gateway's own stack (axum on hyper on tokio, `http1`), run with keep-alive and with `Connection: close`, and (b) the built release gateway binary, one connection per request as served today (`GET /healthz`; a chat `POST` without a provider is refused before its body is read, so it is not a comparable request). The gateway cannot be asked to reuse a connection, and no experimental patch of it was made; the surrogate arms isolate the connection setup and teardown share for this stack, with none of the gateway's own work, which is an upper bound on what local reuse could save per request.
- `examples/tls_handshake_cost.rs`: provider leg. A loopback TLS server and client in one process built from the crates the gateway already depends on (`tokio-rustls` and `rcgen`, already dev-dependencies, exact pins; `aws-lc-rs` provider as the gateway). It measures TCP and TLS setup against a reused connection, standalone. The qualification build was not used for this measurement.
- `examples/conn_cost.rs` (existing): per-connection descriptors and memory, rerun.
- `qualification/perf/run.mjs` (existing, unchanged): stage timing and peak memory, rerun five times.

Protocol: each configuration five times (`--runs 5`), interleaved across configurations within a run; the tools print every run and a median and spread (min to max over runs of the per-run p50); `uptime`, the load average and the five busiest processes are recorded before and after. A discarded pass warms the thread placement first. Fresh-connection configurations are followed by a pause proportional to the connections they opened, because macOS holds a closed connection for 30 s and the ephemeral port range is about 16k wide; the pause is outside the timed section. Raw data: `docs/qualification/connection-reuse-measurement-2026-10-02.json` and `docs/qualification/alpha1-stage-timing-quiet-host-rerun-2026-10-02.json`.

Pins: Apple M4, 10 cores, macOS arm64 (Darwin 25.5.0), Rust 1.98.1, release profile, core `redact-secret 0.1.0-beta.12`, commit `d85e07d` plus this change's documents. Load generator for the stage timing: Node.js v22.16.0.

**Host state, stated plainly: not quiet.** The brief's criterion is a one-minute load average at or below about 25% of the core count (2.5). It was never met at the start of a run: the load at run start was 4.4 to 8.4 for the TLS microbenchmark (it fell during the run), 4.5 (falling to 2.6 at the end) for the keep-alive tool, 5.7 to 7.6 for the stage-timing reruns, and about 6.3 for the connection-cost reruns. The busiest processes throughout were macOS system services (`StorageManagementService`, `ApplicationsStorageExtension`, `XprotectService`, `syspolicyd`), a spike to a one-minute load of about 22 and 28 to 39 earlier in the session was the same cause, and none was ours. **Every figure below is therefore provisional**, and the long-standing "quiet-host rerun owed" items of ADR 0008 are not closed by this change. What the data does support is stated per row.

A discarded set: the first five stage-timing runs were launched from a wrapper script and showed an extra constant of about 72 ms per request on the client side in every workload (client end to end 73 ms against 0.5 ms, and 280 ms against 235 ms for a stream), while a gateway-internal stage such as inspection and the upstream stages stayed in microseconds. The same command typed directly gave 0.4 ms. The cause was not found (a launch-context effect on the Node load generator; no gateway change is involved), the runs are not used and are not in the data files, and the five reported runs were all launched directly.

### Local leg (loopback; median of the per-run p50, with the min and max over five runs)

| Case | One connection per request (today) | Connection reused | Difference |
| --- | --- | --- | --- |
| Surrogate, 4 KiB body, 1 client: latency | 55.4 us [55.0 to 56.0] | 18.8 us [16.8 to 21.5] | about 37 us |
| Surrogate, 4 KiB body, 1 client: throughput | 17.3k req/s [17.1k to 18.0k] | 44.4k req/s [37.7k to 44.9k] | |
| Surrogate, 4 KiB body, 8 clients: latency | 145 us [139 to 153] | 56.2 us [52.6 to 62.1] | about 89 us |
| Surrogate, 4 KiB body, 8 clients: throughput | 51.0k req/s [49.1k to 54.8k] | 139k req/s [117k to 154k] | |
| Surrogate, 384 KiB body, 1 client: latency | 105 us [95.2 to 113] | 127 us [72.4 to 135] | none distinguishable (ranges overlap) |
| Surrogate, 384 KiB body, 8 clients: latency | 445 us [410 to 509] | 444 us [386 to 472] | none |
| Surrogate, 384 KiB body, 8 clients: throughput | 17.3k req/s [15.4k to 18.8k] | 17.5k req/s [14.5k to 18.3k] | none |
| Gateway binary, `GET /healthz`, 1 client (one connection per request) | 49.8 us [48.5 to 50.2] | not available | |
| Gateway binary, `GET /healthz`, 8 clients | 155 us [146 to 157] | not available | |

Reading: for a tiny request on a loopback with no handler work, a connection costs about 37 us (one client) to about 90 us (eight clients contending), and the throughput ceiling of the transport rises 2.5 to 3 times. With a body of a few hundred KiB the body dominates and no benefit is distinguishable. Against the gateway's own measured work for a 4 KiB request in the same rerun (client end to end 532 us [486 to 728] against an instant fake provider, of which the gateway's total excluding upstream is 355 us), 37 us is a single-digit percentage; against any provider that generates text, whose time is in the hundreds of milliseconds to minutes and which this repository does not measure, it is far below a tenth of a percent. The throughput ceiling is not a constraint either: capacity is bounded by `receipt`, `upstream`, and `stream` permits (single digits to tens in the shipped configurations) and by provider time, not by the transport's request rate.

Per-connection resource cost (`conn_cost`, five runs, load about 6): idle 5.1 to 6.4 KiB resident and one descriptor per connection (6,400 / 5,568 / 5,072 bytes at 64 / 256 / 1,024 connections, spread under 10%), a connection holding a 60 KiB unfinished head 70.9 to 73.0 KiB (17,888 KiB at 256), three threads in all. This reproduces ADR 0022's figures (5 to 7 KiB, about 71 KiB, 17,840 KiB at 256) and leaves `max_connections = 256` as it was: still provisional, because the load was not quiet. Keep-alive would not change the cost of a connection, only how long one is held; a held idle keep-alive connection would occupy a `max_connections` slot and its task for as long as it stays open.

### Provider leg (loopback TLS microbenchmark; both ends in one process)

| Case | p50 per request, median of five runs [min to max] |
| --- | --- |
| TCP connection reused | 17.7 us [16.4 to 24.0] |
| TCP connect, one exchange, close | 57.5 us [57.4 to 58.2] |
| TLS 1.3 connection reused | 24.6 us [17.0 to 27.6] |
| TCP connect, full TLS 1.3 handshake, one exchange, close | 256 us [246 to 257] |
| The same, resuming a session ticket | 187 us [186 to 194] |
| TCP connect, full TLS 1.2 handshake, one exchange, close | 239 us [235 to 246] |

(The 256-byte exchange of a reused connection varies by several microseconds between runs with thread placement; the two "reused" rows are the same wire work and differ only by that noise.) A fresh TLS 1.3 connection costs about 230 us more than a reused one in CPU time of both ends together (so roughly half of it on the gateway's side), plus about 40 us for the TCP part. That is small. **What this tool cannot measure is the network:** on a real path a new connection costs one round trip for TCP and one more for TLS 1.3 (two more for TLS 1.2), so the added first-byte time is about two round trips plus the figure above. The round trip to a provider is not measured here (this repository sends nothing to a real provider), so the added time is a function of a number this ADR does not have. As an illustration only, not a measurement: a 20 ms path would add about 40 ms and a 100 ms path about 200 ms to the time before the request is sent. Real chains are longer than the one-certificate ECDSA chain used here, so real verification costs more CPU than the 230 us. Resumption (a ticket from an earlier connection, which a shared client session store offers even with no idle pool) already shortens the CPU part by about 30% and saves no round trip on TLS 1.3 beyond what a full handshake costs; whether the pinned client resumes sessions was not verified.

### Stage timing and peak memory rerun (qualification build, five runs, load 5.7 to 7.6)

Medians of the per-run p50 (microseconds) with the range over the five runs, beside the earlier single run (load 5.2) in brackets: parse 2 [2.0 to 3.0] (2); core inspection of 4 KiB 47 [47 to 55] (45); core inspection of 349 findings in 16 KiB 331 [328 to 336] (328); core inspection of 384 KiB 2,853 [2,836 to 2,984] (2,735); upstream first response against the plain-HTTP loopback fake 171 [153 to 255] (158); client end to end for 4 KiB 532 [486 to 728] (502). Peak sampled resident memory 9,664 / 10,496 / 11,872 KiB for 4 KiB / 16 KiB many-findings / 384 KiB (earlier 9,632 / 10,464 / 11,120). All 5 x 200, 100, and 20 sequential requests succeeded. The full tables are in the qualification report's "quiet-host rerun" subsection.

## Decision

1. **Local leg: keep disabled.** Every response keeps `Connection: close`; one connection serves one request.
2. **Provider leg: keep disabled.** The client keeps `pool_max_idle_per_host(0)`; every request opens a new connection and is sent exactly once.
3. No default numeric limit changes. No code under `src/` changes. No configuration field is added.

## Reasons

- **The measured benefit is small where it is measurable and absent where requests are large.** Local: tens of microseconds per request, against a request whose provider time is orders of magnitude larger, with no distinguishable gain at body sizes of a few hundred KiB. Provider: about 0.2 ms of CPU for TCP plus TLS on the loopback tool; the part that could matter (two round trips) depends on a path latency this change cannot measure and so cannot justify a change.
- **The cost of reuse is safety work that is not free and not yet designed.** Each guarantee below would need a design and tests of its own, and each reopens a boundary that #25, #43, #40 and #41 closed:
  - Local leg. The head guard sees only the first head of a connection (ADR 0019 item 3 is what lets it). Reuse needs the guard to find where each request ends, which means tracking body framing (`Content-Length`, chunked, `Expect`, upgrade) at the byte stream, a second HTTP framing implementation in front of hyper, which is the class of disagreement that produced the request-smuggling precondition in the first place (ADR 0021 shows the pinned stack exposes no hook for it). It would also need a per-request head deadline, field-count and 64 KiB bound on every request after the first, an idle-between-requests deadline, a bound on requests per connection, and a decision on what an idle reused connection does to the `max_connections` budget (ADR 0022: a slot is held for the connection's whole life).
  - Provider leg. Address policy is applied when a name is resolved at connect time. A pooled connection is not resolved again, so a reused connection outlives the validation (ADR 0013, rebinding), and reuse needs a maximum connection age and idle time short enough to re-resolve, plus a test that a connection older than the bound is not used. A stale pooled connection can fail after the request bytes were written; the policy ("never replay", ADR 0017) is that such a failure is reported and never retried transparently, and whether the pinned HTTP client retries on its own in that case, and under which conditions, would have to be established and pinned by a test before pooling could be enabled. The permit semantics (one upstream permit per request, released on completion or cancellation) hold with a pool only if cancellation closes or discards the connection rather than returning it, since a response abandoned mid-body leaves bytes on the connection.
  - Both legs. No credential or request state may cross requests (the request-local credential type already dies with the request, but a pooled connection and a held parser are new places for state to live), and no unsanitized or partly sent payload may be replayed.
- **Absent a measured benefit, an unmeasured reduction of a hardening property is not acceptable** (ADR 0008: a number enters a default only with a recorded measurement; the same standard applies to a trade that weakens a boundary).

## What would change the decision

Re-open #42's question, with a new ADR, when any of these is shown by measurement and not asserted:

1. A measured round trip to the real provider (taken by the operator with their own credentials and network, since this repository does not run against a real provider) such that two round trips plus the handshake CPU exceed a stated share of the median time to first byte of a representative workload (a proposed criterion: more than 5% at the median for a workload where the model time is short, for example tiny completions at high rate), and a workload for which that matters.
2. A deployment where the local leg is not loopback, or a caller that sends many short requests at a rate where the connection setup is a measurable fraction of the application's latency (not seen in the SDK patterns measured here).
3. A pooled design that passes every test listed under "If reuse is re-enabled" below, and a pinned-client behavior check showing no transparent retry of a sent request.

## If reuse is re-enabled later: tests that must exist first

Per leg, and with the production wiring (not a test-only path):

1. Every request on a reused connection passes the same head vetting as the first: `Content-Length` plus `Transfer-Encoding` (both orders and spellings), the 64 KiB head bound, the field-count bound, and the head deadline measured per request; a second request that arrives pipelined behind the first is vetted, never parsed unvetted.
2. No request or credential state crosses requests (a header, credential, or body from request one is absent from request two on the same connection).
3. Address policy is re-applied over the connection lifetime: a maximum age and idle time, and a test with a resolver whose answer changes to a denied address after the first request: the next request never uses the old connection.
4. A failed reuse of a stale connection is never retried transparently: a counting fake shows exactly one transmission of the request bytes, and the caller gets the documented safe error.
5. Cancellation (client disconnect, shutdown) releases the connection, its permits, and its slot, and does not return a half-used connection to a pool.
6. The connection-bound permit of ADR 0022 is still held for the real lifetime of the connection, and idle reused connections cannot starve new connections of slots (a bound on idle ones).
7. No unsanitized or partly sent payload is ever replayed.

## What this does not claim

- No faster, lower-latency, zero-copy, or superiority claim. The numbers are synthetic, from one provisional host, and describe loopback only.
- The surrogate is not the gateway: it shows the cost of connection setup for this HTTP stack, not the gateway's cost of any request; no reuse arm of the gateway itself exists.
- The TLS figure is CPU on a loopback with both ends in one process and a short certificate chain. It says nothing about a real network, a real chain, or the provider's side.
- The load generator for the stage timing is Node.js v22.16.0 (the suites require 24 or newer; the timing path does not depend on it); the Linux dataset of the earlier report was not repeated.
- Quiet-host measurement of the numeric limits remains owed (ADR 0008) because the host never reached the stated quiet criterion.

## Owner

Transport (`src/transport`, `src/server.rs`, `src/head_guard.rs`); the maintainer approves any change to connection reuse.

## Invariants (unchanged by this decision)

1. A connection serves one request on the local leg (ADR 0019).
2. A request is sent to the provider exactly once, on a connection opened for it (ADR 0017).
3. The connection slot is held for the connection's real lifetime (ADR 0022).

## Verification

This change adds no behavior. The measurements are reproducible with the commands in the tools' headers: `cargo build --locked --release && cargo run --locked --release --example keepalive_cost`, `cargo run --locked --release --example tls_handshake_cost`, `cargo run --locked --release --example conn_cost`, and `sh qualification/build.sh --release` followed by the `QUAL_COMMAND='node qualification/perf/run.mjs'` command in the qualification report. `cargo test --locked` (including `tests/destination_policy.rs`, unchanged) and the existing attack and connection-limit suites continue to pin the one-request-per-connection and no-pooling behavior.

## Deferred measured choices

Quiet-host numbers for every provisional limit (workers, inspection queue, `max_findings`, memory budget, `max_connections`); the real-path provider round trip; any pooled or keep-alive design (only if the criteria above are met).
