#!/bin/sh
# Probe a RUNNING gateway over HTTP with the documented MVP smoke checks: /healthz and /readyz
# return 200, and every probe below is REJECTED LOCALLY with the documented status and safe code,
# and the rejection never echoes the planted synthetic token.
#
#   GET  /healthz, /readyz                           200
#   POST /v1/chat/completions, no credential          401 missing_credential
#   POST /v1/chat/completions, {}                      422 unsupported_input
#   POST /v1/chat/completions, unknown field + synthetic token   422 unsupported_input
#   POST /v1/chat/completions, Content-Type text/plain 415 unsupported_input
#   GET  /v1/chat/completions                          405
#   GET  /v1/models                                    404 unsupported_input
#
# SAFETY: the gateway under test may have an upstream configured (the shipped example and
# container configs do), so this script must NEVER send a body that passes validation: every
# POST below fails admission before inspection and before any forwarding, so no probe can reach
# a provider. Do not add a body here that passes validation. The token and key are synthetic.
#
# usage: probe-endpoints.sh <base-url> <scratch-dir>
set -eu
base="$1"
scratch="$2"
mkdir -p "$scratch"
auth='Authorization: Bearer sk-SYNTHETIC-REVOKED-CI-NOT-A-KEY'
json='Content-Type: application/json'
token='ghp_SYNTHETICREVOKED00000000000000000001'

expect() { # label expected-status expected-code(or -) curl-args...
  label="$1"; want="$2"; code_want="$3"; shift 3
  got="$(curl -s -o "$scratch/probe.out" -w '%{http_code}' "$@")"
  echo "$label -> $got"
  test "$got" = "$want"
  if [ "$code_want" != "-" ]; then grep -q "\"code\":\"$code_want\"" "$scratch/probe.out"; fi
  if grep -q "$token" "$scratch/probe.out"; then echo "response echoed the planted token" >&2; exit 1; fi
}

expect "GET /healthz" 200 - "$base/healthz"
expect "GET /readyz" 200 - "$base/readyz"
expect "POST no credential" 401 missing_credential -X POST -H "$json" -d '{}' "$base/v1/chat/completions"
expect "POST {}" 422 unsupported_input -X POST -H "$json" -H "$auth" -d '{}' "$base/v1/chat/completions"
expect "POST unknown field with synthetic token" 422 unsupported_input -X POST -H "$json" -H "$auth" \
  -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"$token\"}],\"frobnicate\":1}" \
  "$base/v1/chat/completions"
expect "POST text/plain" 415 unsupported_input -X POST -H 'Content-Type: text/plain' -H "$auth" -d 'x' "$base/v1/chat/completions"
expect "GET /v1/chat/completions" 405 unsupported_input "$base/v1/chat/completions"
expect "GET /v1/models" 404 unsupported_input "$base/v1/models"
echo "MVP smoke probes: health OK; every proxy-route probe rejected locally with its safe code"
