# Alpha 1 candidate image (issues #7 and #22). Not published; registry/name to be selected.
# It runs the gateway with container/config.container.json: health endpoints and the
# POST /v1/chat/completions text-subset proxy to the fixed OpenAI route. Provider keys come
# from the calling application per request; nothing secret is in the image. A local caller token
# must be mounted at run time (see the note on the listener below).
#
# The build context is a staging directory created by scripts/build-image.sh that holds
# only the already-built Linux x86_64 candidate binary and config.container.json. No
# compiler runs here, so the image carries the exact bytes that were smoke-tested.
#
# Base: distroless cc (glibc, no shell, no package manager), pinned by the digest of the
# multi-arch index (resolved 2026-10-02 with `docker buildx imagetools inspect`). The
# glibc runtime matches the x86_64-unknown-linux-gnu binary (ADR 0012). The build is
# pinned to linux/amd64. Dependabot (docker ecosystem) proposes digest updates.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2

COPY --chown=0:0 --chmod=0755 redact-secret-gateway /usr/local/bin/redact-secret-gateway
COPY --chown=0:0 --chmod=0444 config.container.json /etc/redact-secret-gateway/config.json

# The base image's `nonroot` user (uid/gid 65532). No secrets, build args, or env vars.
USER 65532:65532

# Inside the container the listener must bind a non-loopback address so a published port
# can reach it; the config sets `allow_non_loopback` explicitly (ADR 0009). The supported
# model is one application or sidecar: publish only to host loopback or a private
# application network. Exposing this port beyond that is outside the supported model.
#
# The config enforces local caller authentication (#63, ADR 0030): the gateway reads a token
# from /run/secrets/gateway-local-token once at startup and refuses to start without it, and
# the application must send it as `X-Gateway-Local-Token`. Mount the file read-only. It must be
# a regular file of 32 to 128 bytes of A-Za-z0-9-._~, readable by uid 65532 and not readable,
# writable or executable by others (mode 0400 owned by 65532, or 0440 with gid 65532, which is
# also what a Kubernetes Secret volume with `fsGroup: 65532` and `defaultMode: 0440` gives).
# The token is not in the image and not an environment variable.
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/redact-secret-gateway"]
CMD ["serve", "/etc/redact-secret-gateway/config.json"]
