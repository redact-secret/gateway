#!/usr/bin/env bash
# Evidence for an HTTP intermediary in front of the gateway (issue #44, ADR 0019).
# usage: intermediary.sh <gateway-image> <evidence-dir>
#
# Starts the SHIPPED candidate image on an internal network with no provider (framing and
# rejection cases need none; an admitted request fails at the upstream step with a fixed
# `upstream_*` code, which is how "the gateway admitted this" is observed), puts pinned nginx and
# pinned HAProxy in front of it with minimal default configurations, and sends the same
# conformance set (tools/framing_cases.py) three ways: directly to the gateway, through nginx,
# through HAProxy. tools/merge.py then classifies each case.
#
# What this proves: how these two specific intermediary versions, with the checked-in configs,
# treat each framing form, and what the gateway does with whatever reaches it. It does NOT prove
# any other intermediary, version, or configuration safe.
set -euo pipefail
# shellcheck source=scripts/deployment-evidence/lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

gimage="$1"
out="$2"
mkdir -p "$out/framing"
work="$out/framing"
trap 'cleanup_run' EXIT
net="$RUN_ID-net"
docker network create --internal "$net" >/dev/null
# The exact intermediary configurations used are part of the evidence.
cp "$DE_DIR/conf/nginx.conf" "$DE_DIR/conf/haproxy.cfg" "$work/"

gw="$RUN_ID-gateway"
docker run -d --name "$gw" --network "$net" --network-alias gateway "${HARDEN[@]}" "$gimage" >/dev/null
wait_log "$gw" listening

ng="$RUN_ID-nginx"
docker run -d --name "$ng" --network "$net" --network-alias nginx --user 101:101 "${HARDEN[@]}" \
  --tmpfs /tmp -v "$DE_DIR/conf/nginx.conf:/etc/nginx/nginx.conf:ro" "$NGINX_IMAGE" nginx -g 'daemon off;' >/dev/null
hp="$RUN_ID-haproxy"
docker run -d --name "$hp" --network "$net" --network-alias haproxy --user 99:99 "${HARDEN[@]}" \
  -v "$DE_DIR/conf/haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" "$HAPROXY_IMAGE" >/dev/null

# Readiness: each intermediary answers its own sentinel (served by the intermediary itself).
runner() { docker run --rm --network "$net" --user "$(id -u):$(id -g)" "${HARDEN[@]}" -v "$DE_DIR/tools:/tools:ro" -v "$work:/out" "$PY_IMAGE" python /tools/framing_cases.py "$@"; }
ready() { # <container> <host> <port>
  local i=0
  until docker run --rm --network "$net" "${HARDEN[@]}" "$PY_IMAGE" python -c \
    "import socket,sys; s=socket.create_connection(('$2',$3),timeout=2); s.sendall(b'GET /__sentinel/ready HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n'); sys.exit(0 if b' 204 ' in s.recv(64) else 1)" 2>/dev/null; do
    i=$((i + 1))
    if [ "$i" -gt 100 ] || [ "$(docker inspect --format '{{.State.Running}}' "$1")" != true ]; then
      echo "intermediary $1 did not become ready" >&2
      docker logs "$1" >&2 || true
      exit 1
    fi
  done
}
ready "$ng" nginx 8080
ready "$hp" haproxy 8081

echo "== direct to the gateway"
runner gateway 8787 /out/direct.jsonl
echo "== through nginx"
runner nginx 8080 /out/nginx.jsonl --sentinel
echo "== through haproxy"
runner haproxy 8081 /out/haproxy.jsonl --sentinel

docker logs "$ng" >"$work/nginx.log" 2>&1
docker logs "$hp" >"$work/haproxy.log" 2>&1
printf 'nginx\nhaproxy\n' >"$work/intermediaries.txt"
docker run --rm --user "$(id -u):$(id -g)" "${HARDEN[@]}" -v "$DE_DIR/tools:/tools:ro" -v "$work:/out" "$PY_IMAGE" python /tools/merge.py /out | tee "$out/framing-results.md" >/dev/null

# The conformance run itself must have completed every case on all three paths.
for f in direct nginx haproxy; do
  n="$(wc -l <"$work/$f.jsonl" | tr -d ' ')"
  check "$f path completed all cases" 36 "$n"
done
# Gateway invariants that must hold directly (not intermediary-dependent): the ambiguous-framing
# heads are closed with no response, and Connection: close is answered and honored.
direct_of() { docker run --rm "${HARDEN[@]}" -v "$work:/out:ro" "$PY_IMAGE" python -c "
import json,sys
for l in open('/out/direct.jsonl'):
    r=json.loads(l)
    if r['id']==sys.argv[1]:
        print(len(r['responses']), r['eof'], r.get('after_response'), (r['responses'] or [{}])[0].get('connection'))
" "$1"; }
check "direct: CL+TE closes with no response" "0 True None None" "$(direct_of c02-cl-and-te-cl-first)"
check "direct: pipelined pair yields one response, then the connection closes" "1 True None close" "$(direct_of c29-pipelined-pair)"
check "direct: keep-alive request is answered Connection: close and closed" "1 False closed close" "$(direct_of c35-connection-close-behavior)"

# The evidence directory keeps the raw per-case JSON and logs (sanitized by construction: the logs
# contain request lines and status codes only, never bodies or credentials). Scan to be sure.
if grep -rl 'sk-SYNTHETIC' "$work/nginx.log" "$work/haproxy.log" >/dev/null 2>&1; then
  echo "FAIL intermediary logs contain the synthetic key" >&2
  CHECK_FAILED=1
fi
if [ "$CHECK_FAILED" -ne 0 ]; then
  echo "intermediary: FAILED"
  exit 1
fi
echo "intermediary: run complete"
