#!/bin/sh
# Reproducible configuration, local-auth, readiness, migration and rollback checks against an
# ACTUAL candidate binary (#65; docs/contracts/config-upgrade-rollback.md, local-caller-auth.md).
# usage: smoke-local-auth.sh <binary> <evidence-dir>
#
# Everything is synthetic and loopback only. The exact binary can only reach the real HTTPS
# provider, so this script NEVER sends a request that passes validation: "authenticated" is
# observed as `401 missing_credential` (local authentication passed, then the missing provider
# credential refused the request before any body was read or any upstream contact; no request here
# carries a provider Authorization header). It proves startup,
# readiness, ordering, restart activation and rollback of the shipped bytes; it does not exercise
# an accepted request (that is the fake-upstream qualification build, ADR 0020).
set -eu
bin="$1"
out="$2"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
test -x "$bin"
work="$out/local-auth"
rm -rf "$work"
mkdir -p "$work"
chmod 700 "$work"

TOKEN_A="SYNTHETIC-smoke-local-token-A-0123456789-abcdefgh"
TOKEN_B="SYNTHETIC-smoke-local-token-B-9876543210-hgfedcba"
pid=""
trap '[ -z "$pid" ] || kill "$pid" 2>/dev/null || true' EXIT INT TERM

note() { echo "== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

write_token() { (umask 077 && printf '%s\n' "$2" >"$1"); }

# config <name> <local_auth-json-or-empty> <listener-json>
config() {
  la=""
  [ -z "$2" ] || la=", \"local_auth\": $2"
  cat >"$work/$1.json" <<EOT
{"schema_version": 1,
 "deployment": {"listener": $3, "upstream": {"provider": "openai"}$la},
 "content": {"profile": "full"},
 "resources": {"capacity": {"receipt": 4, "memory_units": 8192, "inspection": 1, "upstream": 2, "stream": 2}}}
EOT
}
LOOP='{"address": "127.0.0.1:0"}'
file_auth() { printf '{"mode": "token", "token": {"file": "%s"}}' "$1"; }

start() { # config-file [VAR=value]  -> sets pid and addr
  : >"$work/serve.out"; : >"$work/serve.err"
  env ${2:-SMOKE_UNUSED=1} "$bin" serve "$1" >"$work/serve.out" 2>"$work/serve.err" &
  pid=$!
  i=0
  until grep -q '^listening ' "$work/serve.out" 2>/dev/null; do
    i=$((i + 1))
    if [ "$i" -gt 100 ] || ! kill -0 "$pid" 2>/dev/null; then fail "server did not start ($1)"; fi
    sleep 0.1
  done
  addr="$(sed -n 's/^listening //p' "$work/serve.out" | head -n 1)"
}
stop() {
  kill -TERM "$pid"
  st=0
  wait "$pid" || st=$?
  pid=""
  [ "$st" -eq 0 ] || fail "gateway did not exit 0 after SIGTERM"
}
# post <extra-header> -> status on stdout, body in $work/body. NEVER carries a provider
# Authorization header: a request that passes local authentication then stops at
# 401 missing_credential, so no request can ever reach the provider from this script.
post() {
  curl -s -o "$work/body" -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -H "$1" --data '{"model":"m","messages":[{"role":"user","content":"hello"}]}' "http://$addr/v1/chat/completions"
}
expect() { # label status code header
  got="$(post "$4")"
  echo "$1 -> $got"
  [ "$got" = "$2" ] || fail "$1: expected $2"
  grep -q "\"code\":\"$3\"" "$work/body" || fail "$1: expected code $3"
  if grep -q -F -e "$TOKEN_A" -e "$TOKEN_B" "$work/body"; then fail "$1: response echoed a token"; fi
}
health() {
  for r in healthz readyz; do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://$addr/$r")" = 200 ] || fail "$r not 200"
  done
}
refuse_start() { # label config [VAR=value]: exit 1, no listener, nothing echoed
  label="$1"; shift
  st=0
  env ${2:-SMOKE_UNUSED=1} "$bin" serve "$1" >"$work/r.out" 2>"$work/r.err" || st=$?
  [ "$st" -eq 1 ] || fail "$label: expected exit 1, got $st"
  if grep -q '^listening ' "$work/r.out"; then fail "$label: announced a listener"; fi
  if grep -q -F -e "$TOKEN_A" -e "$TOKEN_B" -e "$work" "$work/r.out" "$work/r.err"; then fail "$label: output echoed a token or path"; fi
  echo "$label -> refused before readiness: $(head -n 1 "$work/r.err")"
}

note "shipped example and reference configs load through the compiled loader"
for f in "$root"/examples/config.*.json; do
  "$bin" validate-config "$f" >/dev/null || fail "validate-config $f"
  echo "validate-config $(basename "$f") ok"
done

note "Alpha-shaped config (no local_auth): starts, auth disabled, loopback only"
config alpha '' "$LOOP"
"$bin" validate-config "$work/alpha.json" >/dev/null
start "$work/alpha.json"
health
expect "alpha, no local header" 401 missing_credential 'X-Smoke: none'
stop

note "upgrade: add a token file reference, restart"
write_token "$work/token" "$TOKEN_A"
config beta "$(file_auth "$work/token")" "$LOOP"
"$bin" validate-config "$work/beta.json" >/dev/null
start "$work/beta.json"
health
expect "beta, no local header" 401 local_auth_required 'X-Smoke: none'
expect "beta, wrong token" 401 local_auth_invalid "X-Gateway-Local-Token: $TOKEN_B"
expect "beta, malformed (Bearer prefix)" 401 local_auth_invalid "X-Gateway-Local-Token: Bearer $TOKEN_A"
expect "beta, right token, no provider credential refused after auth" 401 missing_credential "X-Gateway-Local-Token: $TOKEN_A"
# The provider Authorization header is not a substitute for the local header.
got="$(curl -s -o "$work/body" -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $TOKEN_A" --data '{}' "http://$addr/v1/chat/completions")"
[ "$got" = 401 ] && grep -q '"code":"local_auth_required"' "$work/body" || fail "provider header substituted for the local header"
echo "local token as provider Authorization -> 401 local_auth_required"

