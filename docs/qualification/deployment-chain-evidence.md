# Deployment-chain evidence for the environment-specific controls

Status: evidence for issue #44 (follow-up of the Alpha 1 attack-boundary suite, #25; epic #11). It closes the "environment-specific" rows of the [threat-control map](alpha1-threat-control-map.md) as far as they can be executed in CI, and says plainly what is only documented. Everything here is synthetic: loopback or `--internal` or `--network none` container networks, a throwaway CA, revoked-looking keys, no real provider, no credential, no payload published. This is not a third-party audit and not a claim about any environment other than the ones recorded below.

## What is executed and what is only documented

| Control (map row) | Localhost companion (Docker, the shipped image) | Kubernetes sidecar (Beta 2, #14) |
| --- | --- | --- |
| 1. Direct upstream bypass (12) | **Executed** (`egress.sh`) | Procedure and policy snippets, **not executed** |
| 2. Intermediary framing conformance (6) | **Executed** (`intermediary.sh`): pinned nginx and pinned HAProxy, 36 cases each | Procedure only, **not executed** (an ingress or mesh sidecar is the same exercise with a different intermediary) |
| 3a. Resolver/DNS answers (5) | **Executed** (`resolver-trust.sh`) | Procedure only, **not executed** |
| 3b. Trust store and TLS-intercepting proxy (5) | **Executed** (`resolver-trust.sh`, throwaway CA) | Procedure only, **not executed** |

Kubernetes is not a supported deployment shape until Beta 2 (#14). The sections marked "procedure, not executed" are a documented procedure to be run when #14 delivers manifests; nothing in them was run, and no result is claimed for them.

The executable part runs in CI (`.github/workflows/deployment-evidence.yml`, on `workflow_dispatch` and on pull requests that touch these scripts or this document). It is deliberately not part of the required `CI passed` aggregate: it needs Docker, pulls digest-pinned images, and its subject is the surrounding environment rather than the gateway's own tests. It uploads its logs as the artifact `deployment-chain-evidence-<sha>`.

## Environment and pins of the recorded run

Recorded by `scripts/deployment-evidence/lib.sh` (`record_environment`) into `environment.txt` of the artifact. The run below is the CI run for the evidence scripts at the commit named; rerun the workflow to refresh it.

| Item | Value |
| --- | --- |
| CI run | [37057716974](https://github.com/redact-secret/gateway/actions/runs/37057716974) (PR #68; checked-out merge commit `ae980d2f03c3ae9127e93758a0a909c4b5122243`) |
| Runner OS | Ubuntu 24.04.5 LTS, kernel `6.17.0-1022-azure`, `x86_64` |
| Docker engine | client 28.0.4, server 28.0.4 (API 1.48, `linux/amd64`), storage driver `overlay2` |
| Gateway under test | the shipped candidate image built from that commit with `cargo build --locked --release` and `scripts/build-image.sh` (no features, no cfg, no test seam, the ADR 0020 qualification build is not used); `redact-secret-gateway 0.1.0-alpha.0`, core `redact-secret =0.1.0-beta.12`, binary sha256 `1f28d92fe5ac6bcf7d87cbb9cca9e2fa07d5d043a16c166c6c17cb12af3f84fb`, image `linux/amd64`, user 65532, base distroless `cc-debian13:nonroot` pinned by digest in `container/Dockerfile` |
| Intermediary 1 | nginx `1.28.3`, image `nginx:1.28-alpine@sha256:a8b39bd9cf0f83869a2162827a0caf6137ddf759d50a171451b335cecc87d236`, config `scripts/deployment-evidence/conf/nginx.conf` |
| Intermediary 2 | HAProxy `3.0.29`, image `haproxy:3.0-alpine@sha256:56b887da77428b7a6621e59e480cdbd330cc805c22d3cedb66ceea76ffdea2c6`, config `scripts/deployment-evidence/conf/haproxy.cfg` |
| Helper (stand-in provider, clients, merge tool) | `python:3.13-alpine@sha256:2dd78ad5cf13a0b68f5134dc49aa9950203a8cf4b7463431b9f3b398287c5059`, Python 3.13.16, stdlib only |
| Image digests | Pinned in `scripts/deployment-evidence/images/Dockerfile` (the single source; Dependabot's docker ecosystem proposes bumps there) |

The scripts also ran, with identical results for every check and an identical per-case framing table, on a developer machine (macOS, Darwin 25.5.0 / Docker Desktop 28.3.3, `linux/arm64`, gateway built for arm64 from the same source). That run is a development cross-check, not recorded evidence.

Nothing in the evidence directory contains a payload, credential, certificate private key, or the candidate binary. Clients print only a status code, the gateway's fixed error code, and counters. The intermediary logs hold request lines of synthetic cases and status codes; `intermediary.sh` fails the run if the synthetic key string appears in them.

## Reproducing

```bash
cargo build --locked --release
sh scripts/build-image.sh target/release/redact-secret-gateway redact-secret-gateway:candidate   # linux/amd64
bash scripts/deployment-evidence/run-all.sh redact-secret-gateway:candidate evidence
```

Needs bash, Docker, and openssl. `run-all.sh` pulls the digest-pinned helper images, records the environment, runs the three scripts, and exits non-zero if any check fails. The scripts use `set -euo pipefail`, wait on conditions (a log line, a counter barrier), and never sleep as synchronization; the only fixed waits are two bounded observations of a negative (the peer left the connection open) and the client read deadline for cases that expect no reply.

## 1. Direct upstream bypass (map row 12)

**Claim to test.** The gateway cannot stop an application from connecting to the provider directly; only the operator's egress control can. The test therefore asserts a property of the operator's policy, and proves nothing about the gateway.

### Localhost companion: executed (`scripts/deployment-evidence/egress.sh`)

Topology (all networks `--internal`, no route out of the Docker host, no Internet):

```
app --- [app net] --- gateway --- [upstream net 93.184.216.0/24] --- stand-in provider (alias api.openai.com)
```

The application container is attached only to the app network, so its only reachable peer is the gateway. The stand-in provider is a TLS server with a throwaway CA, reachable only from the gateway. 93.184.216.0/24 is used because the gateway's address policy requires a public-looking provider address; the network is internal, so no packet leaves the host.

| Check | Result (CI run above) |
| --- | --- |
| Application to gateway to provider (a valid synthetic request) | 200, the stand-in's synthetic answer relayed |
| Direct connect from the application to the provider address, to the metadata address `169.254.169.254:80`, to `1.1.1.1:443`, `8.8.8.8:53`, `10.0.0.1:443` | each fails with `ENETUNREACH` before any packet is sent |
| Application resolves the provider name | fails (`gaierror`), the provider alias exists only on the upstream network |
| Provider counters | exactly 1 connection, from the gateway's upstream-side address; none from the application |
| Negative control: the same direct connect from a container attached to the upstream network (no policy) | connects, and the provider counts it, so the probe can detect a bypass |

**What this proves.** With the application on a network that has no route to the provider, direct connection attempts fail while gateway-routed traffic works, and the probe is shown able to see a bypass. It also shows the supported companion topology: the gateway is the only container with a leg to the provider side.

**What it does not prove.** It is Docker's network isolation, not the gateway, doing the blocking; any other platform needs its own run. It does not cover a compromised host, a container with extra networks or `--network host`, DNS-based exfiltration through a permitted resolver, or exfiltration through the gateway's own legitimate path (the gateway forwards to the provider by design). It is not a statement about an allow-listed egress to provider IP ranges, which is the production form of the control (provider ranges change; the operator owns that list).

### Localhost companion without a container: documented only

A gateway run as a plain host process shares the host network with the application, so the same property needs a host firewall rule keyed on the gateway's user (for example nftables or `pf` allowing outbound TCP 443 only for the gateway's uid and denying the application's uid). Not executed.

### Kubernetes sidecar: procedure, not executed (#14)

Important limit that the procedure must settle first: in a sidecar the application and the gateway share the pod's network namespace, and a `NetworkPolicy` selects pods, not containers. A pod-level default-deny egress policy therefore cannot allow the gateway container while denying the application container in the same pod. The options to evaluate in #14 are (a) per-uid egress rules inside the pod (an init container with `NET_ADMIN` installing owner-match rules, so only the gateway's uid may reach the provider), or (b) running the gateway as its own workload (not the sidecar shape, and a different trust-domain statement than ADR 0009).

Procedure, not executed, for the pod-level layer that is expressible as a `NetworkPolicy` (it denies the whole pod everything except DNS and the provider, which protects against bypass from other workloads and against a compromised application only when combined with (a)):

```yaml
# PROCEDURE, NOT EXECUTED. Illustrative policy snippet for #14.
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: app-with-gateway-egress}
spec:
  podSelector: {matchLabels: {app: app-with-gateway}}
  policyTypes: [Egress]
  egress:
    - to: [{namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: kube-system}}}]
      ports: [{protocol: UDP, port: 53}, {protocol: TCP, port: 53}]
    - to: [{ipBlock: {cidr: 0.0.0.0/0, except: [10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10]}}]
      ports: [{protocol: TCP, port: 443}]   # operators narrow this to the provider's published ranges
```

Steps to run when manifests exist: apply the policy; from the application container attempt TCP 443 to an address outside the allow list, to the node and cloud metadata addresses, and to a pod in another namespace, and record each failure mode; confirm the gateway-routed request still returns the provider answer; run the application as a different uid from the gateway and confirm the owner-match rules (option a) block the application's direct connection while the gateway's connection succeeds; record cluster version, CNI and its version, and the policy engine. Only a CNI that enforces `NetworkPolicy` counts; record which.

## 2. Intermediary framing conformance (map row 6, ADR 0019)

**Claim to test.** If an operator puts an HTTP intermediary in front of the gateway (outside the supported model), what reaches the gateway, and does the combination keep one request per connection and no ambiguous framing? The gateway's own tests cover hostile bytes sent directly; this runs the same forms through two real intermediaries with their default parsers.

### Localhost companion: executed (`scripts/deployment-evidence/intermediary.sh`)

The shipped image runs on an internal network with no provider. Framing and rejection cases need none: a request the gateway admits fails at the upstream step with the fixed `upstream_*` code (`502 upstream_unavailable`), which is how "the gateway admitted this" is observed; a rejected request returns its fixed `malformed_input` or `unsupported_input` code, or an empty-bodied 4xx from the HTTP stack. nginx and HAProxy run with minimal reverse-proxy configurations and no added framing hardening (the evidence is about default parsers). `tools/framing_cases.py` sends each of 36 raw cases on its own connection three ways: directly to the gateway, through nginx, through HAProxy. A sentinel request after each case lets `tools/merge.py` slice the intermediary's single ordered log stream per case without sleeping. Classification:

- **rejected at proxy**: the intermediary answered with its own error (or its log shows its own 4xx) and the gateway was not reached;
- **reached gateway, admitted** or **rejected**: a gateway response came back;
- **normalized**: the gateway admitted the request although the same bytes sent directly are rejected, meaning the intermediary rewrote the framing before the gateway saw it.

Gateway-side invariants asserted every run, directly and pinned to this commit (ADR 0019 and #43): a head with both `Content-Length` and `Transfer-Encoding` (either order) is answered `400 malformed_input` with `Connection: close` before any handler runs; pipelined bytes after the first request are never served (one response, then close); a keep-alive request is answered `Connection: close` and the connection is closed.

Per-case results (CI run above; the full table with log reasons and the raw per-case JSON are in the artifact, `framing/framing-results.md` and `framing-results.json`). "Direct" is what the gateway does with the same bytes sent without an intermediary.

| Case | Direct to gateway | Via nginx 1.28.3 | Via HAProxy 3.0.29 |
| --- | --- | --- | --- |
| c01 baseline `Content-Length` | admitted | admitted | admitted |
| c02 CL then `Transfer-Encoding: chunked` | 400 `malformed_input` | rejected at proxy (400) | **normalized**: admitted |
| c03 TE then CL | 400 `malformed_input` | rejected at proxy (400) | **normalized**: admitted |
| c04 `Transfer-Encoding :` (space before colon) plus CL | 400 `malformed_input` | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c05 `Transfer-Encoding:<TAB>chunked` plus CL | 400 `malformed_input` | rejected at proxy (501) | **normalized**: admitted |
| c06 `Transfer-Encoding: xchunked` plus CL | 400 `malformed_input` | rejected at proxy (501) | rejected at proxy (400) |
| c07 `Transfer-Encoding: chunked, identity` plus CL | 400 `malformed_input` | rejected at proxy (501) | rejected at proxy (400 logged, client saw a close) |
| c08 `TRANSFER-ENCODING: CHUNKED` plus CL | 400 `malformed_input` | rejected at proxy (400) | **normalized**: admitted |
| c09 two different `Content-Length` | 400 | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c10 two identical `Content-Length` | admitted | rejected at proxy (400) | admitted |
| c11 `Content-Length: N, N` | 400 | rejected at proxy (400) | **normalized**: admitted |
| c12 `Content-Length: +N` | 400 | rejected at proxy (400) | rejected at proxy (400) |
| c13 valid chunked body | admitted | admitted | admitted |
| c14 chunk extension | admitted | admitted | admitted |
| c15 chunk trailer | admitted | admitted | admitted |
| c16 chunk size `zz` | 400 `malformed_input` | rejected at proxy (400) | rejected at proxy (400) |
| c17 chunk size over 64 bits | 400 `malformed_input` | rejected at proxy (400) | rejected at proxy (400) |
| c18 chunk lines end in bare LF | 400 `malformed_input` | **normalized**: admitted | rejected at proxy (400) |
| c19 unterminated chunked body, then half-close | 400 `malformed_input` | closed at proxy (truncated request) | closed at proxy (truncated request; forwarding first is timing-dependent) |
| c20 body shorter than `Content-Length`, then half-close | 400 `malformed_input` | closed at proxy (truncated request) | closed at proxy (truncated request; forwarding first is timing-dependent) |
| c21 obsolete line folding | 400 | rejected at proxy (400) | **normalized**: admitted |
| c22 bare CR in a header value | 400 | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c23 bare LF inside the header block | admitted | admitted | admitted |
| c24 NUL in a header value | 400 | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c25 space before colon in `Content-Type` | 400 | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c26 header name with a space | 400 | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c27 `Expect: 100-continue` | 100, then admitted | 100, then admitted | 100, then admitted |
| c28 `Expect: unknown-expectation` | 417 `unsupported_input` | **normalized**: admitted | reached gateway, 417 |
| c29 two pipelined requests | one response, then close | two responses | two responses |
| c30 request then `GET /healthz` in one write | one response, then close | two responses | two responses |
| c31 absolute-form target naming another origin | 400 `unsupported_input` | **normalized**: admitted | rejected at proxy (400 logged, client saw a close) |
| c32 two `Host` fields | admitted | rejected at proxy (400) | rejected at proxy (400 logged, client saw a close) |
| c33 `Host` names the provider | admitted | admitted | admitted |
| c34 HTTP/1.0 request | admitted | admitted | admitted |
| c35 keep-alive request: connection reuse | `Connection: close`, closed | client side: `Connection: keep-alive`, left open | client side: no `Connection` header, left open |
| c36 `Connection: Content-Length` | 400 `malformed_input` | **normalized**: admitted | reached gateway, 400 `malformed_input` |

Summary: nginx rejected 19 of the 36 cases itself, admitted 11 unchanged in outcome, normalized 4 (c18, c28, c31, c36), and closed 2 truncated requests. HAProxy rejected 14 itself (9 of them visible to the client only as a closed connection, with a 4xx in its log), admitted 12 unchanged, normalized 6 (c02, c03, c05, c08, c11, c21), passed 2 through to a gateway rejection, and closed 2 truncated requests.

**What this proves.**

- Neither intermediary, in these versions and with these configurations, delivered a head with both `Content-Length` and `Transfer-Encoding` to the gateway: nginx rejected every such form; HAProxy removed the ambiguity (the gateway admitted what came out, and the gateway answers every head that still carried both with `400`, so those heads did not reach it as both fields). This is an inference from the gateway's recorded behavior, because the gateway keeps no request log and a byte-level tap was not part of this run.
- Pipelining and connection reuse on the client side of an intermediary do not turn into multiple requests on one gateway connection: every request, including the second of a pipelined pair, was answered separately. Each must have arrived on its own gateway connection, because the gateway closes after one response (c29 and c35 directly), so no gateway connection ever carried two requests.
- The `Host` header and an absolute-form target never changed what the gateway forwarded (c31, c32, c33): the same fixed provider route is used (the gateway ignores `Host`; an intermediary that rewrites an absolute-form target to origin-form makes the gateway admit it, which is harmless to destination authority).

**What it does not prove.**

- Anything about another intermediary, another version, or another configuration (for example a CDN, an ingress controller with custom snippets, or a service mesh sidecar). The set is the repeatable conformance test: rerun it for each intermediary an operator actually places in front, and any bump of the pinned images changes the subject (Dependabot proposes the bump; the evidence workflow runs on that PR).
- That the 36 forms are exhaustive. They follow RFC 9112 section 6 and the forms in the control map; they are not a fuzz run.
- That "normalized" is safe by itself. It means the intermediary and the gateway ended up agreeing on one framing for those bytes; it does not show the intermediary's behavior is stable across versions.

**Observed and documented, not changed.** All three parsers (gateway, nginx, HAProxy) treat a bare LF inside the header block as a header separator (c23): `X-Test: a\nX-Injected: b` is two headers everywhere, so there was no disagreement here, and nginx and HAProxy re-serialize the request with CRLF. A stricter intermediary would reject it. The control map's operator requirement ("reject ... bare line feeds") is therefore an operator choice that nginx and HAProxy defaults do not make; whether the gateway should reject bare LF is a product question for #43/the transport owner (see "Issues to consider").

Per-intermediary operator requirement (unchanged, now backed by evidence): reject ambiguous framing, do not share upstream connections between users, do not rewrite `Host` to carry a destination. The supported model remains no intermediary.

### Kubernetes sidecar: procedure, not executed (#14)

For an ingress controller, service mesh sidecar, or gateway-API implementation placed in front of the gateway: deploy it with its production configuration, point `tools/framing_cases.py` at the intermediary's service address and at the gateway's pod address (the script takes any `HOST PORT`), capture the intermediary's log or access-log stream, run `merge.py`, and record the intermediary's exact image digest, version, and configuration. A Kubernetes run needs the sentinel route configured (`GET /__sentinel/<id>` answered by the intermediary itself) or an equivalent per-case log correlation.

## 3. Resolver/DNS answers, trust store, and TLS interception (map row 5, ADR 0013)

### Resolver and DNS answers: executed end to end (`scripts/deployment-evidence/resolver-trust.sh`)

The shipped image runs in a network namespace with no external network (`--network none`) that also holds the TLS stand-in provider. The stand-in listens on loopback and on extra addresses added to `lo` (private, link-local/metadata, CGNAT, benchmark, and one address the policy treats as public). The gateway resolves `api.openai.com` through the container's `/etc/hosts`, which is the system-resolver path the policy resolver uses, bind-mounted per case so each case's answer is exactly what the table says. After each request, a barrier on the stand-in (it drains its accept queues, then waits for in-flight handlers) returns exact counters, so "zero connections" means zero, not "none observed yet".

| Answer for `api.openai.com` | Gateway result | Provider connections |
| --- | --- | --- |
| `127.0.0.1` | `502 upstream_unavailable` | 0 |
| `::1` | `502 upstream_unavailable` | 0 |
| `10.77.0.1` (10/8) | `502 upstream_unavailable` | 0 |
| `172.31.0.1` (172.16/12) | `502 upstream_unavailable` | 0 |
| `192.168.77.1` (192.168/16) | `502 upstream_unavailable` | 0 |
| `169.254.169.254` (link-local, IPv4 metadata) | `502 upstream_unavailable` | 0 |
| `fd00:ec2::254` (IPv6 metadata, ULA) | `502 upstream_unavailable` | 0 |
| `100.64.0.1` (CGNAT) | `502 upstream_unavailable` | 0 |
| `198.18.0.1` (benchmarking) | `502 upstream_unavailable` | 0 |
| `93.184.216.34` and `10.77.0.1` (one public, one private) | `502 upstream_unavailable` | 0 (the whole answer is refused) |
| Control: `93.184.216.34` (public), platform trust store | `502 upstream_tls_failure` | 1 accepted, 1 TLS handshake failed, 0 HTTP requests |

The control row is what makes the zeros meaningful: the same listener, the same gateway, and a public answer produces a connection and a distinct TLS error, so the refusals above were decided before any connect, not by an unreachable address.

Existing unit tests that cover the same policy at the function level (names in the control map, row 5): `src/transport/resolver.rs::disallowed_addresses_are_rejected` (loopback, private, link-local, metadata `169.254.169.254` and `fd00:ec2::254`, IPv4-mapped and compatible, NAT64, ULA, multicast, documentation), `::answers_are_validated_whole_and_pinned`, `::rebinding_second_answer_is_checked_independently`, `::mixed_private_answer_and_empty_answer_reject`; `src/transport/tests.rs::disallowed_addresses_reject_before_any_connection` and `::validation_and_connection_share_one_resolution`. The end-to-end run adds that the shipped binary, resolving through a real system resolver path, refuses the same classes.

**What this proves.** The shipped binary refuses private, loopback, link-local, metadata, CGNAT, and benchmark answers (and an answer that mixes in one of them) with zero connection attempts. **What it does not prove.** It does not show the resolver or network path returns the provider's real addresses: a poisoned answer that points at a different public address is stopped only by TLS hostname verification and can still deny service (ADR 0013). DNS-level controls (a validating resolver, DoT/DoH, split-horizon denial of private answers) remain the operator's. `/etc/hosts` stands in for the resolver; a DNS server returning the same answers goes through the same getaddrinfo path, but that exact setup (a real DNS server in the loop) was not executed.

### Trust store and TLS-intercepting proxy: executed (same script)

The stand-in answers for `api.openai.com` with a certificate signed by a throwaway CA, which is what a TLS-intercepting proxy does. The gateway uses the platform trust store (ADR 0013); in the image that is `/etc/ssl/certs/ca-certificates.crt`.

| Case | Gateway result | Provider |
| --- | --- | --- |
| CA not in the trust store | `502 upstream_tls_failure` | 1 connection, handshake failed, 0 HTTP requests |
| CA appended to the trust store (the image's bundle, sha256 `714d457d580922dbf1d0be8bd35ba236a842b50b0072ae791582a19adef772a5`, plus the throwaway CA, bind-mounted over the bundle path) | `200`, the stand-in's answer relayed | 1 connection, 1 HTTP request |

Operator requirement this evidences (exact failure behavior): a gateway behind a TLS-intercepting proxy fails every request with `502 upstream_tls_failure` and sends no request bytes, unless the proxy's CA is in the gateway's trust store; once installed, the intercepting proxy is trusted completely (no certificate or SPKI pinning exists). Installing a CA is a trust decision the operator owns. Wrong-hostname, self-signed, unknown-CA, and expired certificates against a valid control are covered by `src/transport/tests.rs::tls_positive_control_then_invalid_tls_rejects`; this run adds only the interception scenario against the shipped image.

**What it does not prove.** Trust-store integrity (a compromised root store defeats hostname verification) and each OS's platform verifier behavior beyond the Linux image tested here; macOS, Windows, and revocation behavior were not exercised.

### Kubernetes sidecar: procedure, not executed (#14)

Resolver: from the gateway container, resolve the provider name through the cluster DNS and confirm only public answers; point the name at a private service address, a link-local address, and a node-local address via a test CoreDNS rewrite or a headless-service alias, and confirm `upstream_unavailable` with zero connections at a listener in the pod network (a counting listener as in `tools/tls_stand_in.py`). Trust: mount an additional CA bundle as a ConfigMap over the image's bundle path (or use a derived image), confirm the failure without it and success with it, and record how the cluster's egress proxy or mesh presents certificates. Record cluster, DNS, and CNI versions.

## Limits of this document

- The recorded run is one Linux runner, one Docker engine, two intermediary versions, and the Linux candidate image. Other operating systems, runtimes (Podman, containerd), intermediaries, and a real Kubernetes cluster are not covered.
- No real provider or Internet path was exercised, by design. Behavior against the real provider (anycast, IPv6 provider addresses, real certificate chains) is not evidenced here.
- The stand-in provider is a throwaway TLS server; it proves the gateway's behavior toward certificates and addresses, not provider behavior.
- Results describe the commit named in the environment record. Any change to `src/head_guard.rs`, the transport, or the pinned images changes the subject; the workflow re-records it.

## Issues to consider

- Bare LF inside the header block is accepted by the gateway and by default nginx and HAProxy (c23). Whether the gateway should reject it (it would then fail closed against any intermediary that also treats it as a separator) is a #43 follow-up question.
- A Kubernetes sidecar shares the network namespace with the application, so per-container egress needs per-uid rules or a separate gateway workload; this belongs in the #14 design before manifests are written.
- A byte-level tap (what the gateway actually received) would turn the "ambiguity did not reach the gateway" inference of section 2 into a direct observation; it needs a request-level observation point the product deliberately does not have, so it would live in the harness (a TCP recorder between the intermediary and the gateway).
