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

echo "examples verified against the qualification build"
