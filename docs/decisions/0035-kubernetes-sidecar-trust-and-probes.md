# ADR 0035: Same-Pod sidecar trust, exec probes and mandatory egress

Status: Accepted design under the maintainer's epic #14 implementation delegation.
Implementation status: exec probe implemented; manifests and environment qualification in progress (#89–#95).
Date: 2026-10-03. Builds on ADR 0030, 0032, 0033 and 0024.

## Decision and scope

One application and Gateway share one Pod network namespace. Proxy access binds
`127.0.0.1:8787`; both supported endpoints require the local caller token. Only
these two containers mount that Secret. Provider credentials belong to the app
and remain independent. Config and token are resolved once at startup. Nothing
adds remote/shared service support or changes schema v1.

Use Kubernetes native sidecars: Gateway is an init container with
`restartPolicy: Always`, a startup exec probe and ordinary liveness/readiness
exec probes. The shipped binary implements `probe live|ready <numeric-loopback-address>`.
It uses fixed unauthenticated health paths, no DNS, no config or credentials,
a two-second absolute deadline and a 1024-byte response ceiling. Exit 0 means
HTTP 200 and the exact frozen health body; exit 1 means unavailable or invalid
response; exit 2 means invalid invocation. Kubelet exec reaches loopback without
opening proxy access on Pod IP or assuming shell/curl inside distroless.
Probe failure under connection saturation remains possible (ADR 0022); readiness
can remove a Pod but does not cancel work. Liveness thresholds must tolerate
short overload; capacity tuning belongs to #93.

The default Gateway runs UID/GID 65532, app UID 10001, with no capabilities,
no privilege escalation, read-only root filesystem, RuntimeDefault seccomp,
no host networking/path, no API permissions and no service-account token.
Use fsGroup 65532 and Secret mode 0440 so the two authorized containers can
read it; other containers must not mount it. A common group is an intentional
trust grant. No host administrator or runtime compromise is in this boundary.

Native sidecar startup gates application start until Gateway is ready. During
Pod termination Kubernetes stops regular app containers before native sidecars;
Gateway clears accepting state on SIGTERM and drains for configured
`shutdown_drain_ms`, then cancels and waits at most one additional second.
Example: 10-second drain and 30-second Pod grace, leaving 19 seconds for app
termination and scheduling. App shutdown must fit that remainder; Kubernetes
can SIGKILL after the shared grace. Crashes/OOM/SIGKILL do not promise drain.
SDK base URLs stay loopback on every retry/restart; no direct-provider fallback.

## Placement versus enforcement

The basic shape inspects traffic sent through Gateway. It does **not** prevent
the app from making a direct provider connection. NetworkPolicy grants apply
to the entire Pod: permitting provider egress for Gateway permits it for the
app too. kindnet supplies networking, not NetworkPolicy enforcement. No YAML
policy claim substitutes for an observed denied connection.

An optional, separately tested operator profile may run an initial NET_ADMIN
installer before the native Gateway sidecar and application. Its own dedicated
operator image and administrator privilege are outside the default install.
The installer inserts IPv4 AND IPv6 OUTPUT owner rules: app UID 10001 may use
loopback; every other destination, including DNS and metadata, is rejected.
Gateway UID 65532 uses the reviewed resolver/address/TLS policy. Installer
failure must prevent app startup. Container restarts retain sandbox rules;
new Pods rerun the installer before either workload starts. No process with
NET_ADMIN, SETUID, root, host access or another UID may be launched by the app.
All outbound sockets must be created under UID 10001; passed/preopened sockets,
other same-Pod proxy services and privileged debugging are outside the profile.
The falsifiable requirement is: direct app IPv4/IPv6 egress fails while mediated
traffic succeeds, including startup/restart/replacement. Until #92 records
these results, mandatory traversal is **unqualified**. UID enforcement cannot
protect against compromised Gateway, node/runtime admin or privileged CNI.

A separate Gateway workload would require another ADR for caller identity,
network trust and local-hop confidentiality. It is not an alternative supported
by this ADR. No transparent interception, shared tenancy or Helm is introduced.

## Environment and evidence gates

First qualification target: kind v0.30.0, Kubernetes v1.34.0,
`kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a`,
bundled kindnet/containerd; Linux amd64 and native Linux arm64 measured separately.
Record actual CNI image digest, kernel, runtime and cgroup mode in each run.
Basic IPv4 loopback is the example; Gateway does not listen on IPv6 in it.
Mandatory enforcement must deny IPv6 bypass too; an unavailable ip6tables
backend fails installation. No all-cluster/CNI certification follows.

Exact production candidate evidence covers config/startup/probes/rejections,
identity and termination without provider credentials. Accepted JSON/SSE and
attack traffic use the separate qualification build and synthetic fake provider;
its route/trust seams never enter shipped artifacts. Resolver private,
loopback, metadata, unspecified and multicast answers must be rejected, TLS
certificate/hostname failures must not fall back, and redirects remain disabled.
There is no HTTP intermediary in this topology. Reuse Docker #44 separately.

## Ownership and invariants

`cli` owns the bounded probe; `server` remains sole readiness/drain owner;
`telemetry` owns aggregate counters; deployment examples grant no new proxy
or credential authority. All existing duplicate-key, auth-before-body,
complete-inspection and no-forward invariants remain unchanged. Connection
reuse remains disabled (ADR 0024). Numeric resource requests are provisional
until #93; mandatory egress and platform support stay unqualified until their
actual target evidence is archived. Signing/SBOM/provenance remain #15;
registry publication is not authorized.

## Verification

`tests/config_cli.rs` executes the real binary against the served health router,
refuses remote/hostname/zero-port arguments, rejects false/oversized responses,
and verifies refusal after process termination. The environment evidence matrix
must record startup, invalid config/token, saturation, drain and recovery.
