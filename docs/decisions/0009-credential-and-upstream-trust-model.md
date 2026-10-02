# ADR 0009: Credential and upstream trust model

Status: Accepted (design). Implementation status: planned. Companion to ARCHITECTURE.md "Deployment and trust assumptions" and "Credentials and outbound routing".

## Context

The provider needs transport credentials that are not model-bound text. A caller must not be able to steer where those credentials go.

## Decision

Trust model:

- One application trust domain: a localhost companion or a Kubernetes sidecar. A shared internal gateway is future scope.
- The application and Gateway see original plaintext. The provider receives transformed content plus required transport credentials.
- A compromised application can leak before Gateway or bypass it. A compromised Gateway host can read memory and credentials. Process isolation is not encryption.
- Loopback is an address restriction, not caller authentication.
- Alpha listeners default to loopback. Non-loopback exposure needs a separate documented trust model and is not qualified for 0.1.0.

Credentials:

- Alpha uses caller-supplied provider authentication headers, forwarded only to the configured destination, never logged. No key storage, broker, or rotation product.
- Beta 1 adds a local caller token. It is stripped before forwarding and never substitutes for the provider credential.
- Credentials are request-local and are not placed in shared client default headers.

Upstream:

- Static operator-configured origins and exact route matching. Destination is never derived from headers, URL parameters, body values, or absolute-form targets.
- Mandatory upstream TLS certificate and hostname verification. Redirects disabled. No inherited environment proxy. CONNECT and upgrades rejected. Disallowed query strings rejected.
- Header allowlist, hop-by-hop removal, and organization/project header treatment are defined per route by contract in the MVP and routing issues.
- No Gateway retries. SDK retries are SDK behavior and are documented, not hidden. No exactly-once claim.
- Responses are relayed without response redaction. Provider errors may contain sensitive data.
- Direct upstream bypass is prevented only by operator egress controls, not by Gateway.

## Owner

`transport` enforces. `config` validates origins. The maintainer approves any change to trust scope.

## Invariants

1. A credential reaches only the configured origin for the matched route.
2. No caller-controlled value selects the destination or the policy.
3. Redirects are not followed.
4. No credential or body appears in diagnostics.

## Failure behavior

Misconfigured or disallowed destination fails at startup. A disallowed header, query, or method on a request is rejected locally. After a partial upstream transmission, failure terminates without replay.

## Implementation handoff

- #4: config validation for origins and listener.
- #23-#25: routing and credential handling.
- #6: fake-upstream tests for credential isolation, redirects, and header handling in the scope that is implemented.

## Verification

Fake-upstream tests record headers and destinations. Redirect and SSRF tests land with the routing issues.

## Deferred measured choices

HTTP connection-pool tuning and timeout values (ADR 0008). No values here.
