#!/bin/sh
# Run the SDK qualification suites against the NON-RELEASE qualification build (ADR 0020).
#
#   sh qualification/run-suites.sh [--binary PATH] [--evidence DIR] [--suites node,python,examples|none]
#   QUAL_COMMAND='shell line' runs one extra command against the same stack.
#
# Starts the scripted fake provider and ten gateway instances of the qualification binary
# (standard, tight limits, no upstream, dead provider, overload, the Alpha 2 policy and concurrency instances, and the two authenticated Beta 1 instances; the Chat and Responses suites share them, #88), runs the pinned-SDK suites against
# them, stops everything gracefully, and scans the captured gateway stdout/stderr for any
# synthetic marker. Everything is synthetic and loopback only; no secret, key, or network
# access beyond 127.0.0.1 is used. Exit 0 only if every suite and every check passed.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
bin="$here/target/debug/redact-secret-gateway-qualification"
evidence="$here/evidence"
suites="node,python,examples"
# Optional extra command run against the same stack (measurement drivers): QUAL_COMMAND.
command_line="${QUAL_COMMAND:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --binary) bin="$2"; shift 2 ;;
    --evidence) evidence="$2"; shift 2 ;;
    --suites) suites="$2"; shift 2 ;;
    *) echo "unknown argument" >&2; exit 2 ;;
  esac
done
test -x "$bin"
mkdir -p "$evidence"
evidence="$(cd "$evidence" && pwd)"
rm -f "$evidence"/gateway-*.out "$evidence"/gateway-*.err "$evidence"/fake-provider.out

