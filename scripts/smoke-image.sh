#!/bin/sh
# Smoke-test the candidate OCI image by running it with Docker (Linux amd64 runner).
# usage: smoke-image.sh <image> <expected-binary-sha256> <evidence-dir>
# Checks: the binary inside the image is byte-identical to the candidate binary, the
# image runs as a non-root user, the image's own config validates, the container serves the
# health endpoints through a loopback-only published port, rejects the documented proxy-route
# probes locally (never forwarding; see probe-endpoints.sh), and exits cleanly on SIGTERM. Runs
# with a read-only root filesystem, no capabilities, no new privileges. The image config enforces a
# local caller token, so the run mounts a throwaway token file (lib-local-token.sh) and the probes
# cover the missing and wrong token cases as well.
set -eu
image="$1"
want_sha="$2"
out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/lib-sha256.sh
. "$here/lib-sha256.sh"
# shellcheck source=scripts/lib-local-token.sh
. "$here/lib-local-token.sh"
mkdir -p "$out"
name="rsg-candidate-smoke"
docker rm -f "$name" >/dev/null 2>&1 || true

# 1. Same bytes as the candidate binary.
cid="$(docker create "$image")"
docker cp "$cid:/usr/local/bin/redact-secret-gateway" "$out/binary-from-image"
docker rm "$cid" >/dev/null
got_sha="$(sha256_of "$out/binary-from-image")"
echo "binary sha256 in image: $got_sha" | tee "$out/image-smoke.txt"
sh "$here/check-no-qualification-seam.sh" "$out/binary-from-image" | tee -a "$out/image-smoke.txt"
rm -f "$out/binary-from-image"
test "$got_sha" = "$want_sha"

# 2. Non-root by image configuration.
user="$(docker inspect --format '{{.Config.User}}' "$image")"
echo "image Config.User: $user" | tee -a "$out/image-smoke.txt"
case "$user" in
  "" | 0 | root | 0:* | root:*) echo "image runs as root" >&2; exit 1 ;;
esac

# 2b. The image's own static config validates with the image's own binary.
# The shipped config enforces local caller authentication (#63), so it names a token file that the
# deployment must mount. Without the mount it must fail closed, and with it it must validate.
tokdir="$(mktemp -d)"
trap 'rm -rf "$tokdir"' EXIT
make_local_token "$tokdir"
if docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges "$image" \
  validate-config /etc/redact-secret-gateway/config.json >"$out/no-token.txt" 2>&1; then
  echo "the image config validated without a local token mounted" >&2
  exit 1
fi
grep -q 'invalid_config: unreadable at deployment.local_auth.token' "$out/no-token.txt"
rm -f "$out/no-token.txt"
echo "image config without a mounted token: refused (fail closed)" | tee -a "$out/image-smoke.txt"
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  --mount "$(local_token_mount)" "$image" \
  validate-config /etc/redact-secret-gateway/config.json | tee -a "$out/image-smoke.txt"

# 3. Run: published to host loopback only, hardened runtime flags.
docker run -d --name "$name" \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --mount "$(local_token_mount)" \
  -p 127.0.0.1:8787:8787 "$image" >/dev/null
i=0
until docker logs "$name" 2>/dev/null | grep -q '^listening '; do
  i=$((i + 1))
  if [ "$i" -gt 100 ] || [ "$(docker inspect --format '{{.State.Running}}' "$name")" != true ]; then
    docker logs "$name" >"$out/container.log" 2>&1 || true
    echo "container did not start" >&2
    exit 1
  fi
  sleep 0.1
done

# 4. The running process is not uid 0 (the image has no shell; read the host-visible PID).
pid="$(docker inspect --format '{{.State.Pid}}' "$name")"
uid="$(awk '/^Uid:/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo unknown)"
echo "container process uid: $uid" | tee -a "$out/image-smoke.txt"
test "$uid" != 0

GATEWAY_LOCAL_TOKEN="$GATEWAY_LOCAL_TOKEN" sh "$here/probe-endpoints.sh" "http://127.0.0.1:8787" "$out" | tee -a "$out/image-smoke.txt"

# 5. Graceful stop: docker stop sends SIGTERM first; the exit code must be 0.
docker stop --time 10 "$name" >/dev/null
code="$(docker inspect --format '{{.State.ExitCode}}' "$name")"
echo "container exit code after SIGTERM: $code" | tee -a "$out/image-smoke.txt"
docker logs "$name" >"$out/container.log" 2>&1 || true
docker rm "$name" >/dev/null
test "$code" = 0
grep -q 'shutdown complete' "$out/container.log"
echo "image smoke OK" | tee -a "$out/image-smoke.txt"
