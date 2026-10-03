# Contract: local caller authentication, listener and health authority

Status: **planned (frozen design, #61; not implemented).** Decision and threat review: [ADR 0030](../decisions/0030-local-caller-auth-listener-health.md). Implementation owner: #63 (token type, startup resolution, enforcement before body collection); #86 applies the same boundary to the Responses route; #62 and #64 carry the schema and compatibility rules for the new config keys. Until #63 lands, none of the names below exist in code, any caller that can reach the listener is served, and the shipped behavior is the one in [headers-and-credentials](headers-and-credentials.md).

This contract fixes names, syntax, ordering, and outcomes so that #63, #62, #65, and #86 do not each invent them. A change to any name here is an ADR 0030 amendment.

## Scope and non-goals

The local caller token answers one question: "may this local process use this gateway instance?" It is shared-secret authentication for the single-application trust domain (one application plus its localhost companion or same-Pod sidecar). It is not identity: there is one token, no user, no tenant, no scope, no expiry. No OAuth, no user database, no token minting, no provider API-key management, no shared or multi-tenant gateway, no inbound TLS. It does not defend against another process running as the same operating-system user, or an operator, who can read the token source (ADR 0030, threat review).

## Names (frozen)

| Item | Name |
| --- | --- |
| Request header | `X-Gateway-Local-Token` (HTTP names are case-insensitive; inside the reserved `X-Gateway-Local-*` namespace, `LOCAL_AUTHORITY_PREFIX`) |
| Config object | `deployment.local_auth` (deployment authority) |
| Config keys | `deployment.local_auth.mode` (`"disabled"` or `"token"`), `deployment.local_auth.token` (object with exactly one of `env`, `file`) |
| Error codes | `local_auth_required`, `local_auth_invalid` (new `SafeCode` spellings) |
| Planned types | `LocalToken` (secret, in `transport::local_auth`), `LocalAuth` (immutable plan field: `Disabled` or `Token(LocalToken)`) |
| Constant-time primitive | `subtle::ConstantTimeEq` (crate `subtle`, already in `Cargo.lock` through the TLS stack; #63 promotes it to an exact-pinned direct dependency under the ADR 0011 policy) |

No token value, no token length, and no secret reference value ever appears in config examples, CLI arguments, logs, errors, `Debug`, snapshots, or this documentation. Examples use the synthetic placeholder `<SYNTHETIC_LOCAL_TOKEN>`.

## Token syntax

- Alphabet: RFC 3986 unreserved characters only, `A-Z a-z 0-9 - . _ ~`.
- Length: 32 to 128 bytes inclusive. A token source shorter than 32 is a startup failure; 32 is a floor, not a recommendation. Generate at least 32 random bytes encoded as base64url without padding (43 characters, about 256 bits) from a CSPRNG.
- No scheme prefix. The header value is the bare token. `Bearer <token>` is malformed (the space is outside the alphabet).
- The characters `+ / =` are excluded on purpose (not valid in the token alphabet, unlike the provider `Authorization` token alphabet), so a provider key or a base64 standard value pasted by mistake does not validate.

## Request header rules (`X-Gateway-Local-Token`)

Evaluated only when `mode` is `"token"`. When `mode` is `"disabled"` the header is ignored, not validated, and never forwarded, like any other `X-Gateway-Local-*` field today.

| Input | Outcome |
| --- | --- |
| Header absent | `401 local_auth_required` |
| Header nominated by `Connection` (hop-by-hop removal) | Treated as removed: `401 local_auth_required` |
| Two or more field lines with this name, even identical | `401 local_auth_invalid` |
| One field line whose value contains a comma, whitespace, a quote, a control byte, any byte outside the alphabet, or is empty | `401 local_auth_invalid` |
| Value longer than 128 bytes or shorter than 32 bytes | `401 local_auth_invalid` |
| Well-formed value that is not equal to the configured token | `401 local_auth_invalid` |
| Well-formed value equal to the configured token | Authenticated; the request continues to the checks in [Ordering](#ordering) |

All failure shapes after "absent" return the same status, code, body, and headers, so a caller cannot tell a wrong token from a malformed one beyond what it already knows about what it sent. There is no `400` for this header: ambiguity is an authentication failure, not a body problem. `WWW-Authenticate` is not sent (it would advertise `Bearer`, which is the provider scheme). `Authorization` is never read for local authentication, with or without the local header. `X-Gateway-Local-Token` is never forwarded, never copied into any request-state type, and is not part of `VettedHeaders`.

### Comparison

The candidate is copied into a zero-filled 128-byte buffer after the length bound check; the configured token is stored in an identical zero-filled buffer with its length. Equality is `ct_eq` over both buffers combined with `ct_eq` over the two lengths, evaluated unconditionally, so run time does not depend on the matching prefix or the configured length. Input is bounded (header values already cap at 8,192 bytes; the token is rejected beyond 128 before comparison) and the work per attempt is constant. The result is a boolean consumed locally; the candidate buffer is not logged, not stored, and not placed in any error. (Secure erasure is not promised: SECURITY.md, ADR 0007.)

### Responses

Local-auth rejections are generated locally, before anything is sent upstream, with the fixed body `{"error":{"code":"<code>"}}`, `Content-Type: application/json`, `Cache-Control: no-store`, and `Connection: close`. The body is never read. No response contains a header value, a length, a reference name, or a hint about the expected token.

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| Local token absent or removed by `Connection` | 401 | `local_auth_required` | no |
| Local token duplicate, malformed, out of bounds, or wrong | 401 | `local_auth_invalid` | no |

`401 missing_credential` (provider `Authorization`) and the local codes are distinct; the provider credential code never reports a local-authentication result and the reverse.

## Ordering

For a `POST` to a served proxy route (today `/v1/chat/completions`, with Responses added by #86 through the same boundary), after the connection-level head guard (ADR 0019: framing ambiguity and oversized heads) and exact route/method matching, the first gateway decision is local authentication. It runs before: `Content-Type`, `Content-Length` and `Transfer-Encoding` validation, `vet_inbound`, the `Expect: 100-continue` acknowledgement (no `100 Continue` is sent to an unauthenticated caller), admission queueing, any budget or permit reservation, any body read, parse, inspection, serialization, and any upstream contact. Consequences: an unauthenticated caller consumes no memory budget, inspection worker, or upstream permit, and never causes a body byte to be read.

Unauthenticated callers can still observe, with no authority: head-guard `400`/`431` and the connection bound (a connection over `max_connections` closes at accept), `404`/`405` for routes and methods that are not served, and the health endpoints below. Method and route matching does not depend on a secret.

The check is one boundary shared by every proxy route (ADR 0005 central enforcement): a route cannot be added to the table without passing it, and #86 must not re-implement it.

## Authority separation

1. **Provider `Authorization` stays provider authority.** It is vetted by `vet_inbound` exactly as in [headers-and-credentials](headers-and-credentials.md), request-local, forwarded only to the route's fixed destination. Authenticating locally neither supplies nor substitutes for it; a missing provider credential after successful local authentication is still `401 missing_credential`.
2. **The local token never reaches the provider.** The outbound header set is an allowlist (`WIRE_HEADER_NAMES`); the local header is not on it and #63 adds a test that a valid local token and a decoy never appear on the wire, in telemetry, or in any error.
3. **The local token cannot choose anything.** Not destination, route, profile, policy, limits, or retry. Content profile, `content.on_warn`, PII selection, request policy, and client hints (any header, field, or query) cannot enable, disable, weaken, or bypass local authentication or the listener rules (config variation tests in #63, per the #12 invariant).
4. A single token authenticates the whole gateway instance. There is no per-route, per-request, or per-client token and no header that carries a token identity.

## Configuration

Planned, additive to `schema_version` 1 while unreleased; the schema artifact and compatibility rules are #62 and #64.

```json
{
  "deployment": {
    "listener": { "address": "127.0.0.1:8787" },
    "local_auth": { "mode": "token", "token": { "file": "/run/secrets/gateway-local-token" } }
  }
}
```

| Key | Rules |
| --- | --- |
| `deployment.local_auth` | Optional object. **Absent means `disabled`** (the Alpha behavior), which is supported only on a loopback listener (below). Unknown keys at any depth are rejected, as for every config object. |
| `local_auth.mode` | Required when the object is present: `"disabled"` or `"token"`. `"disabled"` forbids `token`; `"token"` requires it. Explicit `"disabled"` exists so a reviewed config can record the choice. |
| `local_auth.token` | Object with **exactly one** of `env` or `file`; both or neither is `invalid_combination`. It is a reference, never a value: the token itself is not an accepted config key, and no inline value, CLI argument, or default exists. |
| `token.env` | A variable name matching `[A-Z_][A-Z0-9_]{0,63}`. Read once at startup. The process does not rewrite its environment. The environment is visible to same-user processes on many systems (for example `/proc/<pid>/environ`) and to anything that dumps it, so `file` is the recommended delivery. |
| `token.file` | An absolute path (no relative path, no `~`, no variable expansion, no URL). Opened once at startup without a size or time-of-check gap: open, then `fstat` the open handle. Must be a regular file (a symlink is followed once by the OS open and the handle is then checked, not the link), at most 256 bytes read, and on Unix must not be readable, writable, or executable by "other" (mode bits `0o007` clear; group access is allowed so Kubernetes `fsGroup` mounts work). A single trailing `\n` or `\r\n` is removed; any other whitespace is invalid. Mounted-file delivery (Kubernetes Secret volume, Docker or Compose secret) is the recommended path. |

### Startup resolution

Resolution runs during configuration validation, before the listener is bound, and is immutable for the process lifetime: no hot reload, no re-read on failure, no request-time lookup (rotation is a restart, consistent with static restart-activated configuration). `validate-config` performs the same resolution and checks, prints only the existing success line, and never prints any token-derived value, name, or path.

Every failure is `invalid_config: <kind> at deployment.local_auth.token` with a static location and no echo of the variable name, path, or content, then exit 1 and no listener:

| Failure | Kind |
| --- | --- |
| Both or neither of `env`/`file`; `token` present with `mode: "disabled"`, or missing with `"token"` | `invalid_combination` / `missing_field` |
| `env` name malformed, `file` not absolute, unknown key | `invalid_value` / `unknown_field` |
| Variable unset or empty, file missing, unreadable, not a regular file, over 256 bytes, mode check failed | `unreadable` (variable unset or empty, and a file that cannot be read, deliberately share one kind) or `invalid_value` (not a regular file, over 256 bytes, mode) |
| Token outside the alphabet, shorter than 32 or longer than 128 bytes | `invalid_value` |

The token read from a source is held only in `LocalToken` (no `Clone`, `Default`, `Display`, `Serialize`, or equality operator; `Debug` prints a fixed `LocalToken(<redacted>)`), is in the immutable runtime plan, and is never placed in a request-state type or shared client defaults.

## Listener and deployment combinations

| `listener.address` | `allow_non_loopback` | `local_auth.mode` | Result |
| --- | --- | --- | --- |
| loopback | absent / false | absent or `disabled` | **Supported (Alpha behavior).** Loopback is an address restriction, not authentication: any same-host process can call the gateway with its own provider key |
| loopback | absent / false | `token` | **Supported, recommended.** Same-host callers must hold the token |
| loopback | `true` | any | Rejected at startup, as today (`invalid_combination`) |
| non-loopback (including wildcard) | absent / false | any | Rejected at startup, as today |
| non-loopback | `true` | absent or `disabled` | **Rejected at startup** (`invalid_combination` at `deployment.local_auth.mode`). New in #63 |
| non-loopback | `true` | `token` | Starts. **Not a supported shared or remote deployment.** It is acceptable only where the network path to the listener is limited to the same trust domain, for example the container shape published to host loopback, or one Pod's network namespace |

Acknowledgement plus a token does not make a deployment secure and cannot be documented as doing so. The listener speaks plain HTTP: the token and the provider key cross the local hop in the clear to anything on that path. The gateway has no inbound TLS, no rate limiting or lockout, no per-caller identity, and no multi-tenant isolation; a shared or remote gateway, an inbound TLS or authentication platform, and multi-tenancy are outside this release. An intermediary placed in front remains subject to the framing requirements in [headers-and-credentials](headers-and-credentials.md) and does not make remote exposure supported. Same-host and same-Pod processes that can read the token source, or that share the user, are inside the trust boundary and are not defended against.

Online guessing is bounded by the 128-bit-or-better recommended token strength, constant work per attempt, `Connection: close` on every rejection, and the `max_connections` bound; there is no lockout, and weak operator-chosen tokens are the operator's risk (the 32-byte floor rejects the obvious ones).

## Health, readiness and admin authority

Proxy authority and operational probes are separate authorities.

| Route | Authority | Behavior |
| --- | --- | --- |
| `GET /healthz` | None. Never requires the token, ignores it if sent | `200 {"status":"live"}` whenever the process serves |
| `GET /readyz` | None. Never requires the token, ignores it if sent | `200 {"status":"ready"}` or `503 {"error":{"code":"not_ready"}}`, exactly as today |
| `POST` proxy routes | Local token when `mode` is `token` | This contract |
| Admin, metrics, debug, config, or any other route | **None exists and none is added by this contract.** Anything else is `404 unsupported_input` as today | A future operator surface needs its own ADR and its own authority; it cannot reuse the proxy token, and the proxy token cannot reach it |

Health and readiness bodies are fixed and frozen: they never contain configuration, version, address, listener mode, destination, profile, capacity, whether local authentication is enabled, token source kind, or any secret state. Readiness does not turn false on authentication failures and does not distinguish "token not configured" from "token configured": a token that cannot be resolved fails startup and no listener exists to answer. Health makes no upstream call and uses no credential. Because health is unauthenticated, it reveals to anyone who can connect only that a gateway process is serving; that is accepted, and it is one reason non-loopback exposure stays unsupported. Probes (for example Kubernetes `httpGet`) can therefore run without the secret. Health endpoints do not read a body and are method-limited as today.

## Telemetry and logging

The gateway emits no request logs today and #63 adds none containing request data. Permitted: a coarse counter of local-auth rejections by the two codes, with no peer address, header length, or value. Excluded: the token, a prefix or hash of it, its length, its source reference (variable name or path), and the candidate value. The token must not appear in `Debug`, panics, error chains, diagnostics, snapshots, fixtures, or documentation.

## Documentation of limits (to be repeated in README, SECURITY, configuration when #63 lands)

- Same-host and same-Pod trust: every container or process that can read the token source or share the user is trusted. Mount the token only into the application and gateway containers.
- Local HTTP: no confidentiality or integrity on the local hop.
- Loopback is not authentication; a token is the only caller check, and it is optional on loopback.

## Verification owed by #63 (and #65)

Unit and served-stack tests: every row of the header table (absent, nominated, duplicate identical and different, comma list, whitespace, over and under bounds, out-of-alphabet, `Bearer` form, wrong, correct, case-varied header name); rejection before body read (a body that never arrives, a declared huge length, `Expect: 100-continue` receiving no `100`), before any permit or reservation (capacity counters unchanged), and before upstream (fake upstream sees nothing); token never on the wire and never in responses or `Debug`; provider `Authorization` alone does not authenticate and the local token alone does not satisfy `missing_credential`; every startup combination and failure kind including no echo of reference names or paths; mode bits; health and readiness identical with and without the header and with auth enabled; config variation tests (profile, `on_warn`, PII, limits) leaving authentication unchanged; comparison helper handles every length 0 to beyond 128 without panic and its code path has no early return on content. #65 demonstrates both credentials independently through the pinned Node and Python SDKs (the SDKs send the token through `default_headers`/`defaultHeaders`; the provider key stays in the SDK's `api_key`).
