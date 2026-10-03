#!/bin/sh
# Probe a RUNNING gateway over HTTP with the documented MVP smoke checks: /healthz and /readyz
# return 200, and every probe below is REJECTED LOCALLY with the documented status and safe code,
# and the rejection never echoes the planted synthetic token.
#
#   GET  /healthz, /readyz                           200 (never needs the local token)
#   With GATEWAY_LOCAL_TOKEN set (a gateway that enforces deployment.local_auth, #63):
#   POST /v1/chat/completions, no local token          401 local_auth_required
#   POST /v1/chat/completions, wrong local token       401 local_auth_invalid
#   (every POST below then carries the local token)
#   POST /v1/chat/completions, no credential          401 missing_credential
#   POST /v1/chat/completions, {}                      422 unsupported_input
#   POST /v1/chat/completions, unknown field + synthetic token   422 unsupported_input
#   POST /v1/chat/completions, Content-Type text/plain 415 unsupported_input
#   Alpha 2 shapes, each refused by the field contract before inspection (#57):
#   POST tools with a schema $ref + synthetic token    422 unsupported_input
#   POST metadata with a number value                  422 unsupported_input
#   POST tool_calls with malformed JSON arguments      400 malformed_input
#   POST tool_calls with duplicate argument keys       400 malformed_input
#   GET  /v1/chat/completions                          405
#   Responses route (#88), each refused before inspection and forwarding, on the exact binary:
#   POST /v1/responses, no local token (token gateways) 401 local_auth_required
#   POST /v1/responses, no credential                  401 missing_credential
#   POST /v1/responses, {}                             422 unsupported_input
#   POST /v1/responses, store omitted + synthetic token 422 unsupported_input
#   POST /v1/responses, previous_response_id           422 unsupported_input
#   POST /v1/responses, hosted web_search tool         422 unsupported_input
#   POST /v1/responses, function_call malformed arguments 400 malformed_input
#   POST /v1/responses, Content-Type text/plain        415 unsupported_input
#   GET  /v1/responses                                 405
#   POST /v1/responses/ (trailing slash)               404 unsupported_input
#   GET  /v1/models                                    404 unsupported_input
#
# SAFETY: the gateway under test may have an upstream configured (the shipped example and
# container configs do), so this script must NEVER send a body that passes validation: every
# POST below fails admission before inspection and before any forwarding, so no probe can reach
# a provider. Do not add a body here that passes validation. The token and key are synthetic.
#
# usage: [GATEWAY_LOCAL_TOKEN=<token>] probe-endpoints.sh <base-url> <scratch-dir>
# The local token value is never printed. It is supplied by the caller (a throwaway value the
# smoke script generated for this run), never a real credential.
set -eu
base="$1"
scratch="$2"
mkdir -p "$scratch"
auth='Authorization: Bearer sk-SYNTHETIC-REVOKED-CI-NOT-A-KEY'
json='Content-Type: application/json'
token='ghp_SYNTHETICREVOKED00000000000000000001'
# The local caller token header for every POST below. Without a configured token (the
# Alpha behavior on loopback) the gateway ignores any such header, so a neutral one is sent.
if [ -n "${GATEWAY_LOCAL_TOKEN:-}" ]; then
  local_hdr="X-Gateway-Local-Token: $GATEWAY_LOCAL_TOKEN"
else
  local_hdr='X-Gateway-Local-Probe: none'
fi

expect() { # label expected-status expected-code(or -) curl-args...
  label="$1"; want="$2"; code_want="$3"; shift 3
  got="$(curl -s -o "$scratch/probe.out" -w '%{http_code}' "$@")"
  echo "$label -> $got"
  test "$got" = "$want"
  if [ "$code_want" != "-" ]; then grep -q "\"code\":\"$code_want\"" "$scratch/probe.out"; fi
  if grep -q "$token" "$scratch/probe.out"; then echo "response echoed the planted token" >&2; exit 1; fi
  if [ -n "${GATEWAY_LOCAL_TOKEN:-}" ] && grep -q -F -- "$GATEWAY_LOCAL_TOKEN" "$scratch/probe.out"; then
    echo "response echoed the local token" >&2; exit 1
  fi
}

