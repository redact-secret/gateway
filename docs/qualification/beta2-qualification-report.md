# Beta 2 qualification: execution register

Status: implementation and qualification in progress; no production or
all-cluster support claim. Parent #14; children #89–#95. Native sidecar design:
[ADR 0035](../decisions/0035-kubernetes-sidecar-trust-and-probes.md). Aggregate
operations export: [ADR 0036](../decisions/0036-loopback-operations-export.md).

## Work sequence and evidence boundaries

| Issue | Depends on | Deliverable | Completion gate |
| --- | --- | --- | --- |
| #89 | #12 contracts, Docker #44 | Same-Pod trust, placement versus mandatory egress, exact environment, probes/lifecycle ADR | Reviewed contract and explicit unqualified enforcement gate |
| #90 | #89, implemented #62/#63/#13 | Native sidecar manifest, bounded distroless exec probe, security and lifecycle | Actual candidate Pod/probe/identity/drain evidence |
| #91 | implemented baseline | Native Linux ARM64 binary/image and unpublished multi-platform OCI bundle | Actual native executions, identical image binary hashes, OCI descriptors |
| #92 | #89/#90/#91 | Kubernetes UID egress, resolver/address/TLS and startup/restart tests | Recorded threat/control matrix; no transient bypass or seam in candidate |
| #93 | #90/#91, Alpha 2 #58 | Explicit cgroup quota/memory load measurements | Exact hardware/config/core/client pins, stage timing and memory reconciliation |
| #94 | #90/#92/#93 | Declared soak and failure/restart/replacement tests | Measured baseline thresholds, finite drain, recovery and no fabricated completion |
| #95 | #89/#90/#93/#94 | Existing telemetry export, runbook and support/evidence handoff | Fixed cardinality/exposure and accurate evidence matrix |

#91 starts alongside #89. #90/#92 establish deployment before quota measurement;
#93 precedes soak thresholds, then #94/#95 finish the epic. Completed Alpha/Beta 1
issues are inputs and are not reopened. Publication and final signing/SBOM/
provenance remain #15 and later release gates.

## Evidence register

| Layer | Test | Status |
| --- | --- | --- |
| Host binary | Real-binary probe, opt-in operations, fixed-key snapshot and query refusal (`tests/config_cli.rs`) | Passed locally, 9 tests |
| Operations bound | Maximum ten-stage histogram cardinality and 128-KiB serialization ceiling | Execution pending full suite |
| OCI assembly | Platform selection, exact blob hashes, refusal of duplicate architecture and bad rootfs hash (`scripts/test-oci-bundle.py`) | Passed locally, 2 tests |
| Workflow audit | zizmor 1.30.1, offline audit, all five workflows | No findings; Scorecard skipped (no `GITHUB_AUTH_TOKEN`), no live rulesets |
| Cluster setup | kind 0.30.0, pinned Kubernetes 1.34.0 node, kindnet, containerd 2.1.3, LinuxKit 6.10.14, Docker Desktop amd64 | Created locally; initial Ready timeout, later node Ready; no workload claim yet |
| Native artifacts | Candidate artifacts workflow on amd64/arm64 Linux and macOS ARM64 | Pending actual execution |
| Native sidecar/cgroup | `Beta 2 sidecar evidence (native, unpublished)` workflow | Pending actual execution |
| TLS/resolver/OOM matrix | Kubernetes additive attack/recovery evidence | Pending; Docker #44 remains independent |

Never infer a passed workload from a Ready node, applied YAML, successful build,
emulated execution or a passing unit test. Actual native run URLs, source/core/
config/client/platform pins, digests and aggregate datasets must be appended
before related issue closure. The exact candidate startup/rejection evidence is
separate from accepted traffic against the non-release qualification binary.

## Measurement method

The new native harness runs CPU limits 250m, 500m and 1 CPU with 256-MiB memory,
1-KiB/16-KiB/64-KiB text, mixed Chat/Responses and 1/8/32 clients. These are
qualification inputs, not recommended release capacities. Both protocols share
core/admission/transport permits. HTTP round-trip p50/p95/p99 includes provider
and local transport; the production stage histograms distinguish admission,
parse, inspection (includes worker queue/serialization), upstream wait and
stream relay. Do not sum nested stage quantiles or call model latency Gateway
overhead. Linux proc/cgroup sampling records RSS, threads, descriptors, memory
current/peak/limit/events and CPU quota/usage/throttling. OS observations and
permit reservations represent different quantities.

A hosted runner and the local Docker Desktop host are not quiet-host evidence.
All measured figures stay provisional; no global defaults or worker/reuse policy
changes follow from them. Native CPU-quota evidence must justify any future
worker setting. ADR 0024 keeps both connection legs unpooled.

Soak is declared at 600 seconds by default before execution (600–1800 in the
workflow). After workload warmup, permissible RSS growth is max(8 MiB, 20% of
measured baseline); descriptors must return within four and thread count must
match. Those thresholds are scoped rehearsal gates, not a universal leak proof.
All admission occupancy must return to zero after cleanup. Repeated cancellation,
slow readers and mixed requests accompany before/after samples. SIGTERM and
SIGKILL are separate and active-stream terminal absence must be recorded;
provider bytes already delivered cannot be retracted. Explicit OOM and resolver/
TLS cases remain required before the epic is complete.
