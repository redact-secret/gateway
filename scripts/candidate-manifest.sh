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
# step produces them. `distributable` is always false here (ADR 0010; Alpha 1 gate).
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

entries="$(mktemp)"
trap 'rm -f "$entries"' EXIT
add() { # file platform kind format
  f="$dir/$1"
  [ -f "$f" ] || { echo "missing candidate file: $1" >&2; exit 1; }
  jq -n --arg file "$1" --arg sha "$(sha256_of "$f")" --argjson size "$(wc -c <"$f" | tr -d ' ')" \
    --arg platform "$2" --arg kind "$3" --arg format "$4" \
    '{file:$file, sha256:$sha, size_bytes:$size, platform:$platform, kind:$kind, format:$format}' >>"$entries"
}
add redact-secret-gateway-x86_64-unknown-linux-gnu linux-x86_64 binary elf-gnu
add redact-secret-gateway-aarch64-apple-darwin macos-arm64 binary mach-o
add redact-secret-gateway-candidate-image-linux-amd64.tar linux-amd64 oci-image docker-save-tar

image_info="$dir/image-info.json"
[ -f "$image_info" ] || { echo "missing image-info.json" >&2; exit 1; }

jq -n -S \
  --slurpfile artifacts "$entries" \
  --slurpfile image "$image_info" \
  --arg commit "$commit" --argjson dirty "$dirty" \
  --arg gw "$gw_ver" --arg channel "$channel" --arg rustc "$rustc_v" \
  --arg core "$core_ver" --arg core_src "$core_src" --arg core_sum "$core_sum" \
  --arg lock "$(sha256_of Cargo.lock)" --argjson schema "$schema" '
{
  manifest_version: 1,
  status: "skeleton-release-candidate",
  description: "Verified skeleton (health endpoints only; not a sanitizing proxy). Not published.",
  distributable: false,
  distribution_blockers: [
    "private vulnerability reporting enabled but no end-to-end test report recorded (ADR 0010)",
    "Alpha 1 MVP qualification epic not complete"
  ],
  source: { commit: $commit, working_tree_dirty: $dirty },
  gateway_version: $gw,
  toolchain: { rust_toolchain_toml_channel: $channel, rustc: $rustc },
  core: { crate: "redact-secret", pin: ("=" + $core), version: $core, source: $core_src, cargo_lock_checksum: $core_sum },
  cargo_lock_sha256: $lock,
  config_schema_version: $schema,
  build: { command: "cargo build --locked --release", built_once_per_target: true, bit_reproducibility_verified: false },
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
