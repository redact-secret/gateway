#!/usr/bin/env bash
# Evidence for the resolver/DNS answer and trust-store/TLS-interception controls (issue #44).
# usage: resolver-trust.sh <gateway-image> <evidence-dir>
#
# Runs the SHIPPED candidate image (no test seam, no provider-specific flag). The fake provider is
# a throwaway TLS server that shares a network namespace with the gateway. That namespace has NO
# external network (`--network none`): its only addresses are loopback and a few extra addresses
# the holder adds to `lo`, so nothing here can reach a real provider or the Internet.
#
# The gateway resolves `api.openai.com` through the container's /etc/hosts (bind-mounted per case),
# which is the system resolver path the policy resolver uses. Each case maps the name to one
# address class and asserts (1) the gateway's fixed error code and (2) the fake provider's exact
# connection counter: for refused answers the provider must see ZERO connection attempts, so the
# refusal happened before any connect, not as a connect failure.
set -euo pipefail
# shellcheck source=scripts/deployment-evidence/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

gimage="$1"
out="$2"
mkdir -p "$out"
work="$(mktemp -d)"
trap 'cleanup_run; rm -rf "$work"' EXIT
log="$out/resolver-trust.txt"
CHECK_LOG="$log"
: >"$log"
say() { echo "$*" | tee -a "$log"; }

gen_certs "$work/certs"
holder="$RUN_ID-holder"
FAKE_V4=93.184.216.34 # a public address per the gateway policy; exists only on the holder's lo

docker run -d --name "$holder" --network none --read-only --security-opt no-new-privileges \
  --cap-drop ALL --cap-add NET_ADMIN --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -v "$work/certs:/certs:ro" -v "$DE_DIR/tools:/tools:ro" "$PY_IMAGE" sh -c '
    set -eu
    for a in 10.77.0.1 172.31.0.1 192.168.77.1 169.254.169.254 100.64.0.1 198.18.0.1 '"$FAKE_V4"'; do
      ip addr add "$a/32" dev lo
    done
    ip -6 addr add fd00:ec2::254/128 dev lo
    exec python /tools/fake_provider.py serve --cert /certs/leaf.pem --key /certs/leaf.key --ipv6' >/dev/null
wait_log "$holder" listening

# The trust store used by the "CA installed" cases: the image's own bundle plus the throwaway CA.
cid="$(docker create "$gimage")"
docker cp "$cid:/etc/ssl/certs/ca-certificates.crt" "$work/system-bundle.crt"
docker rm "$cid" >/dev/null
cat "$work/system-bundle.crt" "$work/certs/ca.pem" >"$work/bundle-with-throwaway-ca.crt"
chmod 0644 "$work/bundle-with-throwaway-ca.crt"

sync_stat() { docker exec "$holder" python /tools/fake_provider.py sync; }
post() { docker exec "$holder" python /tools/client.py post http://127.0.0.1:8787/v1/chat/completions; }

# run_case <label> <hosts-ips (space separated)> <trust: system|with-ca> <want-code> <want-accept-delta> <want-tls-failed-delta> <want-requests-delta>
run_case() {
  local label="$1" ips="$2" trust="$3" want_code="$4" want_acc="$5" want_tls="$6" want_req="$7"
  local hosts="$work/hosts-$label" gw="$RUN_ID-gw-$label" before after res
  : >"$hosts"
  echo "127.0.0.1 localhost" >>"$hosts"
  local ip
  for ip in $ips; do echo "$ip api.openai.com" >>"$hosts"; done
  local mounts=(-v "$hosts:/etc/hosts:ro")
  if [ "$trust" = with-ca ]; then
    mounts+=(-v "$work/bundle-with-throwaway-ca.crt:/etc/ssl/certs/ca-certificates.crt:ro")
  fi
  before="$(sync_stat)"
  docker run -d --name "$gw" --network "container:$holder" "${HARDEN[@]}" "${mounts[@]}" "$gimage" >/dev/null
  wait_log "$gw" listening
  res="$(post)"
  after="$(sync_stat)"
  docker rm -f "$gw" >/dev/null
  local d_acc d_tls d_req
  d_acc=$(($(jget "$after" accepted) - $(jget "$before" accepted)))
  d_tls=$(($(jget "$after" tls_failed) - $(jget "$before" tls_failed)))
  d_req=$(($(jget "$after" requests) - $(jget "$before" requests)))
  say "case $label: answer=[$ips] trust=$trust -> gateway status=$(jget "$res" status) code=$(jget "$res" code) relayed=$(jget "$res" relayed); provider delta: accepted=$d_acc tls_failed=$d_tls requests=$d_req"
  check "$label gateway code" "$want_code" "$(jget "$res" code)"
  check "$label provider accepted" "$want_acc" "$d_acc"
  check "$label provider tls_failed" "$want_tls" "$d_tls"
  check "$label provider requests" "$want_req" "$d_req"
}

say "== Resolver answers the gateway must refuse (provider must see zero connections)"
run_case loopback-v4 "127.0.0.1" system upstream_unavailable 0 0 0
run_case loopback-v6 "::1" system upstream_unavailable 0 0 0
run_case private-10 "10.77.0.1" system upstream_unavailable 0 0 0
run_case private-172 "172.31.0.1" system upstream_unavailable 0 0 0
run_case private-192 "192.168.77.1" system upstream_unavailable 0 0 0
run_case link-local-metadata-v4 "169.254.169.254" system upstream_unavailable 0 0 0
run_case metadata-v6 "fd00:ec2::254" system upstream_unavailable 0 0 0
run_case cgnat "100.64.0.1" system upstream_unavailable 0 0 0
run_case benchmark "198.18.0.1" system upstream_unavailable 0 0 0
run_case mixed-public-and-private "$FAKE_V4 10.77.0.1" system upstream_unavailable 0 0 0

say "== Controls: a public answer is accepted by the policy, so the counters can move"
run_case control-public-system-trust "$FAKE_V4" system upstream_tls_failure 1 1 0

say "== Trust store and TLS interception (the provider's certificate chains to a throwaway CA)"
say "The fake provider stands in for a TLS-intercepting proxy: it answers for api.openai.com with a"
say "certificate signed by a CA the gateway's platform trust store does not contain."
run_case intercepted-ca-not-installed "$FAKE_V4" system upstream_tls_failure 1 1 0
run_case intercepted-ca-installed "$FAKE_V4" with-ca null 1 0 1

say "throwaway CA sha256 fingerprint: $(openssl x509 -in "$work/certs/ca.pem" -noout -fingerprint -sha256)"
say "system bundle sha256 (from the gateway image): $(openssl dgst -sha256 "$work/system-bundle.crt" | awk '{print $NF}')"

if [ "$CHECK_FAILED" -ne 0 ]; then
  say "resolver-trust: FAILED"
  exit 1
fi
say "resolver-trust: all checks passed"
