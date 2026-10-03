#!/bin/sh
# Run examples/node and examples/python exactly as the README tells a user to, against the
# qualification stack started by run-suites.sh (environment: GATEWAY_STANDARD, QUAL_ADMIN,
# QUAL_SYNTHETIC). Synthetic key, fake provider, loopback only. This verifies the EXAMPLES
# against the qualification build; it says nothing about a real provider.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
: "${GATEWAY_STANDARD:?run via qualification/run-suites.sh}"

OPENAI_API_KEY="$(jq -r .api_key "$QUAL_SYNTHETIC")"
GATEWAY_BASE_URL="$GATEWAY_STANDARD/v1"
EXAMPLE_MODEL=qual-example
export OPENAI_API_KEY GATEWAY_BASE_URL EXAMPLE_MODEL

py="${EXAMPLES_PYTHON:-$root/examples/python/.venv/bin/python}"
# Node 24 runs TypeScript natively; older Node needs the flag (the README requires Node 24+).
if [ "$(node -p 'process.versions.node.split(".")[0]')" -lt 24 ]; then
  NODE_OPTIONS="--experimental-strip-types --no-warnings"
  export NODE_OPTIONS
fi

reset() { curl -s -X POST "$QUAL_ADMIN/__admin/reset" >/dev/null; }
# Two provider calls (JSON, then stream), the planted synthetic token absent and its placeholder
# present in both, and the stream flag preserved on the second.
verify_calls() {
  curl -s "$QUAL_ADMIN/__admin/calls" | jq -e '
    (.calls | length) == 2
    and ([.calls[].body] | all(contains("<SECRET_1>") and (contains("ghp_") | not)))
    and (.calls[1].body | contains("\"stream\":true"))
    and (.calls[0].body | contains("\"stream\"") | not)' >/dev/null
}

echo "== examples/node (npm start -- --demo-redaction)"
reset
(cd "$root/examples/node" && npm start --silent -- --demo-redaction)
verify_calls
echo "== examples/node without a key exits 2 and says so"
code=0
(cd "$root/examples/node" && OPENAI_API_KEY='' npm start --silent) >/dev/null 2>&1 || code=$?
test "$code" -eq 2

echo "== examples/python (--demo-redaction)"
reset
(cd "$root/examples/python" && "$py" openai_via_gateway.py --demo-redaction)
verify_calls
echo "== examples/python without a key exits 2 and says so"
code=0
(cd "$root/examples/python" && OPENAI_API_KEY='' "$py" openai_via_gateway.py) >/dev/null 2>&1 || code=$?
test "$code" -eq 2

