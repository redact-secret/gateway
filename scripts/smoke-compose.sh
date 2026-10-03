#!/bin/sh
# Smoke-test the Compose example against the local candidate image (Linux amd64 runner).
# usage: smoke-compose.sh <compose-file> <evidence-dir>
# Requires the image named in the compose file (redact-secret-gateway:candidate) to exist
# locally (the example uses pull_policy: never, so nothing is pulled). Checks: the file is
# valid Compose, the service starts, becomes ready on host loopback, passes the documented
# MVP probes (health, and proxy-route rejections that never forward), and stops cleanly.
set -eu
file="$1"
out="$2"
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib-local-token.sh
. "$here/lib-local-token.sh"
mkdir -p "$out"
# The example mounts a local caller token from GATEWAY_LOCAL_TOKEN_FILE (#63).
tokdir="$(mktemp -d)"
make_local_token "$tokdir"

docker compose -f "$file" config -q
cleanup() {
  docker compose -f "$file" down --timeout 10 >/dev/null 2>&1 || true
  rm -rf "$tokdir"
}
trap cleanup EXIT

docker compose -f "$file" up -d
i=0
until curl -s -f -o /dev/null "http://127.0.0.1:8787/readyz"; do
  i=$((i + 1))
  if [ "$i" -gt 100 ]; then
    docker compose -f "$file" logs >"$out/compose.log" 2>&1 || true
    echo "compose service did not become ready" >&2
    exit 1
  fi
  sleep 0.1
done
GATEWAY_LOCAL_TOKEN="$GATEWAY_LOCAL_TOKEN" sh "$here/probe-endpoints.sh" "http://127.0.0.1:8787" "$out" | tee "$out/compose-smoke.txt"
docker compose -f "$file" logs >"$out/compose.log" 2>&1 || true
docker compose -f "$file" down --timeout 10
trap - EXIT
echo "compose smoke OK" | tee -a "$out/compose-smoke.txt"
