# Candidate artifacts (expanded through Beta 2, unpublished)

Status: implemented (#7 scaffold; extended to the Alpha 1 MVP candidate in #22 and native ARM64/OCI in #91). Governing decision: [ADR 0012](decisions/0012-release-candidate-artifact-build.md). Nothing described here is published or distributable.

The artifacts contain the cumulative Alpha/Beta implementation: `--version`, `validate-config`, a loopback server with `/healthz` and `/readyz`, and `POST /v1/chat/completions` for the OpenAI Chat Completions text subset and, since the Beta 1 work, `POST /v1/responses` for the stateless OpenAI Responses text subset (#86; the same pipeline and relay bound to the Responses contract, fixed route `openai.responses`). Both proxy routes strictly admit, inspect through the pinned core, forward once to the fixed OpenAI route and relay JSON and SSE with responses not redacted. Opt-in `serve-observed` adds bounded loopback `GET /metrics`, and `probe live|ready` executes fixed health checks. Other routes are rejected locally with 404 `unsupported_input`. The smoke checks post only requests that are rejected before inspection and forwarding (for both proxy routes, `scripts/probe-endpoints.sh`), so even a config with an upstream cannot reach a provider; CI never contacts a provider. The pinned Node/Python SDK suites are run against a **separate non-release test build**, never against these artifacts ([ADR 0020](decisions/0020-sdk-qualification-test-build.md)).

## Target matrix

| Artifact | Platform | Built and smoke-tested on |
| --- | --- | --- |
| `redact-secret-gateway-x86_64-unknown-linux-gnu` | Linux x86_64 (glibc) | `ubuntu-24.04` |
| `redact-secret-gateway-aarch64-unknown-linux-gnu` | Linux ARM64 (glibc) | native `ubuntu-24.04-arm` |
| `redact-secret-gateway-aarch64-apple-darwin` | macOS ARM64 | `macos-15` (Apple silicon) |
| `redact-secret-gateway-candidate-image-linux-amd64.tar` (`docker save` of `redact-secret-gateway:candidate`) | linux/amd64 OCI image | `ubuntu-24.04` with Docker |
| `redact-secret-gateway-candidate-image-linux-arm64.tar` | linux/arm64 OCI image | native `ubuntu-24.04-arm` with Docker |
| `redact-secret-gateway-candidate-image-linux-multiarch.oci.tar` | named `candidate` amd64/arm64 OCI layout | exact native variants; independently selected by Skopeo |

Not included: Windows, macOS x86_64, Helm and npm/PyPI launchers. Kubernetes manifests are repository examples, not OCI layers. Each binary is built once with `cargo build --locked --release` (no features, no cfg); the image copies the Linux binary and its smoke test checks the bytes match.

## Smoke checks (run on the exact candidate bytes, on their own platform)

| Check | Linux binary | macOS binary | Image |
| --- | --- | --- | --- |
| Qualification-seam absence (`scripts/check-no-qualification-seam.sh`: no marker, flag, or test-constructor symbol of the test build) | yes | yes | yes (binary extracted from the image) |
| `--version` has the expected shape | yes | yes | (same bytes, checksum-compared) |
| `validate-config` on every shipped example config | yes | yes | the image's own config, with the image's binary |
| Start on loopback, `/healthz` and `/readyz` 200 | yes | yes | yes, through a host-loopback published port |
| Proxy-route probes for `POST /v1/chat/completions` (seven kinds) and `POST /v1/responses` (store omitted, `previous_response_id`, hosted tool, malformed arguments and the same status classes), all rejected locally with the documented status and safe code (no credential 401, `{}` 422, unknown field plus a synthetic token 422 with no echo, wrong content type 415, `GET` on the route 405, unknown route 404) | yes | yes | yes |
| Graceful SIGTERM, exit 0, `shutdown complete` | yes | yes | yes (`docker stop`) |
| Non-root (uid 65532), read-only root, no capabilities, no new privileges | n/a | n/a | yes |
| Compose example: `docker compose config`, start, ready, probes, down | n/a | n/a | yes, against the candidate image |

## Build locally

```bash
cargo build --locked --release                      # binary for your host
sh scripts/smoke-binary.sh target/release/redact-secret-gateway examples/config.openai.json /tmp/rsg-evidence
# Linux x86_64 host with Docker only:
sh scripts/build-image.sh target/release/redact-secret-gateway redact-secret-gateway:candidate
sh scripts/smoke-image.sh redact-secret-gateway:candidate "$(sha256sum target/release/redact-secret-gateway | cut -d ' ' -f 1)" /tmp/rsg-image-evidence
sh scripts/smoke-compose.sh examples/compose/compose.yaml /tmp/rsg-compose-evidence
```

The authoritative candidate is built by the `Candidate artifacts (Alpha 1, unpublished)` workflow (`.github/workflows/artifacts.yml`, on `workflow_dispatch` and on pull requests touching build files). Local builds do not match the CI bytes (different toolchain host and paths); bit reproducibility is not verified.

## What the workflow produces

One workflow artifact, `alpha1-candidate-<commit>`, containing three binaries, two image archives, the multi-platform OCI archive, `image-info.json`, `image-info-arm64.json`, `oci-index-info.json`, `manifest.json`, `SHA256SUMS`, and `evidence/` (smoke logs: seam check, version, config validation, probe results, per-platform `rustc -vV`, server and container output; no payloads or credentials). `manifest.json` (version 2) records:

- source commit, whether the tree was dirty, and the CI run URL; toolchain (`rust-toolchain.toml` channel, `rustc --version`, and each platform's own `rustc`); gateway version
- exact core pin (`=0.1.0-beta.12` from `Cargo.lock`, with its lock checksum) and `Cargo.lock` sha256
- config schema version and the sha256 of each shipped example config and the Compose file
- `capabilities.proxy`: the endpoints (`POST /v1/chat/completions` and `POST /v1/responses`, each with its reviewed subset), JSON and SSE relay, `response_redaction: false`, and the fixed upstream. The Responses route is part of the shipped binary since #86; its qualification (#88) ran against the separate fake-upstream test build, so a candidate bundle still carries no accepted-request evidence for either endpoint
- `sdk_pins`: npm `openai` and PyPI `openai` versions and the sha256 of the lockfiles that carry their integrity hashes (examples and qualification harness)
- per artifact: file, platform, kind, sha256, size
- image identity: image id, pinned base image digest, runtime user, contained binary sha256
- `signing`, `sbom`, `provenance`: `"not produced"`; `distributable: false` with `distribution_blockers` that are true at the candidate commit (private-report test not recorded, registry and name unselected, no signing/SBOM/provenance, no quiet-host measurement, open follow-ups, publication not authorized)

A workflow step asserts those fields (`distributable` false, proxy capability, pins present, three artifacts, "not produced" markers). Verify a downloaded bundle with `sha256sum -c SHA256SUMS` (use `shasum -a 256 -c` on macOS).

## Container assumptions

- Base: distroless `cc-debian13:nonroot` pinned by digest; no shell, no package manager. Runs as uid 65532.
- Filesystem: read-only root is supported. The process writes nothing. Contents: the binary and `/etc/redact-secret-gateway/config.json` (`container/config.container.json`: upstream `openai`, profile `full`, capacity `8/65536/2/8/8`, measured for the scoped Beta 2 workload; broader production capacity remains provisional). Mount your own config over that path to change it (validate it first with `validate-config`).
- Capabilities: none needed. Run with `--cap-drop ALL --security-opt no-new-privileges`.
- Network: the container config binds `0.0.0.0:8787` with `allow_non_loopback: true`, the explicit acknowledgement required by [ADR 0009](decisions/0009-credential-and-upstream-trust-model.md). Publish the port to host loopback only (`-p 127.0.0.1:8787:8787`) or to a private network shared with the single application or sidecar. Publishing it beyond that is outside the supported model; loopback is an address restriction, not authentication. The container needs outbound HTTPS to `api.openai.com`; the gateway cannot prevent bypass, so restrict direct egress separately.
- No secrets in the image, no environment variables, and no build arguments. The provider key comes from your application per request. The config enforces a **local caller token** (#63): mount a token file read-only at `/run/secrets/gateway-local-token` (a regular file of 32 to 128 bytes of `A-Za-z0-9-._~`, readable by uid 65532 and not readable, writable or executable by others, for example mode 0400 owned by 65532, or 0440 with group 65532 as a Kubernetes Secret volume with `fsGroup: 65532` and `defaultMode: 0440` provides). Without it the container refuses to start (`invalid_config: unreadable at deployment.local_auth.token`). The application sends the token in `X-Gateway-Local-Token`; `/healthz` and `/readyz` need none, so probes run without the secret. Anyone who can read the token source is inside the trust boundary; the local hop is plain HTTP.
- Compose: `examples/compose/compose.yaml` (single service, loopback-published port, the token as a Compose secret from `GATEWAY_LOCAL_TOKEN_FILE`).
- Image name: `redact-secret-gateway:candidate` is a local name. The registry and image name are to be selected; nothing is pushed.

## Upgrade and rollback

Config upgrades from Alpha shapes, rollback pairs and the restart-only activation rule are in [config-upgrade-rollback](contracts/config-upgrade-rollback.md). The candidate image is unpublished; no compatibility between images is promised yet.

## Release gating

Artifact distribution is gated and nothing is published. Before any publication:

1. A license is selected by the maintainer (MIT, done; [ADR 0010](decisions/0010-release-prerequisites-license-and-reporting.md)).
2. Private vulnerability reporting is enabled and verified through the GitHub API (done, 2026-10-02; ADR 0010).
3. The Alpha 1 MVP qualification is reconciled ([report](qualification/alpha1-qualification-report.md)) and the maintainer accepts the residual risks and open follow-ups.
4. A registry and image name are selected, and a separate, environment-gated publish workflow is reviewed. Signing, SBOM, and provenance are not produced by the current workflow.
5. The maintainer authorizes publication.

## Integration examples

`examples/node` and `examples/python` point the OpenAI SDKs at the gateway base URL; `examples/compose/compose.yaml` runs the candidate image. They require your own provider key at run time. How each was verified, and what was not, is in [examples/README.md](../examples/README.md) and the root [README](../README.md#try-it).

## Beta 2 native ARM64/OCI extension (#91, executed)

The candidate workflow adds `aarch64-unknown-linux-gnu` on a native
`ubuntu-24.04-arm` runner, then executes that binary and its linux/arm64 image
(non-root/read-only, seam absence, config, probes and SIGTERM). Image assembly
checks ELF class/endian/machine against the explicitly selected platform and
copies the already-built binary. Smoke refuses an emulated/nonmatching host.

The expanded bundle has three binaries, two docker-save image variants and one
OCI-layout tar (six artifacts). `image-info-arm64.json` records the ARM64 config
ID/binary hash; `oci-index-info.json` records local OCI manifests/platforms/index
digest. OCI assembly preserves exact config/uncompressed layer bytes and verifies
rootfs diff_ids; OCI manifest digests are distinct from Docker config IDs. There
is no registry push, signing, SBOM or provenance. Native run URLs, exact ELF/image
hashes, checksums and platform selection are archived in the
[Beta 2 report](qualification/beta2-qualification-report.md).

The layout's top-level index has one named `candidate` reference to the inner
multi-platform index. The independent consumer uses Skopeo 1.13.3 (Ubuntu
package `1.13.3+ds1-2ubuntu0.24.04.3`) to select both architectures and compare
the complete selected config with each original native Docker config. This
selection is separate from native execution in the two platform jobs. After
extracting the OCI archive into `layout`, inspect locally without registry access:

```sh
skopeo --override-os linux --override-arch amd64 inspect --config oci:layout:candidate
skopeo --override-os linux --override-arch arm64 inspect --config oci:layout:candidate
```

The transport and named-reference syntax follows the
[containers/image OCI transport documentation](https://github.com/containers/image/blob/main/docs/containers-transports.5.md).
