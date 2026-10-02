#!/usr/bin/env bash
# Run every executable deployment-chain evidence procedure (issue #44) against a gateway image.
# usage: run-all.sh <gateway-image> <evidence-dir>
#
# Needs: bash, docker, openssl. <gateway-image> must be the shipped candidate image (for example
# built by scripts/build-image.sh from `cargo build --locked --release`, linux/amd64). Nothing
# here uses a real provider, a credential, or the Internet beyond pulling the digest-pinned
# helper images. Exit status is non-zero if any control check fails.
set -euo pipefail
# shellcheck source=scripts/deployment-evidence/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

gimage="$1"
out="$2"
mkdir -p "$out"
out="$(cd "$out" && pwd)" # absolute: used as a docker bind-mount source

pull_helpers
record_environment "$out/environment.txt" "$gimage"
cat "$out/environment.txt"

rc=0
for step in egress resolver-trust intermediary; do
  echo
  echo "===== $step ====="
  if ! bash "$DE_DIR/$step.sh" "$gimage" "$out" 2>&1 | tee "$out/$step.stdout.log"; then
    rc=1
  fi
done
exit "$rc"
