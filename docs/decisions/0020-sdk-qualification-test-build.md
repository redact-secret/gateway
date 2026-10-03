# ADR 0020: SDK qualification through a separate non-release test build

Status: Accepted by the maintainer (approved in the #22 implementation session). Implemented in #22. Supersedes the open item at the end of [ADR 0017](0017-json-forwarding-deadlines-and-cancellation.md) ("SDK qualification (#22) without a production seam") and completes the SDK items of [ADR 0018](0018-sse-relay-termination-and-stream-bounds.md). Date: 2026-10-02. Constrained by [ADR 0009](0009-credential-and-upstream-trust-model.md), [ADR 0012](0012-release-candidate-artifact-build.md), and [ADR 0013](0013-fixed-https-destinations-and-outbound-authority.md).

## Context

The Alpha 1 acceptance items "works with pinned Node/Python SDK clients" (#20, #21, #22) need a real OpenAI SDK talking to a real gateway process whose provider is a local fake. The destination policy (ADR 0013) forbids, by design, any feature, flag, environment variable, or configuration path that points a release binary at anything but the reviewed HTTPS provider. The fake-upstream tests therefore live in `#[cfg(test)]` code that no external process can reach. Three ways out were considered:

1. **A seam in the shipped crate** (a cargo feature, `--cfg`, an environment variable, or a configuration field). Rejected: it is exactly what ADR 0013 forbids, and a release binary could then carry or be configured into an unreviewed destination.
2. **Make the unmodified product binary reach a fake with no seam at all**, by giving the fake a "public" loopback alias address, a name in `/etc/hosts`, a certificate for `api.openai.com`, and that CA in the system trust store. It would exercise the real address and TLS policy, but it needs root, rewrites the host's name resolution and trust store, cannot run on a developer laptop or macOS without global side effects, and tests the platform plumbing as much as the gateway. Not chosen; it remains a possible later deployment-chain test (#44).
3. **A separate, non-release test build** whose provider route is a loopback fake. Chosen.

## Decision

1. **The qualification build is a different crate, generated, never shipped.** `qualification/build.sh` copies the shipped `src/` into `qualification/gen/`, applies `qualification/seam.patch` with no fuzz, adds three new files from `qualification/overlay/` (`qualification.rs`, `transport_qualification.rs`, `main.rs`), writes a manifest whose `[dependencies]` block is copied byte for byte from the shipped `Cargo.toml`, and derives its `Cargo.lock` from the shipped `Cargo.lock` (then verifies that every locked package and checksum also exists in the shipped lock). The generated package is `redact-secret-gateway-qualification`; its binary has the same name; it is its own workspace root. The shipped package, its sources, its manifest (still no `[features]`, no `[workspace]`, no `build.rs`), its lockfile, its CI, its candidate workflow, its scripts, and its container image gain nothing. A drifted source tree makes the patch fail loudly instead of silently qualifying something else.
2. **What the seam changes, and nothing else.** The patch touches exactly five shipped files, in the copy only: it removes `#[cfg(test)]` from the existing test-only constructors (`Origin::for_test_http`, `for_test_https`, `Destination::for_test`, `RouteBinding::for_test`, `Scheme::Http`, `AddressPolicy::PublicOrLoopback`), splits `Services::init` into `init` and `init_with(plan, upstream, metrics)`, and adds `mod qualification` and `#![allow(dead_code)]`. The overlay's `Upstream::from_plan_with_fake_provider` builds the same hardened client and the same single reviewed route table, but with the route's origin replaced by the loopback fake over plain HTTP; the address argument must be a loopback IP literal or startup fails. When the configuration has no `deployment.upstream` the route table stays empty, exactly as in production, so the "no upstream configured, 501" path is qualified too. Everything else is the production code path: strict admission, core inspection, the sealed request, header policy, deadlines, bounds, permits, SSE relay, cancellation, telemetry. The reviewed origin table (`REVIEWED_HOSTS`), `Origin::parse`, the production address policy for real destinations, and TLS verification are not edited and are not exercised differently.
3. **Identification, so it cannot be confused with the product.** The binary is named `redact-secret-gateway-qualification`, prints `RSG-QUALIFICATION-BUILD-NOT-FOR-RELEASE` in its version line and on stderr at startup, requires `--fake-provider <loopback-ip:port>` for `serve` (without it the qualification binary refuses to serve), and also serves a loopback-only metrics snapshot used for the synthetic timing measurements (counters only, no payload).
4. **Distinct provenance and no distribution.** It is built only by `qualification/build.sh`, in the dedicated workflow `.github/workflows/qualification.yml` (SHA-pinned actions, `permissions: {}` by default with `contents: read` per job, `persist-credentials: false`, no secrets, no registry or write permission, no OIDC). That workflow uploads only text evidence under `qualification/evidence/`; it never uploads the test binary and never touches the candidate bundle. The candidate workflow `artifacts.yml` and ordinary CI never build, reference, or run it.
5. **The policy test is relaxed only by an explicit allowlist, and it is new rather than loosened.** No existing scan in `tests/destination_policy.rs` needed relaxing: the seam never enters `src/`, `Cargo.toml`, `scripts/`, `container/`, or the existing workflows, so every existing check (seams are `cfg(test)`-gated in source, no `[features]` and no `--features`/`--cfg` in `.github`, `scripts`, `container`, no environment variable read in `src/`, the real binary rejects test/insecure/origin/proxy fields) still passes unchanged. What the decision does relax is the implicit rule "no file in the repository may implement a test upstream". That is now stated as an allowlist of exactly eight files, in `QUALIFICATION_SEAM_ALLOWLIST`:

   - `.github/workflows/qualification.yml`
   - `qualification/build.sh`
   - `qualification/run-suites.sh`
   - `qualification/seam.patch`
   - `qualification/perf/run.mjs` (starts extra qualification gateways with different capacity for the measurements)
   - `qualification/overlay/main.rs`
   - `qualification/overlay/qualification.rs`
   - `qualification/overlay/transport_qualification.rs`

   Tests added to `tests/destination_policy.rs` assert: the allowlist is exactly that set and each entry is one existing file (no pattern, no directory, nothing under `src/`, `container/`, `scripts/`, `tests/`, `examples/`, and never `ci.yml`, `artifacts.yml`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `deny.toml`); the seam markers appear in no other non-document file in the repository (walked, not sampled); the patch touches exactly five named `src/` files while the shipped tree stays `cfg(test)`-gated; the overlay refuses non-loopback addresses and labels the build; the shipped manifest has no features, workspace, patch table, second binary, or build script and no qualification manifest is committed; the shipped binary built by `cargo test` contains none of the markers or the test-constructor symbol names; and the dedicated workflow is isolated (pinned actions, no secrets, evidence-only uploads) while `ci.yml`, `artifacts.yml`, `container/`, and `scripts/` mention nothing of it. The widening of this list is a maintainer decision and requires a new ADR.
6. **Release binaries are proven free of it, on the candidate bytes.** `scripts/check-no-qualification-seam.sh` scans a binary for the markers and the test-constructor symbol names (built from fragments, so the script is not itself a hit). The candidate workflow runs it on the Linux and macOS binaries and on the binary extracted from the image, as part of the documented smoke checks, and a negative control (the qualification binary fails the same check) was run when the script was written. `tests/destination_policy.rs` does the same scan on the debug binary in ordinary CI.
7. **The fake provider is test-only, loopback, and synthetic.** It is a dependency-free Node script (`qualification/fake-provider/server.mjs`) that listens on `127.0.0.1` only, holds no credential, and invents every byte it returns. It records requests (header names, a hash of the authorization header, the body the gateway sent) so tests can assert what reached the provider, and exposes an event-driven admin port for waiting (no polling, no sleeping as synchronization). Scenarios are selected by the request `model`.
8. **Qualification scope and evidence.** The pinned OpenAI SDKs (npm `openai` 7.27.0 with Node 24 and TypeScript 5.9.3 tooling, PyPI `openai` 3.24.0 on Python 3.13) are installed from lockfiles with integrity hashes (`package-lock.json`, `requirements.txt --require-hashes --no-deps`). The suites cover the supported text forms, planted-secret absence with structure preserved, rejected inputs with zero upstream connections and requests, JSON and error handling, fragmented/multibyte/multi-event/slow/gated/interrupted SSE, client abort and repeated cancellation returning every permit, and the observed SDK retry behavior (recorded as evidence and reconciled with `docs/contracts/errors-and-telemetry.md`). The README examples run in the same workflow. What this does **not** show is stated in the qualification report: it is not a live-provider test, not a test of the shipped artifacts' network path (that path is the part the fake replaces), and not a recall claim.

## Consequences

- The shipped binary's network path (HTTPS, platform trust, public-address policy) is covered by its own unit tests with throwaway certificates (ADR 0013) and by the candidate smoke checks that never reach a provider; the SDK suites cover everything between the SDK and that final hop. The reviewed final hop with a real provider is not exercised by this repository's CI.
- The seam patch is a maintenance cost: a change to `server.rs`, `transport.rs`, `destination.rs`, or `resolver.rs` near the patched lines makes `qualification/build.sh` fail until the patch is regenerated. That failure is intended; it forces a person to look at what the qualification build changes.
- The qualification build is a second build of the same sources; its dependency versions are the shipped lockfile's by construction and by check, but it is not byte-identical to any artifact.

## Owner

Maintainer. Any widening of the allowlist, any use of the qualification build for a purpose other than SDK and measurement qualification, and any decision to ship or distribute it requires a new ADR.

## Invariants

1. No shipped path (source, manifest, lockfile, CI, candidate workflow, scripts, container, candidate binaries) contains the seam.
2. The qualification binary is never uploaded as a candidate artifact, never published, and always labelled.
3. The fake provider address must be loopback.
4. The reviewed destination table, production address policy, and TLS policy are unchanged and are not weakened in any build.
5. Rejected requests deliver zero connections and zero request bytes to the fake provider; this is asserted on the provider's own record.

## Failure behavior

A patch that no longer applies, a lock mismatch, a missing marker or loopback guard, an allowlist change, a marker outside the allowlist, a marker in a candidate binary, or a qualification workflow that uploads anything but evidence fails CI. Nothing falls back to a weaker check.

## Verification

`tests/destination_policy.rs` (the tests named above), `scripts/check-no-qualification-seam.sh` in the candidate workflow, and the `Qualification (SDK, non-release test build)` workflow. See [the qualification report](../qualification/alpha1-qualification-report.md) for the run links.

## Beta 2 extension (#92–#94)

Under epic #14 delegation, the exact seam allowlist additionally permits
`.github/workflows/beta2.yml` and `qualification/sidecar/run.py` to build/run the
separate non-release workload image on native Linux architectures. It is never
uploaded as a candidate, placed in an OCI candidate bundle or published. Only
safe aggregate JSON/text evidence is uploaded. Distributed source/config and
candidate build paths gain no fake upstream/trust setting. The overlay also
forwards the bounded production probe command and allows loopback observed mode.
