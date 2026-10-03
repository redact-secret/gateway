# One-application Kubernetes companion

Implementation example; cluster/platform/resource qualification is in progress
under epic #14. See [ADR 0035](../../docs/decisions/0035-kubernetes-sidecar-trust-and-probes.md).
Requires Kubernetes native sidecars (tested environment will be Kubernetes 1.34).

Build the candidate locally without publishing it. Render `GATEWAY_IMAGE_DIGEST`
and `APPLICATION_IMAGE_DIGEST` to verified image digests available to your
cluster. No distributable registry image exists. Load unpublished candidates
into kind for qualification; a local tag is a test-only transport, and the report
must record its actual image ID and binary hash.

Create the ConfigMap with `kubectl create configmap gateway-config
--from-file=config.json=examples/kubernetes/config.json`. Supply a generated
local token as a Secret from a private mode-0600 file with
`kubectl create secret generic gateway-local-token --from-file=token=<private-file>`.
The examples never contain a live token or provider key. Both containers mount
the token read-only; kubelet exec probes need no credential. Your application
must read the mounted token and send `X-Gateway-Local-Token` independently of its
provider `Authorization`; SDK base URL alone does not add that header. Keep SDK
retries bounded and never switch to a direct provider URL after gateway failure.

Apply the rendered `sidecar.yaml` in the intended namespace. Gateway's native
sidecar startup probe gates application start. There is no Service or ingress;
proxy and health endpoints listen on Pod loopback only. Exec probes run the
shipped binary, not shell/curl. Probe processes count against the container's
CPU/memory and share Gateway's connection bound; transient saturation can make
a probe fail. Requests/limits are provisional pending #93 measurements.

Config/token changes require a validated restart. Updating a mounted Secret does
not reload a running process; prefer versioned ConfigMap/Secret names and roll
out a Pod-template change. Roll back image AND compatible config/token references
as described in [the rollback contract](../../docs/contracts/config-upgrade-rollback.md).

The 30-second Pod termination grace includes app shutdown followed by Gateway's
10-second drain plus one-second cancellation grace. Give the app a finite
shutdown budget of less than 19 seconds. SIGKILL/OOM cannot promise graceful
drain; partial SSE is incomplete and delivered provider data cannot be retracted.

This basic manifest permits direct app egress. A Pod-level NetworkPolicy cannot
permit Gateway provider egress while refusing it to its colocated app. Mandatory
traversal needs the separately qualified operator profile in ADR 0035; until its
tests pass that protection is not supported. No NET_ADMIN or privileged container
is needed for this basic installation. The app/Gateway and token readers are one
trust domain; node/runtime administration is trusted.
