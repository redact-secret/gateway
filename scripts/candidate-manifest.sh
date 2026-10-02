#!/bin/sh
# Generate the release-candidate manifest and SHA256SUMS for the candidate bytes in a
# directory. Run from the repository root at the candidate commit (needs git, jq, awk).
#
# usage: candidate-manifest.sh <dir>
# <dir> holds the candidate files (names are fixed, see ARTIFACTS below) and gets
# manifest.json and SHA256SUMS written into it. Smoke evidence under <dir>/evidence is
# listed in SHA256SUMS but is not a candidate artifact.
#
# Honesty rules: signing, SBOM, and provenance are recorded as "not produced" because no
# step produces them. `distributable` is always false here (ADR 0010; Alpha 1 gate). Every
# blocker below is a true statement at this commit; remove one only when its evidence exists.
set -eu
dir="$1"
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib-sha256.sh
. "$here/lib-sha256.sh"

commit="$(git rev-parse HEAD)"
dirty=false
[ -z "$(git status --porcelain)" ] || dirty=true

channel="$(awk -F'"' '/^channel *=/ {print $2}' rust-toolchain.toml)"
rustc_v="$(rustc --version)"
core_ver="$(awk '$0=="name = \"redact-secret\"" {f=1; next} f && /^version =/ {gsub(/"/,"",$3); print $3; exit}' Cargo.lock)"
core_src="$(awk '$0=="name = \"redact-secret\"" {f=1; next} f && /^source =/ {sub(/^source = "/,""); sub(/"$/,""); print; exit}' Cargo.lock)"
core_sum="$(awk '$0=="name = \"redact-secret\"" {f=1; next} f && /^checksum =/ {gsub(/"/,"",$3); print $3; exit}' Cargo.lock)"
gw_ver="$(awk -F'"' '/^version *=/ {print $2; exit}' Cargo.toml)"
schema="$(awk -F'[ ;]+' '/^pub const SCHEMA_VERSION/ {print $6; exit}' src/config.rs)"
test -n "$core_ver" && test -n "$core_sum" && test -n "$schema" && test -n "$gw_ver"

# SDK pins: the exact versions the examples install and the SDK qualification ran with, and the
# sha256 of the lockfiles that carry their integrity hashes.
node_sdk="$(jq -r '.dependencies.openai' examples/node/package.json)"
node_sdk_q="$(jq -r '.dependencies.openai' qualification/sdk/node/package.json)"
py_sdk="$(awk -F'==' '/^openai==/ {print $2}' examples/python/requirements.in)"
py_sdk_q="$(awk -F'==' '/^openai==/ {print $2}' qualification/sdk/python/requirements.in)"
test -n "$node_sdk" && test -n "$py_sdk"
[ "$node_sdk" = "$node_sdk_q" ] || { echo "Node SDK pin differs between examples and qualification" >&2; exit 1; }
[ "$py_sdk" = "$py_sdk_q" ] || { echo "Python SDK pin differs between examples and qualification" >&2; exit 1; }

entries="$(mktemp)"
configs="$(mktemp)"
trap 'rm -f "$entries" "$configs"' EXIT
add() { # file platform kind format
  f="$dir/$1"
  [ -f "$f" ] || { echo "missing candidate file: $1" >&2; exit 1; }
  platform_rustc=""
  [ ! -f "$dir/evidence/$5/rustc.txt" ] || platform_rustc="$(awk '/^rustc / {print; exit}' "$dir/evidence/$5/rustc.txt")"
  jq -n --arg file "$1" --arg sha "$(sha256_of "$f")" --argjson size "$(wc -c <"$f" | tr -d ' ')" \
    --arg platform "$2" --arg kind "$3" --arg format "$4" --arg rustc "$platform_rustc" \
    '{file:$file, sha256:$sha, size_bytes:$size, platform:$platform, kind:$kind, format:$format}
     + (if $rustc == "" then {} else {built_with:$rustc} end)' >>"$entries"
}
add redact-secret-gateway-x86_64-unknown-linux-gnu linux-x86_64 binary elf-gnu linux-x86_64
add redact-secret-gateway-aarch64-apple-darwin macos-arm64 binary mach-o macos-arm64
add redact-secret-gateway-candidate-image-linux-amd64.tar linux-amd64 oci-image docker-save-tar none

for f in examples/config.openai.json examples/config.skeleton.json container/config.container.json examples/compose/compose.yaml; do
  jq -n --arg file "$f" --arg sha "$(sha256_of "$f")" '{file:$file, sha256:$sha}' >>"$configs"
done

image_info="$dir/image-info.json"
[ -f "$image_info" ] || { echo "missing image-info.json" >&2; exit 1; }

run_url=""
[ -z "${GITHUB_RUN_ID:-}" ] || run_url="${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-}/actions/runs/${GITHUB_RUN_ID}"

jq -n -S \
  --slurpfile artifacts "$entries" \
  --slurpfile configs "$configs" \
  --slurpfile image "$image_info" \
  --arg commit "$commit" --argjson dirty "$dirty" \
  --arg gw "$gw_ver" --arg channel "$channel" --arg rustc "$rustc_v" \
  --arg core "$core_ver" --arg core_src "$core_src" --arg core_sum "$core_sum" \
  --arg lock "$(sha256_of Cargo.lock)" --argjson schema "$schema" \
  --arg node_sdk "$node_sdk" --arg py_sdk "$py_sdk" \
  --arg node_lock "$(sha256_of examples/node/package-lock.json)" \
  --arg node_q_lock "$(sha256_of qualification/sdk/node/package-lock.json)" \
  --arg py_lock "$(sha256_of examples/python/requirements.txt)" \
  --arg py_q_lock "$(sha256_of qualification/sdk/python/requirements.txt)" \
  --arg run_url "$run_url" '
{
  manifest_version: 2,
  status: "alpha1-release-candidate-unpublished",
  description: "Alpha 1 MVP candidate: health endpoints and the POST /v1/chat/completions text-subset proxy (JSON and SSE relay, responses not redacted) to the fixed OpenAI route. Not published, not distributable.",
  distributable: false,
  distribution_blockers: [
    "private vulnerability reporting is enabled (the license is MIT) but no end-to-end test report is recorded (ADR 0010)",
    "registry and image name are not selected; nothing is pushed",
    "no signing, SBOM, or provenance is produced (planned for Beta 3)",
    "numeric limits and capacity are provisional: no quiet-host performance measurement is recorded (ADR 0008)",
    "open Alpha 1 follow-ups gateway #40 through #44 and core issues redact-secret/redact-secret #1177 through #1180 are unresolved",
    "the maintainer has not authorized publication (the qualification report lists the remaining items)"
  ],
  capabilities: {
    proxy: {
      endpoint: "POST /v1/chat/completions",
      subset: "OpenAI Chat Completions text only; unsupported fields and content forms are rejected",
      json_relay: true,
      sse_relay: true,
      response_redaction: false,
      upstream: "fixed https://api.openai.com (provider profile openai), set by deployment.upstream.provider"
    },
    health: ["GET /healthz", "GET /readyz"],
    platforms: ["linux-x86_64", "macos-arm64", "linux-amd64 image"]
  },
  source: { commit: $commit, working_tree_dirty: $dirty, ci_run_url: (if $run_url == "" then null else $run_url end) },
  gateway_version: $gw,
  toolchain: { rust_toolchain_toml_channel: $channel, rustc: $rustc },
  core: { crate: "redact-secret", pin: ("=" + $core), version: $core, source: $core_src, cargo_lock_checksum: $core_sum },
  cargo_lock_sha256: $lock,
  config_schema_version: $schema,
  config_files: $configs,
  sdk_pins: {
    node: { package: "openai (npm)", version: $node_sdk, examples_lockfile_sha256: $node_lock, qualification_lockfile_sha256: $node_q_lock, lockfile_integrity: "npm package-lock.json integrity hashes; installed with npm ci --ignore-scripts" },
    python: { package: "openai (PyPI)", version: $py_sdk, examples_requirements_sha256: $py_lock, qualification_requirements_sha256: $py_q_lock, lockfile_integrity: "requirements.txt with sha256 hashes; installed with pip --require-hashes --no-deps" }
  },
  qualification: {
    sdk_suites: "run against a separate non-release test build, never against these artifacts; see docs/qualification/alpha1-qualification-report.md",
    candidate_artifacts_run_documented_smoke_checks: true
  },
  build: { command: "cargo build --locked --release", features: "none", built_once_per_target: true, bit_reproducibility_verified: false },
  image: ($image[0] + { registry_name: "to be selected; not pushed", registry_digest: "none (not pushed)" }),
  artifacts: $artifacts,
  signing: "not produced",
  sbom: "not produced",
  provenance: "not produced"
}' >"$dir/manifest.json"

(
  cd "$dir"
  files="$(ls redact-secret-gateway-* manifest.json image-info.json)"
  [ ! -d evidence ] || files="$files
$(find evidence -type f | LC_ALL=C sort)"
  printf '%s\n' "$files" | while IFS= read -r f; do
    printf '%s  %s\n' "$(sha256_of "$f")" "$f"
  done >SHA256SUMS
)
echo "wrote $dir/manifest.json and $dir/SHA256SUMS"
