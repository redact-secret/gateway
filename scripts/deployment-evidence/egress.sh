#!/usr/bin/env bash
# Evidence for the direct-upstream-bypass control, localhost-companion shape (issue #44).
# usage: egress.sh <gateway-image> <evidence-dir>
#
# What this proves: the OPERATOR's network policy (here a Docker `--internal` network that has no
# route out), not the gateway, stops an application from reaching the provider directly. The
# application container is attached only to an internal network that also holds the gateway; the
# provider (a synthetic TLS fake with a throwaway CA, at a public-looking address on a second
# internal network) is reachable only from the gateway. The test asserts:
#   1. gateway-routed traffic works end to end (200, synthetic answer relayed);
#   2. every direct connection attempt from the application fails at the network layer;
#   3. the provider saw exactly one connection, from the gateway, and none from the application;
#   4. negative control: the same direct connection from a container that is NOT behind the policy
#      succeeds, so the probe can detect a bypass.
# No real provider, no Internet, no credential: both networks are `--internal` and every payload
# is synthetic. 93.184.216.0/24 is used only because the gateway's address policy requires a
# public-looking provider address; the network is internal, so no packet leaves the Docker host.
set -euo pipefail
# shellcheck source=scripts/deployment-evidence/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

gimage="$1"
out="$2"
mkdir -p "$out"
out="$(cd "$out" && pwd)" # absolute: used as a docker bind-mount source
work="$(mktemp -d)"
trap 'cleanup_run; rm -rf "$work"' EXIT
log="$out/egress.txt"
CHECK_LOG="$log"
: >"$log"
say() { echo "$*" | tee -a "$log"; }

PROVIDER_IP=93.184.216.34
gen_certs "$work/certs"
make_local_token "$work/token"
appnet="$RUN_ID-app"
upnet="$RUN_ID-up"
docker network create --internal "$appnet" >/dev/null
docker network create --internal --subnet 93.184.216.0/24 "$upnet" >/dev/null

# Trust store with the throwaway CA appended (the "provider" stands in for the real one).
cid="$(docker create "$gimage")"
docker cp "$cid:/etc/ssl/certs/ca-certificates.crt" "$work/system-bundle.crt"
docker rm "$cid" >/dev/null
cat "$work/system-bundle.crt" "$work/certs/ca.pem" >"$work/bundle.crt"
chmod 0644 "$work/bundle.crt"

provider="$RUN_ID-provider"
docker run -d --name "$provider" --network "$upnet" --ip "$PROVIDER_IP" \
  --network-alias api.openai.com "${HARDEN[@]}" \
  -v "$work/certs:/certs:ro" -v "$DE_DIR/tools:/tools:ro" "$PY_IMAGE" \
  python /tools/tls_stand_in.py serve --cert /certs/leaf.pem --key /certs/leaf.key >/dev/null
wait_log "$provider" listening

gw="$RUN_ID-gateway"
docker create --name "$gw" --network "$appnet" --network-alias gateway "${HARDEN[@]}" \
  -v "$work/bundle.crt:/etc/ssl/certs/ca-certificates.crt:ro" \
  --mount "$(local_token_mount)" "$gimage" >/dev/null
docker network connect "$upnet" "$gw"
docker start "$gw" >/dev/null
wait_log "$gw" listening
gw_up_ip="$(docker inspect --format "{{(index .NetworkSettings.Networks \"$upnet\").IPAddress}}" "$gw")"

app="$RUN_ID-app"
docker run -d --name "$app" --network "$appnet" "${HARDEN[@]}" \
  -v "$DE_DIR/tools:/tools:ro" "$PY_IMAGE" tail -f /dev/null >/dev/null
appc() { docker exec -e LOCAL_TOKEN="$GATEWAY_LOCAL_TOKEN" "$app" python /tools/client.py "$@"; }

say "== Application behind the operator policy (attached only to an internal network with the gateway)"
res="$(appc post http://gateway:8787/v1/chat/completions)"
say "app -> gateway -> provider: $res"
check "gateway-routed request status" 200 "$(jget "$res" status)"
check "gateway-routed request relayed the synthetic answer" true "$(jget "$res" relayed)"

for target in "$PROVIDER_IP 443" "169.254.169.254 80" "1.1.1.1 443" "8.8.8.8 53" "10.0.0.1 443"; do
  # shellcheck disable=SC2086
  res="$(appc connect $target)"
  say "direct attempt: $res"
  case "$(jget "$res" result)" in
    error:*) check "direct $target fails at the network layer" yes yes ;;
    *) check "direct $target fails at the network layer" yes "no ($res)" ;;
  esac
done
res="$(appc resolve api.openai.com)"
say "app name resolution of the provider name: $res"
case "$res" in
  *'"result": "error:'*) check "app cannot resolve the provider name" yes yes ;;
  *) check "app cannot resolve the provider name" yes "no ($res)" ;;
esac

stats="$(docker exec "$provider" python /tools/tls_stand_in.py sync)"
say "provider counters after the application's attempts: $stats"
check "provider connections (gateway only)" 1 "$(jget "$stats" accepted)"
check "provider requests served" 1 "$(jget "$stats" requests)"
peer_ok=yes
case "$stats" in
  *"\"peers\": {\"$gw_up_ip\": 1}"*) ;;
  *) peer_ok="no ($stats; gateway=$gw_up_ip)" ;;
esac
check "provider peer is the gateway's upstream-side address" yes "$peer_ok"

say "== Negative control: the same direct attempt from a container NOT behind the policy"
ctl="$RUN_ID-control"
docker run -d --name "$ctl" --network "$upnet" "${HARDEN[@]}" \
  -v "$DE_DIR/tools:/tools:ro" "$PY_IMAGE" tail -f /dev/null >/dev/null
res="$(docker exec "$ctl" python /tools/client.py connect "$PROVIDER_IP" 443)"
say "control direct attempt: $res"
check "control reaches the provider directly (probe can detect a bypass)" connected "$(jget "$res" result)"
stats="$(docker exec "$provider" python /tools/tls_stand_in.py sync)"
check "provider counted the control connection" 2 "$(jget "$stats" accepted)"

if [ "$CHECK_FAILED" -ne 0 ]; then
  say "egress: FAILED"
  exit 1
fi
say "egress: all checks passed"