note "restart activation: rewriting the token file changes nothing until the restart"
write_token "$work/token" "$TOKEN_B"
expect "running gateway, old token still accepted" 401 missing_credential "X-Gateway-Local-Token: $TOKEN_A"
expect "running gateway, new token not yet active" 401 local_auth_invalid "X-Gateway-Local-Token: $TOKEN_B"
stop
start "$work/beta.json"
health
expect "after restart, new token accepted" 401 missing_credential "X-Gateway-Local-Token: $TOKEN_B"
expect "after restart, old token refused" 401 local_auth_invalid "X-Gateway-Local-Token: $TOKEN_A"
stop

note "token from the environment"
config beta-env '{"mode": "token", "token": {"env": "SMOKE_LOCAL_TOKEN"}}' "$LOOP"
start "$work/beta-env.json" "SMOKE_LOCAL_TOKEN=$TOKEN_A"
expect "env token accepted" 401 missing_credential "X-Gateway-Local-Token: $TOKEN_A"
expect "env token wrong" 401 local_auth_invalid "X-Gateway-Local-Token: $TOKEN_B"
stop

note "rollback: the Beta file minus local_auth is exactly the Alpha file; on loopback it means disabled"
start "$work/alpha.json"
expect "rolled back, no local header needed" 401 missing_credential "X-Gateway-Local-Token: $TOKEN_A"
stop

note "unsupported or unresolvable combinations are refused before readiness"
config nonloop-noauth '' '{"address": "0.0.0.0:0", "allow_non_loopback": true}'
refuse_start "non-loopback acknowledged, no token (rollback of the container shape)" "$work/nonloop-noauth.json"
config nonloop-unack '' '{"address": "0.0.0.0:0"}'
refuse_start "non-loopback without acknowledgement" "$work/nonloop-unack.json"
config missing-file "$(file_auth "$work/absent")" "$LOOP"
refuse_start "token file missing" "$work/missing-file.json"
write_token "$work/open-token" "$TOKEN_A"
chmod 0644 "$work/open-token"
config open-file "$(file_auth "$work/open-token")" "$LOOP"
refuse_start "token file readable by other" "$work/open-file.json"
write_token "$work/short-token" "too-short"
config short-file "$(file_auth "$work/short-token")" "$LOOP"
refuse_start "token shorter than 32 bytes" "$work/short-file.json"
config env-unset '{"mode": "token", "token": {"env": "SMOKE_LOCAL_TOKEN_UNSET"}}' "$LOOP"
refuse_start "token variable unset" "$work/env-unset.json"
config both '{"mode": "token", "token": {"env": "SMOKE_LOCAL_TOKEN", "file": "/run/secrets/x"}}' "$LOOP"
refuse_start "env and file both given" "$work/both.json" "SMOKE_LOCAL_TOKEN=$TOKEN_A"
config inline '{"mode": "token", "token": {"value": "x"}}' "$LOOP"
refuse_start "inline token value" "$work/inline.json"
config older '' "$LOOP"
sed 's/"schema_version": 1/"schema_version": 2/' "$work/older.json" >"$work/newer.json"
refuse_start "newer schema_version" "$work/newer.json"

rm -rf "$work"
echo "local-auth lifecycle smoke OK"
