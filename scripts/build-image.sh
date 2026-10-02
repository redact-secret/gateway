#!/bin/sh
# Build the Linux amd64 candidate OCI image from an ALREADY BUILT candidate binary.
# The image contains those exact bytes; nothing is compiled here. Local name only:
# the registry/image name is not selected and nothing is pushed.
# usage: build-image.sh <linux-binary> [image-ref]   (default redact-secret-gateway:candidate)
set -eu
bin="$1"
image="${2:-redact-secret-gateway:candidate}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
ctx="$(mktemp -d)"
trap 'rm -rf "$ctx"' EXIT
cp "$bin" "$ctx/redact-secret-gateway"
chmod 0755 "$ctx/redact-secret-gateway"
cp "$root/container/config.container.json" "$ctx/config.container.json"
docker build --platform linux/amd64 --provenance=false --sbom=false \
  -f "$root/container/Dockerfile" -t "$image" "$ctx"