# Beta 1 dual-credential demonstration (#65): the same two examples against a gateway that enforces
# deployment.local_auth. The provider key (OPENAI_API_KEY) and the local caller token
# (GATEWAY_LOCAL_TOKEN, sent as X-Gateway-Local-Token) are independent: each example works only
# with both, a swap of either is refused locally with a safe code, and the fake provider's own
# record shows it received the provider key, never the local token, and no request when refused.
if [ -n "${GATEWAY_AUTHFILE:-}" ]; then
  LOCAL_TOKEN="$(jq -r .local_token "$QUAL_SYNTHETIC")"
  base_auth="$GATEWAY_AUTHFILE/v1"
  # run_example <node|python> VAR=value...  (the environment is exactly what the README tells a user to set)
  run_example() {
    lang="$1"; shift
    if [ "$lang" = node ]; then
      (cd "$root/examples/node" && env GATEWAY_BASE_URL="$base_auth" "$@" npm start --silent -- --demo-redaction)
    else
      (cd "$root/examples/python" && env GATEWAY_BASE_URL="$base_auth" "$@" "$py" openai_via_gateway.py --demo-redaction)
    fi
  }
  # The two accepted calls carried the provider key and no trace of the local token.
  verify_dual() {
    verify_calls
    sha="$(printf 'Bearer %s' "$OPENAI_API_KEY" | shasum -a 256 | cut -d' ' -f1)"
    curl -s "$QUAL_ADMIN/__admin/calls" | jq -e --arg sha "$sha" '
      [.calls[] | (.authorization_sha256 == $sha) and (.local_token_seen | not)
        and (.header_names | index("x-gateway-local-token") | not)] | all' >/dev/null
  }
  # refused <lang> <label> <code> VAR=value...: exits non-zero with the safe code, prints neither
  # credential, and the fake provider saw no connection and no request.
  refused() {
    lang="$1"; label="$2"; want="$3"; shift 3
    reset
    if out="$(run_example "$lang" "$@" 2>&1)"; then echo "$lang $label: unexpectedly succeeded" >&2; exit 1; fi
    case "$out" in *"code $want"*) ;; *) echo "$lang $label: expected code $want" >&2; exit 1 ;; esac
    case "$out" in *"$LOCAL_TOKEN"*|*"$OPENAI_API_KEY"*) echo "$lang $label: output exposes a credential" >&2; exit 1 ;; esac
    curl -s "$QUAL_ADMIN/__admin/calls" | jq -e '.connections == 0 and (.calls | length) == 0' >/dev/null
    echo "$lang $label -> refused locally ($want), nothing reached the provider"
  }

  for lang in node python; do
    echo "== examples/$lang with both credentials against the local-auth gateway"
    reset
    run_example "$lang" GATEWAY_LOCAL_TOKEN="$LOCAL_TOKEN"
    verify_dual
    refused "$lang" "provider key only" local_auth_required GATEWAY_LOCAL_TOKEN=
    refused "$lang" "provider key sent as the local token" local_auth_invalid GATEWAY_LOCAL_TOKEN="$OPENAI_API_KEY"
    refused "$lang" "local token sent as the provider key" local_auth_required OPENAI_API_KEY="$LOCAL_TOKEN" GATEWAY_LOCAL_TOKEN=
  done
fi

# Responses examples (#87): two provider calls to /v1/responses (JSON, then stream), sanitized,
# the stream flag on the second only, each ended by the provider's terminal event.
verify_responses_calls() {
  curl -s "$QUAL_ADMIN/__admin/calls" | jq -e '
    (.calls | length) == 2
    and ([.calls[].path] | all(. == "/v1/responses"))
    and ([.calls[].body] | all(contains("<SECRET_1>") and (contains("ghp_") | not) and contains("\"store\":false")))
    and (.calls[1].body | contains("\"stream\":true"))
    and (.calls[0].body | contains("\"stream\"") | not)' >/dev/null
}

echo "== examples/node responses (npm run start:responses -- --demo-redaction)"
reset
(cd "$root/examples/node" && npm run start:responses --silent -- --demo-redaction)
verify_responses_calls
echo "== examples/python responses (--demo-redaction)"
reset
(cd "$root/examples/python" && "$py" responses_via_gateway.py --demo-redaction)
verify_responses_calls
echo "== responses examples: a provider-declared failure and a truncated stream exit 1"
for pair in qual-resp-sse-failed:declared qual-resp-sse-incomplete:declared qual-resp-sse-truncated:terminal qual-resp-sse-clean-no-terminal:terminal; do
  scenario="${pair%%:*}"
  word="${pair##*:}"
  for run in node python; do
    code=0
    if [ "$run" = node ]; then
      (cd "$root/examples/node" && EXAMPLE_MODEL="$scenario" npm run start:responses --silent) >/dev/null 2>"$QUAL_EVIDENCE/example-responses.err" || code=$?
    else
      (cd "$root/examples/python" && EXAMPLE_MODEL="$scenario" "$py" responses_via_gateway.py) >/dev/null 2>"$QUAL_EVIDENCE/example-responses.err" || code=$?
    fi
    test "$code" -eq 1
    # The Node example names a provider-declared outcome (the Python example reports the error
    # class only, by design, so its exit code is the check).
    if [ "$run" = node ] && [ "$word" = declared ]; then grep -q "declared" "$QUAL_EVIDENCE/example-responses.err"; fi
  done
done
rm -f "$QUAL_EVIDENCE/example-responses.err"

echo "examples verified against the qualification build"
