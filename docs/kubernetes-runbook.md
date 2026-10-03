# Kubernetes companion operations

Scope: one application and Gateway in one Pod. The
[example](../examples/kubernetes/README.md), [trust/lifecycle ADR](decisions/0035-kubernetes-sidecar-trust-and-probes.md)
and [execution register](qualification/beta2-qualification-report.md) determine
what has actually been qualified. The tested 128/256-MiB request/limit and one-CPU ceiling form a reference profile; production sizing remains provisional.
No registry image is published; do not substitute an unverified image tag.

## Startup and exposure

Validate the exact config with the candidate binary and its mounted token source
before rolling out. `kubectl rollout status deployment/gateway-companion` waits
for the Pod readiness condition; Gateway's native startup probe gates app start.
Probe commands execute the shipped binary on loopback and need no token. A
missing/unreadable token or invalid config prevents a listener and application
startup. Static diagnostics name only error kind and fixed configuration slot.
Check volume ownership/mode and reference names locally; never copy token contents
or environment dumps into logs, support tickets or shell traces.

No Service/ingress is needed. `serve-observed` adds loopback `GET /metrics`;
ordinary `serve` does not. Scrape from a trusted same-Pod application/helper
using a bounded local request to `http://127.0.0.1:8787/metrics`. Do not publish
this endpoint by port-forward/ingress on shared interfaces. The proxy caller
token does not authorize remote operations access. Responses are aggregate JSON
version 1 with fixed names, no request labels; values reset on process restart.
Health is not a provider-network check and reveals no credential/config state.

## Operating signals and responses

| Observation | Interpretation | Action |
| --- | --- | --- |
| `401 local_auth_required` / `local_auth_invalid` | Local token is missing/wrong/ambiguous; no body inspection or upstream contact | Check app header setup against the mounted restart-activated source; never substitute provider `Authorization` |
| `422 unsupported_input` / limit or incomplete inspection | Entire request rejected before forwarding | Check the reviewed endpoint subset and declared limits; never bypass Gateway with the original request |
| `503 overload`, admission `waiting` or occupied memory/inspection | Finite capacity or queue exhausted | Reduce concurrency/payload, inspect quota throttling, compare measured profile; avoid unbounded SDK retries |
| Upstream first-response/total time increases | Provider/network wait, distinct from scan time | Check bounded network/TLS/DNS outcomes and provider health externally; Gateway does not retry |
| Stream downstream wait and buffered bytes increase | Slow reader/backpressure | Read/cancel promptly, check write/lifetime limits; do not collect a full SSE stream in Gateway |
| `abandoned`, idle/lifetime/buffer/shutdown stream ends | Stream did not finish through the relay | Require endpoint-specific provider terminal signal in the SDK; no exception alone proves completion |
| Liveness failures during saturation | Probes share connection/CPU limits | Investigate throttling and finite connection occupancy before tuning thresholds; aggressive restart can duplicate provider work |
| Cgroup throttling or OOMKilled | CPU/memory budget exceeded | Inspect safe cgroup/Pod counters, lower load or use a measured larger profile; process kill cannot promise cleanup/drain |

Stages use microseconds. `inspection` includes worker wait and serialization;
`serialization` is nested, so adding both double-counts. Histogram pairs are
noncumulative upper bound/count; percentile bounds can overestimate by up to
25%. Stream `completed` counts provider EOF/relay completion, not semantic SDK
success. Shared admission occupancy is a point-in-time relaxed read; OS RSS and
actual running workers require Linux/cgroup observation, not inference from it.
Counters aggregate both endpoints. There are no request IDs, per-caller labels,
raw findings, scoring exports or payload traces.

## Drain, restart and rollback

App retries always retain the loopback base URL and local token header. There
is no raw/direct-provider fallback. Keep app shutdown finite: the example's
30-second grace includes app stop, Gateway's 10-second drain and one-second
cancellation grace. SIGTERM stops acceptance, then drain/cancel; a truncated SSE
cannot be interpreted as a fabricated completion. SIGKILL/OOM/crash has no drain
promise, and any provider work already delivered remains delivered.

Use versioned ConfigMap/Secret names, validate, change the Pod template and roll
out. Mounted-file updates do not hot-reload the process. Token rotation needs a
restart and updated app token source. Follow the
[upgrade/rollback contract](contracts/config-upgrade-rollback.md): restore the
compatible binary, config and token references together. Rolling back observed
mode to an older binary requires changing `serve-observed` back to `serve`.

The basic sidecar permits direct app egress. Ordinary NetworkPolicy cannot
separate colocated app/Gateway permissions. The optional UID operator profile
requires its own documented NET_ADMIN init container and actual denial tests
through restarts/replacements. App UID 10001 may connect only to TCP loopback port 8787; other loopback services, DNS and metadata are denied. The report records those tests on both native architectures. Installer failure must hold application startup;
never continue a mandatory-egress claim after a failed enforcement test. Node,
runtime, privileged administrators and other trusted token readers remain inside
the deployment assumptions. No all-CNI or all-cluster support is implied.
