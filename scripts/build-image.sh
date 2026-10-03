#!/bin/sh
# Build the Linux amd64 candidate OCI image from an ALREADY BUILT candidate binary.
# The image contains those exact bytes; nothing is compiled here. Local name only:
# the registry/image name is not selected and nothing is pushed.
# usage: build-image.sh <linux-binary> [image-ref]   (default redact-secret-gateway:candidate)
set -eu
bin="$1"
image="${2:-redact-secret-gateway:candidate}"
platform="${3:-linux/amd64}"
case "$platform" in linux/amd64|linux/arm64) ;; *) echo 'unsupported image platform' >&2; exit 1;; esac
python3 - "$bin" "$platform" <<'PY'
import struct, sys
with open(sys.argv[1], 'rb') as binary:
    head = binary.read(20)
expected = {'linux/amd64': 62, 'linux/arm64': 183}[sys.argv[2]]
if len(head) != 20 or head[:6] != b'\x7fELF\x02\x01' or struct.unpack('<H', head[18:20])[0] != expected:
    raise SystemExit('candidate ELF architecture does not match image platform')
PY
here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
ctx="$(mktemp -d)"
trap 'rm -rf "$ctx"' EXIT
cp "$bin" "$ctx/redact-secret-gateway"
chmod 0755 "$ctx/redact-secret-gateway"
cp "$root/container/config.container.json" "$ctx/config.container.json"
docker build --platform "$platform" --provenance=false --sbom=false \
  -f "$root/container/Dockerfile" -t "$image" "$ctx"
