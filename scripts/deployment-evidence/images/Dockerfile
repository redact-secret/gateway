# Pinned helper images for the deployment-chain evidence scripts (issue #44). This file is never
# built: scripts/deployment-evidence/lib.sh reads the `FROM <ref> AS <name>` lines so there is one
# place that names each image, and Dependabot (docker ecosystem) proposes digest updates here.
# Digests are the multi-arch index digests, resolved 2026-10-02 with `docker buildx imagetools inspect`.
#   python  - synthetic fake provider, evidence clients, and the network-namespace holder
#   nginx   - HTTP intermediary under test (reverse proxy in front of the gateway)
#   haproxy - second HTTP intermediary under test
FROM python:3.14-alpine@sha256:f6a589d43c42b9e7f7dc67a12d37132491f362859a5d750607710cc56da3bc72 AS python
FROM nginx:1.28-alpine@sha256:a8b39bd9cf0f83869a2162827a0caf6137ddf759d50a171451b335cecc87d236 AS nginx
FROM haproxy:3.0-alpine@sha256:56b887da77428b7a6621e59e480cdbd330cc805c22d3cedb66ceea76ffdea2c6 AS haproxy
