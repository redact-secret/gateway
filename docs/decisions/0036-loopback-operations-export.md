# ADR 0036: Opt-in loopback operations export

Status: Accepted under epic #14 implementation delegation (#95).
Implementation status: implemented; qualification in progress.
Date: 2026-10-03. Amends ADR 0030 decision 10 only for the explicitly opted-in mode.

`serve <config>` retains its existing routes. `serve-observed <config>` additionally
exposes `GET /metrics` on the same **numeric loopback** listener. Non-loopback
configuration is refused before binding even when proxy token auth is enabled.
No schema keys or absent-key semantics change; an older binary rejects the new
CLI command. Rollback uses `serve` with the same compatible config. The Kubernetes
example opts in; Docker's existing entrypoint remains `serve`.

Operations authority is same-Pod/host loopback reachability, independent of the
proxy token. The token neither enables telemetry nor grants proxy authority via
a scrape. A scrape reads no request body, token, provider key, config file,
reference name, URL label or finding. It cannot forward. Queries and non-GET
methods reject; HEAD is explicitly denied. There is no external metrics listener,
admin mutation, per-client identity or remote observability service.

The version-1 JSON snapshot reuses the production `Metrics` atomics and shared
`Admission`. Ten fixed stage names, seven fixed stream-end names and two fixed
auth outcomes bound cardinality. Each histogram has at most 252 upper-bound/count
pairs; buckets are noncumulative, microseconds, with at most 25% quantization
overestimate. The output is capped at 128 KiB; failure returns a fixed overload
body. Scrapes share the connection/head/write/deadline bounds and allocate no
queue, background job or persistent labels. There is no raw error/request trace.
Responses carry `no-store`. At the provisional 256-connection cap, worst-case
retained export bodies add up to 32 MiB; parsed snapshot/serialization scratch
is finite and generated synchronously on the reactor. Resource measurement must
include scrapes and probe processes. Do not expose this route on shared networks.

The snapshot aggregates both endpoints. Counters reset on restart and relaxed
reads are not one transactional snapshot. `completed` means provider EOF and
relay completion, not an SDK/provider semantic terminal event. Inspection time
includes worker queue and serialization; do not sum nested stages as independent
Gateway overhead. Provider and consumer wait stages remain separately named.
Occupancy is permits held, not OS RSS or actual running worker count. No socket
count, per-route outcome matrix, cgroup statistic or CPU utilization is fabricated;
collect OS/cgroup evidence externally. Existing APIs remain available for tests.

Tests execute the real binary to prove opt-in routing, fixed-key JSON, query
rejection without request labels and unchanged ordinary serve. Maximum histogram
cardinality/output size is tested independently. Proxy no-forward, auth isolation
and qualification-seam absence gates remain required.
