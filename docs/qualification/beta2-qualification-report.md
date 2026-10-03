# Beta 2: native sidecar and operational qualification

Status: passed for the recorded native amd64/arm64 environments. Epic #14; children #89–#95. No registry publication or all-cluster support claim.

## Executed evidence

- [Native Kubernetes/cgroup/600-second soak run](https://github.com/redact-secret/gateway/actions/runs/37147921349).
- [Native binaries/images and independent OCI consumer run](https://github.com/redact-secret/gateway/actions/runs/37147921313).
- Full Rust CI, real-binary CLI (11), pinned Node/Python SDK suites, shipped examples and OCI integrity tests (2) pass on the qualified revision.
- zizmor 1.30.1 offline audit: five workflows, no findings. Scorecard not run without its token; live repository rulesets are absent.

The following JSON/text datasets are committed for durable review; GitHub artifacts alone expire. Source commits are PR merge-checkout revisions, with exact per-run pins in the datasets. Artifact and sidecar workflows build separately: ELF hashes are compared below, while each image retains its own recorded identity.

| Architecture | Dataset | Source commit | Candidate ELF SHA-256 | Artifact ELF match |
| --- | --- | --- | --- | --- |
| amd64 | [amd64](beta2/amd64/environment.json) | `4ba7052214cb7a5139f4cda36d80e53f07ef451f` | `dd890ac5ab323bbf72e24124fc6cbeb8dcce3e4c6ea3be0ec5811da2aacb2a93` | yes |
| arm64 | [arm64](beta2/arm64/environment.json) | `4ba7052214cb7a5139f4cda36d80e53f07ef451f` | `a7bc8903b2cd030522e9272fec1c00323d27cb2a084031f1aacaf5537e59c72e` | yes |

Artifact source `4ba7052214cb7a5139f4cda36d80e53f07ef451f`; core `=0.1.0-beta.12` with registry checksum `2cc951e8b991e9ec27343a872627f53a25b252cc8148cb4ec7160192ed9d856d`; Rust 1.98.1; Node openai 7.27.0 and Python openai 3.24.0. [Manifest](beta2/manifest.json) records the three binaries, two native image archives and named `candidate` multi-platform OCI layout. [OCI consumer evidence](beta2/oci-index-info.json) records Skopeo 1.13.3 selecting exact amd64/arm64 configurations without registry access. Image-extracted ELF bytes match each native smoke-tested binary. macOS ARM64 remains independently smoke-tested.

## Work sequence

#89 trust/lifecycle → #90 native deployment/probes → #92 enforcement/resolver/TLS → #93 quota/budget → #94 soak/recovery → #95 operations and handoff. #91 native artifacts runs alongside the deployment work. Earlier Alpha/Beta 1 epics remain completed inputs.

## Scope and reproduction

Qualification covers native Linux amd64 and arm64 on GitHub's Ubuntu 24.04
runners, kind 0.30.0, pinned Kubernetes 1.34.0, bundled kindnet and containerd,
cgroup v2 and one application per Pod. The committed environment datasets
record the actual node/CNI images, host kernel, node resources, source commit,
Rust/toolchain, Cargo lock hash, core `=0.1.0-beta.12`, schema 1, candidate ELF
hash and candidate/qualification image IDs. This is a scoped rehearsal, with
host load recorded and `quiet_host: false`; it certifies neither every CNI nor
production model capacity. The load client is Python stdlib `http.client` from
the digest-pinned helper, rather than an SDK. The independently passing Beta 1
SDK workflow retains its exact Node/Python pins.

Reproduce with the `Beta 2 sidecar evidence (native, unpublished)` workflow on
this revision, or install the pinned native kind tool and execute the workflow's
build and `qualification/sidecar/run.py` commands. `BETA2_SOAK_SECONDS=600` is
declared before execution; workflow dispatch permits 600–1800 seconds. Secrets,
CA private keys and request/response fixtures are ephemeral, excluded from the
aggregate evidence directory and destroyed with the dedicated test cluster.

Production candidates and non-release workload builds are distinct. Real shipped
ELFs/images execute probes, local-auth rejection, the shipped Deployment,
identity, actual resolver/TLS policy and OOM/recovery. Accepted workload JSON/SSE
uses the separate fake-provider qualification build; no route override or CA
control enters distributed artifacts. The TLS experiment mounts a temporary
operator CA in the ordinary OS trust store and aliases destination IPs inside
the isolated Pod; no production trust-policy bypass is introduced. Docker #44
remains an independent [deployment-chain report](deployment-chain-evidence.md).

## Threat/control results

| Threat or failure | Recorded experiment | Scoped result |
| --- | --- | --- |
| Sidecar placement mistaken for mandatory traversal | Basic profile directly calls the same-Pod provider | Direct connection succeeds; basic manifest has no mandatory-egress claim |
| App bypass of operator profile | UID 10001 probes Pod IPv4, synthetic ULA IPv6, 127.0.0.1:9000 and [::1]:9000 | All denied; trusted UID 20001 IPv6 control succeeds; mediated requests succeed |
| Incomplete enforcement at startup | Drop NET_ADMIN from the initial installer | Installer fails and neither Gateway nor app starts |
| Unavailable/invalid Gateway | Invalid schema and absent token file | Native sidecar never opens readiness; app never starts |
| Restart gap | Continuous four-destination probes during active Gateway SIGTERM and SIGKILL | Zero direct-provider reachability; readiness and mediated traffic recover |
| Replacement/rollout | New enforced Pod and real Deployment rollout with active JSON/SSE | New installer completes before app; denied direct traffic and successful mediated traffic recover |
| Unsafe resolver answers | Loopback v4/v6, RFC1918, metadata v4/v6, CGNAT, benchmark and mixed public/private | Rejected before TLS connection; zero provider request bytes |
| TLS trust or hostname failure | Public address with untrusted CA or wrong hostname | Connection attempted, zero HTTP requests, no plaintext fallback |
| Explicit operator trust | Same public address with private CA appended to OS roots | Synthetic HTTPS request succeeds; local caller token never relayed |
| Header/auth confusion | Missing, wrong, duplicate local token and duplicate Authorization | Fixed rejection, zero provider delivery; existing full header/duplicate-key suite passes |
| OOM | Lower a ready production Gateway's own cgroup hard limit, then allocate through an authenticated request | Exit 137 plus increased kernel oom_kill counter; runtime reason recorded separately; same-Pod restart and new enforced Pod recover |
| Retry ambiguity | Two explicit client attempts whose provider work outlives header deadlines | Two provider deliveries, zero Gateway automatic retries; no exactly-once claim |
| SSE truncation | Stalled provider, slow reader, cancellation, active termination/replacement | No fabricated terminal signal; ordinary short streams carry real provider terminals |

The mandatory profile is optional administrator configuration, separate from the
unprivileged basic manifest. Its initial NET_ADMIN installer permits app UID
10001 only TCP 127.0.0.1:8787/[::1]:8787, rejecting all other ports/destinations
including DNS, metadata and other loopback services. Gateway UID 65532 stays
subject to the existing resolver/address/TLS policy. Root/SETUID/NET_ADMIN in the
app, passed sockets, another trusted UID acting as proxy, compromised Gateway,
node/runtime administration and privileged CNI are outside this protection.
The trusted fake-provider observer creates a positive control, not extra app
permission. No ordinary Pod NetworkPolicy separation is claimed.

## Resource and latency interpretation

Each architecture executes 33 ten-second scenarios: CPU limits 250m, 500m and
1 CPU; safe text parameter 1 KiB/16 KiB/64 KiB with 1/8/32 clients; plus 100
revoked synthetic findings and a 64-property dense schema at eight clients.
`text_bytes` is the requested text-size parameter, not encoded JSON body bytes;
findings replace that text and dense schemas add their own structure. Endpoints
alternate Chat/Responses and share admission/inspection/upstream/stream budgets.
Memory request/limit is 128/256 MiB; capacity is receipt 8, inspection 2,
upstream 8, stream 8 and 65,536 reservation units. Load configs use 2-second
drain, 3-second stream idle/header, 15-second stream lifetime/upstream total and
2-second downstream write-stall; the shipped Deployment independently executes
its default 10-second drain and 30-second Pod grace configuration.

Client round-trip percentiles include local HTTP and synthetic provider cost.
Datasets report both all attempts and successful-only percentiles; fast 503
responses must not improve a claimed success latency. Production histogram
before/after deltas distinguish admission wait, parse, inspection, serialization
and upstream wait. Inspection includes worker queue and serialization, so
nested stage quantiles must not be added. These figures are not model/network
latency, nor a subtraction benchmark establishing isolated Gateway overhead.
Cgroup cpu.stat provides quota usage and throttling; proc samples provide RSS,
OS threads and descriptor counts without reading payloads or process memory.

The existing worker policy remains min(inspection permits, quota-aware available
parallelism, fixed maximum). Two observed OS threads do not mean two inspection
workers. A one-CPU qualification profile therefore does not justify increasing
workers beyond quota. Lower quotas show throttling/overload; reduce client
concurrency before raising workers or queue lengths. No global default or worker
change is made. Both HTTP connection legs remain unpooled under ADR 0024.

Reservation units, RSS and cgroup memory are different measurements. The
65,536-unit logical allowance is 64 MiB (1 KiB per unit) but does not preallocate that memory.
Bounded metrics serialization adds at most 128 KiB per response (32 MiB at the
256-connection bound) outside proxy reservations, alongside runtime/allocator/
core and probe costs. The measured workload is smaller than every theoretical
maximum; a 256-MiB container can fail closed or OOM on larger aggregate demand.
The 128-MiB request/256-MiB limit/one-CPU ceiling is a tested reference profile,
not a guarantee that every simultaneously admitted maximum fits that limit.
Production sizing and quiet-host release SLOs remain Beta 3 inputs.

## Soak, operations and release handoff

The 600-second soak repeatedly mixes JSON, both completed SSE protocols,
cancellation and slow readers, then cools down. The predeclared acceptance
bound is RSS growth at most max(8 MiB, 20% of the measured warm baseline),
descriptors at most warm +4, unchanged OS thread count and zero admission
occupancy. These are measured rehearsal bounds, not a universal leak proof.
SIGTERM allows finite configured drain/cancellation; SIGKILL and OOM promise no
drain. Provider work delivered before cancellation cannot be retracted.

`serve-observed` opts into loopback-only fixed-key version-1 JSON metrics. Ten
stage histograms, seven stream-end counters, two local-auth rejection counters,
upstream attempts, buffering and shared admission have bounded cardinality.
Worst-case histogram serialization remains below 128 KiB; unknown queries and
methods get fixed JSON/no-store errors without echo. Health/probes need neither
config nor credentials, and never contact a provider. Metrics grant no proxy
authority. Trusted same-Pod observers are the exposure assumption; no Service,
remote scrape listener or shared gateway is introduced.

The [operations runbook](../kubernetes-runbook.md),
[sidecar example](../../examples/kubernetes/README.md),
[ADR 0035](../decisions/0035-kubernetes-sidecar-trust-and-probes.md) and
[ADR 0036](../decisions/0036-loopback-operations-export.md) define startup,
non-root identity, permissions, static token rotation, overload signals, bounded
shutdown and coordinated binary/config/token rollback. Unknown defaults,
production provider behavior, broader clusters/CNIs and maximum aggregate
capacity remain explicitly unqualified. #15 owns final stable-candidate
reconciliation, signing, SBOM, provenance, dependency review and any publication.
No registry upload or final stable release is performed by Beta 2.

## Measurements

### amd64: quota and resource observations

| CPU limit | Maximum sampled RSS (MiB) | Cgroup memory peak (MiB) | Max descriptors | OS threads | Throttled periods / periods |
| --- | ---: | ---: | ---: | --- | --- |
| 250m | 10.21 | 9.40 | 36 | [2] | 1109 / 1206 |
| 500m | 9.73 | 9.63 | 33 | [2] | 868 / 1192 |
| 1 | 9.79 | 10.29 | 33 | [2] | 240 / 1205 |

Representative safe-text parameter 16 KiB, ten seconds per scenario:

| CPU | Clients | HTTP 200 | HTTP 503 | Success p50/p95/p99 (ms) | All-attempt p95 (ms) |
| --- | ---: | ---: | ---: | --- | ---: |
| 250m | 1 | 3277 | 0 | 1.36/2.68/51.17 | 2.68 |
| 250m | 8 | 2467 | 808 | 6.83/83.82/95.19 | 83.00 |
| 250m | 32 | 2169 | 2547 | 24.15/102.22/128.32 | 98.80 |
| 500m | 1 | 6475 | 0 | 1.34/2.02/3.29 | 2.02 |
| 500m | 8 | 5221 | 1237 | 5.87/53.51/62.92 | 52.89 |
| 500m | 32 | 4797 | 3316 | 21.31/64.47/84.38 | 64.15 |
| 1 | 1 | 6528 | 0 | 1.35/2.05/2.98 | 2.05 |
| 1 | 8 | 9573 | 1890 | 5.72/10.16/13.41 | 9.86 |
| 1 | 32 | 8195 | 2181 | 20.70/41.82/56.11 | 41.00 |

| CPU (16 KiB, 8 clients) | Mean parse (µs) | Mean inspection including queue (µs) | Mean nested serialization (µs) | Mean upstream wait (µs) |
| --- | ---: | ---: | ---: | ---: |
| 250m | 53.16 | 4500.39 | 13.76 | 8971.00 |
| 500m | 12.22 | 1765.90 | 13.57 | 6077.48 |
| 1 | 11.92 | 777.61 | 13.71 | 3446.92 |

Soak: declared 600 seconds; actual 604.6 seconds including final cooldown. RSS warm/recovered 8444/8364 KiB (allowance 8192 KiB); descriptors 11/11; threads 2/2. All final shared-admission occupancy is zero.

OOM exit 137, runtime reason `OOMKilled`, kernel oom_kill delta 1; same-Pod and new-Pod recovery pass. Active TERM/KILL restart results and continuous egress samples are in [restart.json](beta2/amd64/restart.json). All 13 resolver/TLS cases pass in [resolver-tls.json](beta2/amd64/resolver-tls.json).

### arm64: quota and resource observations

| CPU limit | Maximum sampled RSS (MiB) | Cgroup memory peak (MiB) | Max descriptors | OS threads | Throttled periods / periods |
| --- | ---: | ---: | ---: | --- | --- |
| 250m | 8.59 | 10.23 | 33 | [2] | 1103 / 1184 |
| 500m | 9.12 | 11.08 | 36 | [2] | 847 / 1183 |
| 1 | 8.66 | 10.09 | 37 | [2] | 198 / 1182 |

Representative safe-text parameter 16 KiB, ten seconds per scenario:

| CPU | Clients | HTTP 200 | HTTP 503 | Success p50/p95/p99 (ms) | All-attempt p95 (ms) |
| --- | ---: | ---: | ---: | --- | ---: |
| 250m | 1 | 3985 | 0 | 1.38/1.94/42.19 | 1.94 |
| 250m | 8 | 4213 | 550 | 4.57/76.28/79.27 | 76.06 |
| 250m | 32 | 3526 | 2952 | 16.61/88.17/96.63 | 86.00 |
| 500m | 1 | 7144 | 0 | 1.30/1.58/2.19 | 1.58 |
| 500m | 8 | 8867 | 1035 | 4.29/42.46/48.60 | 42.01 |
| 500m | 32 | 7727 | 3837 | 15.39/49.02/59.09 | 48.42 |
| 1 | 1 | 6904 | 0 | 1.34/1.66/2.25 | 1.66 |
| 1 | 8 | 13813 | 1405 | 4.22/6.91/8.74 | 6.80 |
| 1 | 32 | 11685 | 2975 | 15.05/27.93/34.93 | 27.07 |

| CPU (16 KiB, 8 clients) | Mean parse (µs) | Mean inspection including queue (µs) | Mean nested serialization (µs) | Mean upstream wait (µs) |
| --- | ---: | ---: | ---: | ---: |
| 250m | 23.47 | 2168.21 | 6.91 | 8803.57 |
| 500m | 11.86 | 850.31 | 6.87 | 4432.98 |
| 1 | 7.87 | 416.96 | 7.23 | 3065.24 |

Soak: declared 600 seconds; actual 613.2 seconds including final cooldown. RSS warm/recovered 7744/7744 KiB (allowance 8192 KiB); descriptors 11/11; threads 2/2. All final shared-admission occupancy is zero.

OOM exit 137, runtime reason `OOMKilled`, kernel oom_kill delta 2; same-Pod and new-Pod recovery pass. Active TERM/KILL restart results and continuous egress samples are in [restart.json](beta2/arm64/restart.json). All 13 resolver/TLS cases pass in [resolver-tls.json](beta2/arm64/resolver-tls.json).
