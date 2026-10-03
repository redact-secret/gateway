# ADR 0030: Local caller token, listener and health authority

Status: Accepted (design), by maintainer delegation to the #61 author under epic #12. Implementation status: **planned** (#63 enforces; #86 shares the boundary with the Responses route; #62/#64 freeze the schema and compatibility; #65 qualifies with Node and Python). Date: 2026-10-03. Builds on [ADR 0006](0006-runtime-plan-and-authority-separation.md), [ADR 0009](0009-credential-and-upstream-trust-model.md), [ADR 0016](0016-header-allowlists-and-request-local-credentials.md), [ADR 0019](0019-request-head-guard-and-one-request-per-connection.md), [ADR 0022](0022-connection-bound-at-accept.md). Normative rules: [local-caller-auth contract](../contracts/local-caller-auth.md).

## Context

Alpha 1 has no caller authentication. Loopback is an address restriction: any process on the host that can reach the port can use the gateway, with its own provider key (README, SECURITY.md). Beta 1 adds a separate local caller token for the single-application trust domain. ADR 0016 reserved the `X-Gateway-Local-*` namespace and required a distinct type and module, with `Authorization` left to the provider credential. This ADR fixes the header, token syntax, comparison, secret delivery, ordering, listener combinations, and health exposure before #63 writes code. Epic #12 non-goals stand: no OAuth, user database, token minting, provider API-key management, shared tenancy, or inbound TLS.

## Decision

1. **Header.** `X-Gateway-Local-Token`, one field line, bare token (no scheme). It never uses `Authorization`.
2. **Token syntax.** Unreserved characters (RFC 3986): `A-Za-z0-9-._~`, 32 to 128 bytes. Excludes `+ / =` and whitespace so it cannot be confused with a provider key or a comma list.
3. **One failure surface.** Absent or removed by `Connection`: `401 local_auth_required`. Duplicate (even identical), combined, malformed, out of bounds, or wrong: `401 local_auth_invalid`. Same body, status, and headers for every invalid cause. No `WWW-Authenticate`. No `400`: ambiguity in an authentication header is an authentication failure.
4. **Comparison.** Bounded fixed-width constant-time equality with `subtle::ConstantTimeEq` over 128-byte zero-padded buffers plus a constant-time length comparison. `subtle` is already in the lock file through the TLS stack; #63 pins it as a direct dependency (ADR 0011 policy, `cargo deny` unchanged). No `==` on token bytes, no early return on content, no hashing step that adds a second secret-dependent path.
5. **Secret delivery.** Reference, never value: `deployment.local_auth.token` is exactly one of `{"env": NAME}` or `{"file": ABSOLUTE_PATH}`. Mounted file is recommended (Kubernetes Secret volume, Docker/Compose secret), environment is allowed with the documented same-user exposure. No inline value, no CLI argument, no config default. Files: open then `fstat` the handle, regular file, at most 256 bytes, no "other" permission bits on Unix, one trailing newline stripped.
6. **Immutable startup resolution.** Resolved in config validation before bind (also by `validate-config`), held in an immutable `LocalToken` with redacted `Debug` and no `Clone`/`Display`/`Serialize`, never reloaded. Rotation is restart. Every failure is `invalid_config` at a static location and echoes no name, path, or content.
7. **Mode and default.** `deployment.local_auth.mode` is `"disabled"` or `"token"`; the object absent means `disabled`. Auth-disabled is a supported mode only on a loopback listener and remains the default so the Alpha single-application setup keeps working. When disabled the local header is ignored and never forwarded.
8. **Non-loopback needs more than the acknowledgement.** `allow_non_loopback: true` additionally requires `mode: "token"`, otherwise startup fails with `invalid_combination`. Even then the deployment is not a supported shared or remote one (decision 11).
9. **Order.** After the head guard and exact route/method matching, local authentication is the first gateway decision on a proxy route, before content-type and framing validation, `vet_inbound`, the `100 Continue`, admission, reservation, body read, inspection, and upstream. One shared boundary for all proxy routes, so #86 cannot add a route that skips it.
10. **Probes are separate authority.** `GET /healthz` and `GET /readyz` need no token, ignore it, and keep their frozen bodies; they never disclose config, address, destination, version, profile, capacity, whether auth is enabled, or secret state. No admin, metrics, or debug route exists or is added; a future one needs its own ADR and authority and cannot accept the proxy token.
11. **Trust limits stated, not hidden.** Same-host and same-Pod processes that can read the token source or share the user are inside the boundary. The local hop is plain HTTP, so token and provider key are readable on that path. There is no lockout, per-caller identity, TLS, or tenancy. A shared gateway, an inbound TLS or auth platform, and multi-tenancy are outside the release; fronting the gateway with a TLS proxy does not change that.
12. **Provider authority untouched.** The local token never substitutes for `Authorization`, never reaches upstream (the outbound allowlist cannot carry it), cannot change destination, profile, policy, or limits, and no policy, profile, or client hint can weaken it.

## Owner

`transport` (`local_auth` module, one authentication boundary used by every proxy route), `config` (`deployment.local_auth` parse and startup resolution into the immutable runtime plan, ADR 0006), `telemetry` (two new `SafeCode` spellings). The maintainer approves any change to the names, syntax bounds, or the listener combination table.

## Threat review

