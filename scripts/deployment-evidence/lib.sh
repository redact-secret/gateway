#!/usr/bin/env bash
# Sourced helpers for the deployment-chain evidence scripts (issue #44).
# Requires: bash, docker, openssl. Nothing here touches the network beyond pulling the pinned
# helper images, and nothing publishes a payload or credential: clients print only status
# codes, fixed error codes, and counters.
set -euo pipefail

DE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$DE_DIR/../.." && pwd)"
# Unique per run so parallel runs and leftovers cannot collide.
RUN_ID="rsg44-$$"

# image_ref <stage>: the pinned `name:tag@sha256:digest` for a helper image.
image_ref() {
  awk -v s="$1" 'toupper($1) == "FROM" && $4 == s { print $2 }' "$DE_DIR/images/Dockerfile"
}

# shellcheck source=scripts/lib-sha256.sh
. "$REPO_ROOT/scripts/lib-sha256.sh"

PY_IMAGE="$(image_ref python)"
NGINX_IMAGE="$(image_ref nginx)"
HAPROXY_IMAGE="$(image_ref haproxy)"

# Hardened flags for every container we start (the gateway uses its own documented flags too).
HARDEN=(--read-only --cap-drop ALL --security-opt no-new-privileges)

pull_helpers() {
  docker pull --quiet "$PY_IMAGE" >/dev/null
  docker pull --quiet "$NGINX_IMAGE" >/dev/null
  docker pull --quiet "$HAPROXY_IMAGE" >/dev/null
}

# wait_log <container> <fixed-string> [tries]: poll the container log for a line, failing fast if
# the container exits. Condition-based: returns the moment the line appears.
wait_log() {
  local c="$1" pat="$2" tries="${3:-300}" i=0
  until docker logs "$c" 2>&1 | grep -q -F -- "$pat"; do
    i=$((i + 1))
    if [ "$i" -gt "$tries" ] || [ "$(docker inspect --format '{{.State.Running}}' "$c")" != true ]; then
      echo "container $c did not log '$pat'" >&2
      docker logs "$c" >&2 || true
      return 1
    fi
    sleep 0.1
  done
}

# gen_certs <dir>: throwaway CA and a leaf for api.openai.com signed by it (EC P-256, 2 days).
# The private keys stay in <dir> (a temp dir); only fingerprints are ever recorded.
gen_certs() {
  local d="$1"
  mkdir -p "$d"
  openssl ecparam -name prime256v1 -genkey -noout -out "$d/ca.key" 2>/dev/null
  openssl req -x509 -new -key "$d/ca.key" -sha256 -days 2 \
    -subj "/CN=Synthetic Throwaway Evidence CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out "$d/ca.pem" 2>/dev/null
  openssl ecparam -name prime256v1 -genkey -noout -out "$d/leaf.key" 2>/dev/null
  openssl req -new -key "$d/leaf.key" -subj "/CN=api.openai.com" -out "$d/leaf.csr" 2>/dev/null
  printf '%s\n' "basicConstraints=CA:FALSE" "keyUsage=digitalSignature" \
    "extendedKeyUsage=serverAuth" "subjectAltName=DNS:api.openai.com" >"$d/leaf.ext"
  openssl x509 -req -in "$d/leaf.csr" -CA "$d/ca.pem" -CAkey "$d/ca.key" -CAcreateserial \
    -days 2 -sha256 -extfile "$d/leaf.ext" -out "$d/leaf.pem" 2>/dev/null
  # Containers run as an unprivileged user; the files are throwaway and synthetic.
  chmod 0644 "$d"/*.pem "$d/leaf.key" "$d/ca.key"
}

# record_environment <out-file> <gateway-image>: exact environment and pins, no secrets.
record_environment() {
  local out="$1" gimage="$2"
  {
    echo "recorded_at_utc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host_uname: $(uname -srm)"
    if [ -r /etc/os-release ]; then echo "host_os: $(. /etc/os-release && echo "$PRETTY_NAME")"; fi
    echo "docker_client: $(docker version --format '{{.Client.Version}}')"
    echo "docker_server: $(docker version --format '{{.Server.Version}} (API {{.Server.APIVersion}}, {{.Server.Os}}/{{.Server.Arch}})')"
    echo "docker_kernel: $(docker info --format '{{.KernelVersion}}')"
    echo "docker_os: $(docker info --format '{{.OperatingSystem}}')"
    echo "docker_storage_driver: $(docker info --format '{{.Driver}}')"
    echo "gateway_commit: ${GITHUB_SHA:-$(/usr/bin/git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)}"
    echo "pr_head_commit: ${EVIDENCE_HEAD_SHA:-n/a}"
    echo "core_pin: $(awk '/^name = "redact-secret"$/ {getline; print "redact-secret " $3}' "$REPO_ROOT/Cargo.lock" | tr -d '"')"
    echo "gateway_image: $gimage"
    echo "gateway_image_id: $(docker image inspect --format '{{.Id}}' "$gimage")"
    echo "gateway_image_platform: $(docker image inspect --format '{{.Os}}/{{.Architecture}}' "$gimage")"
    echo "gateway_version: $(docker run --rm "${HARDEN[@]}" "$gimage" --version)"
    local cid tmp
    cid="$(docker create "$gimage")"
    tmp="$(mktemp -d)"
    docker cp "$cid:/usr/local/bin/redact-secret-gateway" "$tmp/gateway-binary" >/dev/null
    docker rm "$cid" >/dev/null
    echo "gateway_binary_sha256: $(sha256_of "$tmp/gateway-binary")"
    rm -rf "$tmp"
    echo "gateway_image_user: $(docker image inspect --format '{{.Config.User}}' "$gimage")"
    for s in python nginx haproxy; do
      ref="$(image_ref "$s")"
      echo "helper_image_$s: $ref"
      echo "helper_image_${s}_local_id: $(docker image inspect --format '{{.Id}}' "$ref")"
    done
    echo "nginx_version: $(docker run --rm "${HARDEN[@]}" --entrypoint nginx "$NGINX_IMAGE" -v 2>&1)"
    echo "haproxy_version: $(docker run --rm "${HARDEN[@]}" "$HAPROXY_IMAGE" haproxy -v 2>&1 | head -1)"
    echo "python_version: $(docker run --rm "${HARDEN[@]}" "$PY_IMAGE" python --version 2>&1)"
    echo "openssl_host: $(openssl version)"
  } >"$out"
}

# cleanup_prefix: remove every container and network this run created.
cleanup_run() {
  local ids
  ids="$(docker ps -aq --filter "name=^${RUN_ID}-" || true)"
  if [ -n "$ids" ]; then docker rm -f $ids >/dev/null 2>&1 || true; fi
  local nets
  nets="$(docker network ls -q --filter "name=^${RUN_ID}-" || true)"
  if [ -n "$nets" ]; then docker network rm $nets >/dev/null 2>&1 || true; fi
}

# jget <json-line> <key>: read one top-level key with python-free shell (values are simple).
jget() {
  printf '%s' "$1" | sed -n "s/.*\"$2\": *\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p"
}

# check <label> <expected> <actual>: record a result line and fail the run on mismatch.
CHECK_FAILED=0
check() {
  if [ "$2" = "$3" ]; then
    echo "PASS $1: $3" | tee -a "${CHECK_LOG:-/dev/null}"
  else
    echo "FAIL $1: expected '$2', got '$3'" | tee -a "${CHECK_LOG:-/dev/null}"
    CHECK_FAILED=1
  fi
}
