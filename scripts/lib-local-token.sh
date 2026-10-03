#!/bin/sh
# Sourced helper (POSIX sh and bash): create a throwaway local caller token for a smoke or evidence
# run of the candidate image (#63, docs/contracts/local-caller-auth.md).
#
# make_local_token <dir>
#   Writes <dir>/gateway-local-token (43 random base64url characters, mode 0400, owned by the
#   image's `nonroot` uid 65532 so the container can read it and nobody else can) and sets
#   GATEWAY_LOCAL_TOKEN_FILE to that path and GATEWAY_LOCAL_TOKEN to the value, for the clients
#   that must send it. The value is generated per run, is not a provider credential, is never
#   printed, and dies with the runner. Needs `sudo` (passwordless on GitHub-hosted runners) when
#   not run as root, because the file is handed to uid 65532. Mount the file read-only at
#   /run/secrets/gateway-local-token, where container/config.container.json expects it.
make_local_token() {
  mkdir -p "$1"
  GATEWAY_LOCAL_TOKEN_FILE="$(cd "$1" && pwd)/gateway-local-token"
  (umask 077 && head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n' >"$GATEWAY_LOCAL_TOKEN_FILE")
  GATEWAY_LOCAL_TOKEN="$(cat "$GATEWAY_LOCAL_TOKEN_FILE")"
  chmod 0400 "$GATEWAY_LOCAL_TOKEN_FILE"
  if [ "$(id -u)" -eq 0 ]; then
    chown 65532:65532 "$GATEWAY_LOCAL_TOKEN_FILE"
  else
    sudo chown 65532:65532 "$GATEWAY_LOCAL_TOKEN_FILE"
  fi
  export GATEWAY_LOCAL_TOKEN GATEWAY_LOCAL_TOKEN_FILE
}

# The docker flag that mounts the token where the container config reads it.
local_token_mount() {
  printf '%s' "type=bind,src=$GATEWAY_LOCAL_TOKEN_FILE,dst=/run/secrets/gateway-local-token,readonly"
}