| Threat | Control | Residual |
| --- | --- | --- |
| Token theft from config, repo, or image | Config holds only a reference; unknown keys (including `token`, `value`) are rejected at every depth; no CLI argument; examples are synthetic | The token source (file, environment) is a secret at rest and must be protected by the operator |
| Token theft from the process environment | `file` recommended; `env` documented as visible to same-user processes and dump tooling; process spawns no children | Environment delivery is weaker; same-user processes can read it |
| Token theft from logs, errors, `Debug`, snapshots | `LocalToken` redacted `Debug`; no echo of value, length, reference, or candidate; fixed error bodies; only a coarse rejection counter | Secure erasure not promised (ADR 0007): allocator and OS buffers may retain copies |
| Token sniffed on the local hop | None possible: the listener is plain HTTP | Documented. Loopback and same-Pod networking are the only supported paths; non-loopback is acknowledged as unsupported |
| Local untrusted caller | Token required in `token` mode before any body read, budget, or upstream contact; rejects consume constant bounded work and `Connection: close`; `max_connections` bounds sockets | Another process with the same user or token-source access is trusted. `disabled` mode (loopback default) leaves every same-host process able to call with its own provider key. No lockout |
| Header ambiguity (duplicates, comma lists, `Bearer`, whitespace, case, folding, `Connection` nomination) | Exactly one field line, restricted alphabet, case-insensitive name only, nomination treated as removal, all invalid forms collapse to one outcome; folding and invalid names are rejected by the HTTP layer; framing ambiguity by the head guard (ADR 0019) | An intermediary that rewrites headers could still change the outcome; intermediaries must meet the existing framing requirements |
| Timing side channel | Fixed-width constant-time comparison with constant work; identical failure response | Network timing noise and the HTTP layer's own behavior are not controlled; bounded by token entropy |
| Confused deputy between local and provider credentials | Different header, different type, different module; local token never forwarded; `Authorization` never read for local auth; separate error codes | None identified |
| Secret-loading failure (unset, empty, unreadable, wrong mode, oversize, malformed, torn write) | Failure at startup before bind, no listener, no readiness, no fallback to disabled; single open and `fstat` of the handle; no re-read | An unreadable token on a restart takes the gateway down (the intended fail-closed outcome) |
| Config downgrade (policy, profile, client hint, header, query) | Authority is deployment-only (`deployment.local_auth`); content and resource keys cannot reach it; no per-request override; variation tests owed | None identified |
| Health exposure | Fixed bodies, no token, no config or secret state, no upstream probe, no admin route | An unauthenticated caller learns that a gateway process is serving |
| Brute force | At least 32 bytes accepted, 256-bit generation recommended; constant cost per attempt; close after every rejection | No rate limit; weak operator-chosen 32-character tokens are the operator's risk |
| Auth-before-parse resource attack | Authentication needs no body, permit, or inspection; rejection precedes all of them | Head guard and connection bound still apply first and observably |

## Invariants

1. A request that fails local authentication has body bytes read: none; permits or budget reserved: none; upstream contact: none.
2. The local token and the provider credential never share a type, a header, or an error code, and neither substitutes for the other.
3. The local token never appears in a request-state type, an outbound header, a log, an error, a telemetry label, a snapshot, or documentation.
4. Authentication mode, token, and listener rules come only from deployment configuration, resolved once at startup; no content, resource, header, or client input changes them.
5. A token that cannot be resolved at startup prevents the listener; there is no silent fallback to `disabled`.

## Failure behavior

Rejections are local, with the fixed bodies and the codes in the contract, followed by connection close. Startup failures print `invalid_config: <kind> at deployment.local_auth...` and exit 1 with nothing bound.

## Implementation handoff

- #63: `transport::local_auth` (`LocalToken`, `LocalAuth`, the shared boundary function), config keys and combination checks, `SafeCode::{LocalAuthRequired, LocalAuthInvalid}`, direct pinned `subtle`, tests listed in the contract, update `headers-and-credentials`, `errors-and-telemetry`, `configuration`, README, SECURITY.md, and flip statuses to implemented. The container config uses a non-loopback bind and must gain a token mount when the combination rule lands.
- #86: route table entries pass through the #63 boundary; no per-route authentication code.
- #62/#64: add `deployment.local_auth` to the schema and compatibility notes (additive to `schema_version` 1 while unreleased; an unreleased-build exception, after release it needs the documented compatibility procedure).
- #65: Node and Python qualification of both credentials, negative auth cases, and token absence upstream.
- #89: the Kubernetes sidecar contract must state that the token Secret is mounted only into the application and gateway containers and that probes need no secret.

## Verification

This issue changes documentation only. Verified by review against the code: `src/transport/headers.rs` ignores `X-Gateway-Local-*` and builds outbound headers from an allowlist; the health routes return the fixed bodies in `docs/configuration.md`; config rejects unknown keys at every depth and rejects non-loopback without `allow_non_loopback`; `subtle` is in `Cargo.lock`. Implementation tests are listed in the contract and are owed by #63.

## Deferred choices

Whether to add a rate limit or lockout, a second accepted token for zero-downtime rotation, a file-watch reload, or per-route tokens is not decided and is not planned for Beta 1; each would need its own ADR (rotation is a restart in this release).

## Implementation status

Planned. Nothing in this ADR is implemented; the shipped behavior remains Alpha (no caller authentication, loopback default).
