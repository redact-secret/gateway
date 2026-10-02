# Candidate artifacts (skeleton, unpublished)

Status: implemented for the verified skeleton (#7). Governing decision: [ADR 0012](decisions/0012-release-candidate-artifact-build.md). Nothing described here is published or distributable.

The artifacts contain the **skeleton**: `--version`, `validate-config`, and a loopback server with `/healthz` and `/readyz`. Every other route is rejected locally with 404 `unsupported_input`. `POST /v1/chat/completions` runs strict admission and validation (#18) and then always ends in a local `501 not_implemented`; nothing is forwarded. It is not a sanitizing proxy; the proxy arrives with the Alpha 1 MVP (#8).

## Target matrix

| Artifact | Platform | Built and smoke-tested on |
| --- | --- | --- |
| `redact-secret-gateway-x86_64-unknown-linux-gnu` | Linux x86_64 (glibc) | `ubuntu-24.04` |
| `redact-secret-gateway-aarch64-apple-darwin` | macOS ARM64 | `macos-15` (Apple silicon) |
| `redact-secret-gateway-candidate-image-linux-amd64.tar` (`docker save` of `redact-secret-gateway:candidate`) | linux/amd64 OCI image | `ubuntu-24.04` with Docker |

Not included: Linux ARM64 and multi-arch images (Beta 2), Windows, macOS x86_64, Helm, npm/PyPI launchers, Kubernetes manifests. Each binary is built once; the image copies the Linux binary and its smoke test checks the bytes match.

## Build locally

```bash
cargo build --locked --release                      # binary for your host
sh scripts/smoke-binary.sh target/release/redact-secret-gateway examples/config.skeleton.json /tmp/rsg-evidence
# Linux x86_64 host with Docker only:
sh scripts/build-image.sh target/release/redact-secret-gateway redact-secret-gateway:candidate
sh scripts/smoke-image.sh redact-secret-gateway:candidate "$(sha256sum target/release/redact-secret-gateway | cut -d ' ' -f 1)" /tmp/rsg-image-evidence
```

The authoritative candidate is built by the `Candidate artifacts (skeleton, unpublished)` workflow (`.github/workflows/artifacts.yml`, on `workflow_dispatch` and on pull requests touching build files). Local builds do not match the CI bytes (different toolchain host and paths); bit reproducibility is not verified.

## What the workflow produces

One workflow artifact, `skeleton-candidate-<commit>`, containing the two binaries, the image archive, `image-info.json`, `manifest.json`, `SHA256SUMS`, and `evidence/` (smoke logs: version, config validation, probe results, server and container output; no payloads or credentials). `manifest.json` records:

- source commit, toolchain (`rust-toolchain.toml` channel and `rustc --version`), gateway version
- exact core pin (`=0.1.0-beta.12` from `Cargo.lock`, with its lock checksum) and `Cargo.lock` sha256
- config schema version
- per artifact: file, platform, kind, sha256, size
- image identity: image id, pinned base image digest, runtime user, contained binary sha256
- `signing`, `sbom`, `provenance`: `"not produced"`; `distributable: false` with the blockers

Verify a downloaded bundle with `sha256sum -c SHA256SUMS` (use `shasum -a 256 -c` on macOS).

## Container assumptions

- Base: distroless `cc-debian13:nonroot` pinned by digest; no shell, no package manager. Runs as uid 65532.
- Filesystem: read-only root is supported. The process writes nothing. Contents: the binary and `/etc/redact-secret-gateway/config.json`. Mount your own config over that path to change it (validate it first with `validate-config`).
- Capabilities: none needed. Run with `--cap-drop ALL --security-opt no-new-privileges`.
- Network: the container config binds `0.0.0.0:8787` with `allow_non_loopback: true`, the explicit acknowledgement required by [ADR 0009](decisions/0009-credential-and-upstream-trust-model.md). Publish the port to host loopback only (`-p 127.0.0.1:8787:8787`) or to a private network shared with the single application or sidecar. Publishing it beyond that is outside the supported model; loopback is an address restriction, not authentication.
- No secrets, environment variables, or build arguments. No provider credentials are used by the skeleton.
- Compose: `examples/compose/compose.skeleton.yaml` (single service, loopback-published port, skeleton only).
- Image name: `redact-secret-gateway:candidate` is a local name. The registry and image name are to be selected; nothing is pushed.

## Release gating

Artifact distribution is gated and nothing is published. Before any publication:

1. A license is selected by the maintainer (MIT, done; [ADR 0010](decisions/0010-release-prerequisites-license-and-reporting.md)).
2. Private vulnerability reporting is enabled (done) and verified with an end-to-end test report (ADR 0010, open).
3. The Alpha 1 MVP qualification epic is complete (#8 and dependents), plus the other Alpha 1 gates in `SECURITY.md`.
4. A registry and image name are selected, and a separate, environment-gated publish workflow is reviewed. Signing, SBOM, and provenance are not produced by the current workflow.

## Draft integration examples

`examples/node` and `examples/python` show an OpenAI SDK pointed at the gateway base URL. They are drafts: the proxy endpoint is not available until the Alpha 1 MVP (#8), and they must not be run expecting success.