pids=""
gateway_names="standard tight noupstream deadprovider overload policyforward policycommon concurrent authfile authenv"
cleanup() {
  for p in $pids; do kill "$p" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

wait_for_line() { # file pattern
  i=0
  until grep -q "$2" "$1" 2>/dev/null; do
    i=$((i + 1))
    if [ "$i" -gt 200 ]; then echo "timed out waiting for $2 in $(basename "$1")" >&2; exit 1; fi
    sleep 0.1
  done
}

# 1. Fake provider.
node "$here/fake-provider/server.mjs" >"$evidence/fake-provider.out" 2>"$evidence/fake-provider.err" &
provider_pid=$!
pids="$pids $provider_pid"
wait_for_line "$evidence/fake-provider.out" provider_port
provider_port="$(jq -r .provider_port "$evidence/fake-provider.out")"
admin_port="$(jq -r .admin_port "$evidence/fake-provider.out")"

# 2. Gateways (qualification binary only; the shipped binary cannot do this by design).
start_gateway() { # name config provider-port [NAME=value for the gateway process only]
  env ${4:-QUAL_UNUSED=1} "$bin" serve "${cfg_of:-$here/configs/$2.json}" --fake-provider "127.0.0.1:$3" \
    >"$evidence/gateway-$1.out" 2>"$evidence/gateway-$1.err" &
  eval "gw_$1_pid=$!"
  pids="$pids $!"
  wait_for_line "$evidence/gateway-$1.out" '^listening '
}
start_gateway standard standard "$provider_port"
start_gateway tight tight "$provider_port"
start_gateway noupstream noupstream "$provider_port"
start_gateway deadprovider standard 1 # nothing listens on port 1: provider unreachable
start_gateway overload overload "$provider_port" # one upstream and one stream permit
# Alpha 2 policy instances (#57): same pinned core, different static content policy.
start_gateway policyforward policy-forward "$provider_port" # full profile, on_warn = forward
start_gateway policycommon policy-common "$provider_port" # common profile (narrower detector set)
start_gateway concurrent concurrent "$provider_port" # wider capacity for the isolation runs

# Beta 1 local caller authentication instances (#65), one per delivery mechanism, with different
# static content policy: `authfile` (token mounted as a mode-0600 file, full profile) and
# `authenv` (token from the process environment of that one gateway only, common profile,
# on_warn forward). The token values are the synthetic ones in synthetic.json.
token_file="$evidence/local-auth-token"
(umask 077 && jq -j .local_token "$here/synthetic.json" >"$token_file")
sed "s|@TOKEN_FILE@|$token_file|" "$here/configs/local-auth-file.json" >"$evidence/local-auth-file.json"
cfg_of="$evidence/local-auth-file.json" start_gateway authfile local-auth-file "$provider_port"
cfg_of="" start_gateway authenv local-auth-env "$provider_port" "QUAL_LOCAL_TOKEN=$(jq -r .local_token "$here/synthetic.json")"

addr_of() { sed -n 's/^listening //p' "$evidence/gateway-$1.out" | head -n 1; }
metrics_of() { sed -n 's/^qualification-metrics //p' "$evidence/gateway-$1.out" | head -n 1; }

QUAL_ADMIN="http://127.0.0.1:$admin_port"
QUAL_PROVIDER="http://127.0.0.1:$provider_port" # direct control path, never used by the gateway tests
GATEWAY_STANDARD="http://$(addr_of standard)"
GATEWAY_TIGHT="http://$(addr_of tight)"
GATEWAY_NOUPSTREAM="http://$(addr_of noupstream)"
GATEWAY_DEADPROVIDER="http://$(addr_of deadprovider)"
GATEWAY_OVERLOAD="http://$(addr_of overload)"
GATEWAY_POLICYFORWARD="http://$(addr_of policyforward)"
GATEWAY_POLICYCOMMON="http://$(addr_of policycommon)"
GATEWAY_CONCURRENT="http://$(addr_of concurrent)"
GATEWAY_AUTHFILE="http://$(addr_of authfile)"
GATEWAY_AUTHENV="http://$(addr_of authenv)"
QUAL_METRICS_STANDARD="http://$(metrics_of standard)"
QUAL_SYNTHETIC="$here/synthetic.json"
QUAL_BINARY="$bin"
case "$bin" in */release/*) QUAL_PROFILE=release ;; *) QUAL_PROFILE=debug ;; esac
QUAL_EVIDENCE="$evidence"
export QUAL_BINARY QUAL_PROFILE QUAL_ADMIN QUAL_PROVIDER GATEWAY_STANDARD GATEWAY_TIGHT GATEWAY_NOUPSTREAM GATEWAY_DEADPROVIDER GATEWAY_OVERLOAD \
  GATEWAY_POLICYFORWARD GATEWAY_POLICYCOMMON GATEWAY_CONCURRENT GATEWAY_AUTHFILE GATEWAY_AUTHENV \
  QUAL_METRICS_STANDARD QUAL_SYNTHETIC QUAL_EVIDENCE

# 3. Suites.
status=0
case ",$suites," in
  *,node,*)
    echo "== Node.js/TypeScript SDK suite"
    (cd "$here/sdk/node" && npm test) || status=1
    ;;
esac
case ",$suites," in
  *,python,*)
    echo "== Python SDK suite"
    py="${QUAL_PYTHON:-$here/sdk/python/.venv/bin/python}"
    (cd "$here/sdk/python" && "$py" -m unittest discover -v -s tests -p 'test_*.py') || status=1
    ;;
esac

case ",$suites," in
  *,examples,*)
    echo "== README examples (Node and Python) against the qualification build"
    sh "$here/run-examples.sh" || status=1
    ;;
esac

if [ -n "$command_line" ]; then
  echo "== custom command against the same stack"
  sh -c "$command_line" || status=1
fi

# 4. Graceful stop: each gateway must exit 0 and report a clean shutdown.
for name in $gateway_names; do
  eval "pid=\$gw_${name}_pid"
  kill -TERM "$pid" 2>/dev/null || true
  code=0
  wait "$pid" || code=$?
  if [ "$code" -ne 0 ] || ! grep -q 'shutdown complete' "$evidence/gateway-$name.out"; then
    echo "gateway $name did not shut down cleanly (exit $code)" >&2
    status=1
  fi
done
kill -TERM "$provider_pid" 2>/dev/null || true
wait "$provider_pid" 2>/dev/null || true
pids=""

# 5. The captured gateway output must never contain a synthetic marker, key, prompt text,
#    provider reply, or provider error text (the "no leak in logs" check). The qualification
#    banner proves the output came from the qualification binary.
patterns="$evidence/forbidden-patterns.txt"
jq -r '.log_forbidden[]' "$QUAL_SYNTHETIC" >"$patterns"
leaks=0
for f in "$evidence"/gateway-*.out "$evidence"/gateway-*.err; do
  if grep -F -f "$patterns" "$f" >/dev/null 2>&1; then
    echo "SYNTHETIC MARKER FOUND in $(basename "$f")" >&2
    leaks=1
  fi
done
rm -f "$patterns" "$token_file"
[ "$leaks" -eq 0 ] || status=1
for name in $gateway_names; do
  grep -q 'QUALIFICATION BUILD' "$evidence/gateway-$name.err" || { echo "missing banner for $name" >&2; status=1; }
done
bytes="$(cat "$evidence"/gateway-*.out "$evidence"/gateway-*.err | wc -c | tr -d ' ')"
files="$(ls "$evidence"/gateway-*.out "$evidence"/gateway-*.err | wc -l | tr -d ' ')"
echo "gateway output scanned: $bytes bytes in $files files, markers found: $leaks"

if [ "$status" -eq 0 ]; then echo "qualification suites passed"; else echo "qualification suites FAILED" >&2; fi
exit "$status"
