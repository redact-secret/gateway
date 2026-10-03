#!/bin/sh
# Smoke-test an ACTUAL candidate binary on the platform it was built for.
# usage: smoke-binary.sh <binary> <config.json> <evidence-dir>
# Runs: the qualification-seam absence check, --version, validate-config, serve (loopback) +
# the documented MVP probes (health, and proxy-route rejections that never forward), then
# SIGTERM and requires a clean exit. The probes send only synthetic, locally rejected requests
# (see probe-endpoints.sh), so even a config with an upstream cannot reach a provider.
set -eu
bin="$1"
config="$2"
out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$out"
test -x "$bin"

sh "$here/check-no-qualification-seam.sh" "$bin" | tee "$out/seam-check.txt"

ver="$("$bin" --version)"
printf '%s\n' "$ver" | tee "$out/version.txt"
case "$ver" in
  "redact-secret-gateway "*" (core redact-secret "*")") ;;
  *) echo "unexpected version line" >&2; exit 1 ;;
esac

"$bin" validate-config "$config" | tee "$out/validate-config.txt"

"$bin" serve "$config" >"$out/serve.out" 2>"$out/serve.err" &
pid=$!
i=0
until grep -q '^listening ' "$out/serve.out" 2>/dev/null; do
  i=$((i + 1))
  if [ "$i" -gt 100 ] || ! kill -0 "$pid" 2>/dev/null; then
    echo "server did not start" >&2
    exit 1
  fi
  sleep 0.1
done
sh "$here/probe-endpoints.sh" "http://127.0.0.1:8787" "$out" | tee "$out/probe.txt"
kill -TERM "$pid"
status=0
wait "$pid" || status=$?
echo "exit status after SIGTERM: $status" | tee -a "$out/probe.txt"
test "$status" -eq 0
grep -q 'shutdown complete' "$out/serve.out"
# Configuration, local-auth, readiness, migration and rollback lifecycle of the same bytes (#65).
sh "$here/smoke-local-auth.sh" "$bin" "$out"
echo "binary smoke OK"
