#!/bin/sh
# Probe a RUNNING skeleton gateway over HTTP: /healthz and /readyz return 200 and the
# proxy route is rejected locally (an empty JSON body is 422 `unsupported_input`). Sends only a
# synthetic empty JSON object, which fails validation before inspection, and the skeleton
# configs name no upstream, so no request can ever be forwarded (#20): this probe must never
# reach a provider. Do not add a body here that passes validation.
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
