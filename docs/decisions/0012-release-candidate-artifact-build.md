# ADR 0012: Release-candidate artifact build (verified skeleton; extended for Alpha 1 in #22)

Status: Accepted (design); implemented (scaffold, issue #7; extended to the Alpha 1 MVP candidate in issue #22, see the update at the end). Nothing is published.

## Context

Issue #7 needs an explicit build matrix, an image, a manifest, and smoke evidence for the verified skeleton, without distributing anything. The license is MIT and private reporting is enabled and verified (ADR 0010), and the gateway is not yet a proxy (the Alpha 1 MVP, #8, is open).

## Decision

Targets (Alpha 1 candidate only): Linux x86_64, macOS ARM64, and a Linux amd64 OCI image. Not included: Linux ARM64 or multi-arch image (Beta 2), Windows, macOS x86_64, Helm, npm/PyPI launchers.

Build:

- Each target is built once with `cargo build --locked --release` from the candidate commit with the toolchain in `rust-toolchain.toml`, on a runner of that platform (`ubuntu-24.04`, `macos-15` arm64). No cross-compilation, so every smoke test runs on the platform that built the bytes.
- Linux uses `x86_64-unknown-linux-gnu` (glibc), not musl. The TLS stack (`rustls` with `aws-lc-rs`, ADR 0011) compiles C code, and musl would add a C toolchain and a different allocator to a security-boundary binary before any measurement (ADR 0008) justifies it. The cost is a glibc runtime requirement, met by the image base.
- `RUSTFLAGS=--remap-path-prefix=<workspace>=/build` removes the checkout path. Bit-for-bit reproducibility is not verified or claimed; the manifest says so.

Image:

- Built from the already-built Linux binary: the Dockerfile only copies it. The image smoke test compares the binary inside the image with the candidate checksum.
- Base `gcr.io/distroless/cc-debian13:nonroot`, pinned by multi-arch index digest, resolved with `docker buildx imagetools inspect` on 2026-10-02. Chosen for glibc without a shell or package manager; glibc 2.41 is newer than the 2.39 of the build runner. Dependabot (docker) proposes digest updates for review.
- Runs as uid 65532, no env, build args, secrets, or volumes. Contents: the binary and one config file. It is designed for `--read-only --cap-drop ALL --security-opt no-new-privileges`, and the smoke test uses those flags.
- The image config binds `0.0.0.0:8787` with `allow_non_loopback: true`, the explicit acknowledgement required by ADR 0009 and the config validator. That address is inside the container network namespace; the supported publication is host loopback (`-p 127.0.0.1:8787:8787`) or a private application network. Publishing beyond the single application or sidecar is outside the supported model.
- Local name `redact-secret-gateway:candidate`. The registry and image name are not selected and nothing is pushed.

Manifest (`scripts/candidate-manifest.sh`, a POSIX shell script using `jq`; no new dependencies): source commit, toolchain, exact core pin and its `Cargo.lock` checksum, `Cargo.lock` sha256, config schema version, per-artifact sha256, size and platform, and the image identity. `signing`, `sbom`, `provenance` are the string `not produced`. `distributable` is `false` with the ADR 0010 and Alpha 1 blockers. `SHA256SUMS` covers the artifacts, the manifest, and the smoke evidence.

Release gating: no workflow publishes, creates a release, or pushes to a registry, and none holds registry credentials. A future publish workflow must refuse to run unless private reporting is enabled (ADR 0010) and until the Alpha 1 MVP qualification epic passes.

## Owner

Maintainer approves the registry/name and any change to the target list.

## Invariants

1. Each artifact is built once; later stages reuse and re-verify the same bytes.
2. A platform is qualified only by running the artifact on that platform.
3. No artifact claims signing, SBOM, or provenance that no step produced.
4. Skeleton artifacts are labeled skeleton, unpublished, and not distributable.

## Failure behavior

Any checksum mismatch, smoke failure, root user, or missing candidate file fails the workflow. A failed workflow produces no manifest bundle.

## Implementation handoff

- `.github/workflows/artifacts.yml`, `scripts/`, `container/`.
- #8 and later: replace the skeleton probes with real endpoint qualification. Beta 3: provenance, SBOM, signing.

## Verification

The `Candidate artifacts` workflow runs on pull requests that touch build files and on `workflow_dispatch`; its summary lists the manifest and checksums.

## Deferred measured choices

Resource budgets for the image, musl or static linking, and base-image alternatives, pending measurements (ADR 0008).

## Update in #22: the Alpha 1 candidate

The gateway is now a proxy (#18 to #21, #23 to #25), so the candidate is the Alpha 1 MVP, still unpublished and not distributable. What changed, without changing a decision above:

- **Smoke checks on the exact bytes, on their own platform** (Linux x86_64 on `ubuntu-24.04`, macOS ARM64 on `macos-15`, the linux/amd64 image on `ubuntu-24.04` with Docker): the qualification-seam absence check (`scripts/check-no-qualification-seam.sh`, ADR 0020), `--version`, `validate-config` for the shipped example configs, startup on loopback, `/healthz` and `/readyz`, seven proxy-route probes that are all rejected locally and can never forward (no credential 401, `{}` 422, unknown field plus a synthetic token 422 with no echo, wrong content type 415, `GET` on the route 405, unknown route 404), graceful SIGTERM. The image additionally: byte-identical binary, non-root (uid 65532), the image's own config validates, read-only root, all capabilities dropped, host-loopback publication only. The Compose example is started against the candidate image and probed the same way.
- **The image config now names the reviewed upstream** (`deployment.upstream.provider: openai`), profile `full`, and non-degenerate capacity numbers (`receipt 8`, `memory_units 65536`, `inspection 2`, `upstream 8`, `stream 8`: the numbers the SDK qualification ran with; provisional and unmeasured, ADR 0008). The earlier `1/1/1/1/1` skeleton numbers accepted no real request.
- **Manifest version 2** records the proxy capability, the SDK pins (versions and lockfile sha256), the shipped config and Compose file checksums, per-platform `rustc`, the CI run URL, and distribution blockers that are true today (private-report test not recorded, registry and name unselected, no signing/SBOM/provenance, no quiet-host measurement, open follow-ups and core issues, publication not authorized). `distributable` stays `false`.
- **The SDK suites run against the separate qualification build, never against these artifacts.** The artifacts' own network path is exercised by their unit tests and smoke checks, not by an SDK against a provider.
- The artifact bundle is named `alpha1-candidate-<sha>`.
