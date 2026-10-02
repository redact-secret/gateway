# ADR 0012: Release-candidate artifact build for the verified skeleton

Status: Accepted (design); implemented (scaffold, issue #7). Nothing is published.

## Context

Issue #7 needs an explicit build matrix, an image, a manifest, and smoke evidence for the verified skeleton, without distributing anything. License selection and private reporting are still open (ADR 0010), and the gateway is not yet a proxy (the Alpha 1 MVP, #8, is open).

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

Release gating: no workflow publishes, creates a release, or pushes to a registry, and none holds registry credentials. A future publish workflow must refuse to run while ADR 0010 is open and until the Alpha 1 MVP qualification epic passes.

## Owner

Maintainer approves the registry/name, license, and any change to the target list.

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
