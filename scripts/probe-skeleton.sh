#!/bin/sh
# Probe a RUNNING skeleton gateway over HTTP: /healthz and /readyz return 200 and the
# proxy route is rejected locally (an empty JSON body is 422 `unsupported_input`). The skeleton is not a
# proxy; nothing is forwarded. Sends only a synthetic empty JSON object.
#
# usage: probe-skeleton.sh <base-url> <scratch-dir>
set -eu
base="$1"
scratch="$2"
mkdir -p "$scratch"
for p in healthz readyz; do
  code="$(curl -s -o /dev/null -w '%{http_code}' "$base/$p")"
  echo "GET /$p -> $code"
  test "$code" = 200
done
code="$(curl -s -o "$scratch/proxy.out" -w '%{http_code}' -X POST -H 'Content-Type: application/json' -H 'Authorization: Bearer sk-SYNTHETIC-REVOKED-CI-NOT-A-KEY' -d '{}' "$base/v1/chat/completions")"
echo "POST /v1/chat/completions -> $code"
test "$code" = 422
grep -q unsupported_input "$scratch/proxy.out"
echo "proxy route rejected locally (unsupported_input)"