expect "GET /healthz" 200 - "$base/healthz"
expect "GET /readyz" 200 - "$base/readyz"
if [ -n "${GATEWAY_LOCAL_TOKEN:-}" ]; then
  # Authentication is the first decision: an unauthenticated POST is refused before its body is
  # read, whatever the body and the provider credential are.
  expect "POST no local token" 401 local_auth_required -X POST -H "$json" -H "$auth" -d '{}' "$base/v1/chat/completions"
  expect "POST wrong local token" 401 local_auth_invalid -X POST -H "$json" -H "$auth" \
    -H "X-Gateway-Local-Token: SYNTHETIC-WRONG-LOCAL-TOKEN-0123456789abcdef" -d '{}' "$base/v1/chat/completions"
fi
expect "POST no credential" 401 missing_credential -X POST -H "$json" -H "$local_hdr" -d '{}' "$base/v1/chat/completions"
expect "POST {}" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" -d '{}' "$base/v1/chat/completions"
expect "POST unknown field with synthetic token" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"$token\"}],\"frobnicate\":1}" \
  "$base/v1/chat/completions"
expect "POST text/plain" 415 unsupported_input -X POST -H 'Content-Type: text/plain' -H "$auth" -H "$local_hdr" -d 'x' "$base/v1/chat/completions"
expect "POST tool schema with \$ref and a synthetic token" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"$token\"}],\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"f\",\"parameters\":{\"type\":\"object\",\"properties\":{\"a\":{\"\$ref\":\"#/x\"}}}}}]}" \
  "$base/v1/chat/completions"
expect "POST metadata with a number value" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}],\"metadata\":{\"k\":1}}" \
  "$base/v1/chat/completions"
expect "POST tool_calls with malformed arguments" 400 malformed_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"q\"},{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{bad\"}}]}]}" \
  "$base/v1/chat/completions"
expect "POST tool_calls with duplicate argument keys" 400 malformed_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"q\"},{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":1,\\\"a\\\":2}\"}}]}]}" \
  "$base/v1/chat/completions"
expect "GET /v1/chat/completions" 405 unsupported_input "$base/v1/chat/completions"
if [ -n "${GATEWAY_LOCAL_TOKEN:-}" ]; then
  expect "POST /v1/responses no local token" 401 local_auth_required -X POST -H "$json" -H "$auth" -d '{}' "$base/v1/responses"
fi
expect "POST /v1/responses no credential" 401 missing_credential -X POST -H "$json" -H "$local_hdr" -d '{}' "$base/v1/responses"
expect "POST /v1/responses {}" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" -d '{}' "$base/v1/responses"
# Each body below is refused by the Responses contract; none of them is a valid request, so none can be forwarded.
expect "POST /v1/responses store omitted with a synthetic token" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"input\":\"$token\"}" "$base/v1/responses"
expect "POST /v1/responses previous_response_id" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"store\":false,\"previous_response_id\":\"resp_synthetic\",\"input\":\"$token\"}" "$base/v1/responses"
expect "POST /v1/responses hosted web_search tool" 422 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"store\":false,\"input\":\"$token\",\"tools\":[{\"type\":\"web_search\"}]}" "$base/v1/responses"
expect "POST /v1/responses function_call with malformed arguments" 400 malformed_input -X POST -H "$json" -H "$auth" -H "$local_hdr" \
  -d "{\"model\":\"m\",\"store\":false,\"input\":[{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{bad\"}]}" "$base/v1/responses"
expect "POST /v1/responses text/plain" 415 unsupported_input -X POST -H 'Content-Type: text/plain' -H "$auth" -H "$local_hdr" -d 'x' "$base/v1/responses"
expect "GET /v1/responses" 405 unsupported_input "$base/v1/responses"
expect "POST /v1/responses/ (trailing slash)" 404 unsupported_input -X POST -H "$json" -H "$auth" -H "$local_hdr" -d '{}' "$base/v1/responses/"
expect "GET /v1/models" 404 unsupported_input "$base/v1/models"
echo "smoke probes: health OK; every proxy-route probe (Chat Completions and Responses) rejected locally with its safe code"
