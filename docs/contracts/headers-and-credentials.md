# Contract: headers, framing, and provider credentials

Status: implemented for inbound vetting, the request-local credential, outbound wire-header construction, and the response-header allowlist (#24, [ADR 0016](../decisions/0016-header-allowlists-and-request-local-credentials.md)). Sending the request and relaying ordinary JSON responses is implemented (#20, [ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md)). **Planned:** the local caller token (Beta 1 #12), frozen in [local-caller-auth](local-caller-auth.md) ([ADR 0030](../decisions/0030-local-caller-auth-listener-health.md), #61) and enforced by #63. Code: `src/transport/headers.rs`, `src/transport/credential.rs`, `Upstream::outbound`.

## Principles

1. Allowlists, not denylists. The outbound header set is **built**, never copied from the inbound request.
2. The provider credential is request-local transport authority. It is not model text, never inspected or transformed, never stored in shared state, and never logged.
3. Local gateway authority (Beta 1) and the provider credential are different things with different headers and types. Neither substitutes for the other.
4. Framing (`Host`, `Content-Length`, `Transfer-Encoding`) on the outbound request is regenerated from the validated deployment destination and the exact sanitized bytes.

## Inbound request headers (`vet_inbound`)

"Ignored" means read by nothing and therefore never forwarded; the request is still served. "Rejected" means a local error before any body capacity is reserved.

| Header | Treatment | Rationale |
| --- | --- | --- |
| `Authorization` | **Required**, exactly one, `Bearer <token>` (scheme case-insensitive, one space, token of `A-Za-z0-9 - . _ ~ + / =`, 1 to 512 bytes). Absent: `401 missing_credential`. Duplicate (even identical), other scheme, or malformed: `400 malformed_input` | The Alpha credential model is caller-supplied provider auth (ADR 0009). Duplicates are ambiguous (which one would the provider honor?). A restricted alphabet excludes comma lists, whitespace, quotes, and control bytes |
| `OpenAI-Organization`, `OpenAI-Project` | **Optional**, at most one each, `[A-Za-z0-9_.-]{1,128}`, forwarded verbatim. Repeated or malformed: `400 malformed_input` | They select the caller's billing and project context at the provider, the same trust level as the key. Constraining syntax prevents header injection and smuggling of other values |
| `Content-Type` | Must be exactly one `application/json` (optional `charset=utf-8`), else `415`. Outbound is the fixed `application/json` | Fixed by the route admission (#18); the caller's spelling is never copied |
| `Content-Length`, `Transfer-Encoding` | Validated by route admission (#18, [errors](errors-and-telemetry.md)). Never forwarded; outbound length is regenerated | See Framing below |
| `Content-Encoding` | Rejected (`415`) | Compressed bodies cannot be inspected |
| `Connection` | Parsed. Each token is a header the caller says is hop-by-hop and is treated as **removed** (a nominated `Authorization` means the credential is missing: `401`; nominated organization/project are dropped). A malformed token, or a token naming `Host`, `Content-Length`, `Transfer-Encoding`, `Content-Type`, `Content-Encoding`, `Expect`, or `Upgrade`: `400 malformed_input`. Never forwarded | RFC 9110 section 7.6.1. Nominating framing headers makes the request ambiguous |
| `Upgrade` | Rejected (`400 unsupported_input`) | No upgrades (ARCHITECTURE) |
| `Expect` | `100-continue` accepted (answered by the HTTP layer, never forwarded). Any other value: `417 unsupported_input` | RFC 9110 section 10.1.1 |
| `Host` | Ignored. Outbound `Host` comes only from the reviewed destination URL | The caller cannot choose the origin |
| `User-Agent` | Ignored. Outbound is `redact-secret-gateway/<version>` | Caller UA is arbitrary, fingerprints the caller, and can carry data |
| `Accept` | Ignored. Outbound is `application/json, text/event-stream` | Fixed; streaming relay is #21 ([ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md)) |
| `Accept-Encoding` | Ignored. Outbound is `identity` | The gateway does not decode provider responses, so it must not ask for a coding |
| `TE`, `Trailer`, `Keep-Alive`, `Proxy-*`, `Forwarded`, `X-Forwarded-*`, `X-Real-IP`, `Via`, `Cookie`, `X-Api-Key`, `api-key`, SDK telemetry (`X-Stainless-*`, ...), `OpenAI-Beta`, any other header | Ignored (not forwarded) | Strip is safer for SDK and proxy compatibility than reject, and an allowlist cannot forward what it does not name |
| `X-Gateway-Local-*` (reserved) | Ignored and never forwarded | Local-authority namespace, see below |
| Header bytes | Total names+values over 16,384 bytes, or any value over 8,192 bytes: `431 limit_exceeded` | Bounded before reservation. Confirmed against measured SDK and modelled intermediary headers, with the size-class table: [ADR 0023](../decisions/0023-header-size-measurement-and-size-classes.md) |

Query strings and absolute-form targets are rejected (`400`); the path is fixed. The only caller-controlled metadata that intentionally reaches the provider are the credential, the optional organization/project ids, and the request body.

## Outbound request headers (`wire_headers`, `Upstream::outbound`)

Exactly these, and no others (`WIRE_HEADER_NAMES`; `Host` is added by the HTTP client from the destination URL):

`Authorization: Bearer <token>` (normalized scheme, marked sensitive), `Content-Type: application/json`, `Content-Length: <sanitized byte length>`, `Accept`, `Accept-Encoding: identity`, `User-Agent`, and the optional `OpenAI-Organization` / `OpenAI-Project`.

There is no `Transfer-Encoding`, `Connection`, `Expect`, `Upgrade`, `TE`, `Trailer`, `Proxy-*`, `Forwarded`, `X-Forwarded-*`, or cookie. The body is one sealed buffer; the length is `body.len()` of the `SanitizedRequest`, so neither the original body length nor a caller-declared length can influence it. The builder takes only `VettedHeaders` and a `SanitizedRequest`; no original-body reference exists on this path, so none can be kept for retries or fallback (there are none).

## Provider credential

- Type `ProviderCredential` (`transport::credential`): no `Clone`, `Default`, `Display`, `Serialize`, or equality; `Debug` prints a fixed `ProviderCredential(<redacted>)`; the header value is flagged sensitive for the HTTP library. `VettedHeaders` (credential plus organization/project) has a redacted `Debug` and is consumed by `Upstream::outbound`, so a request holds exactly one copy and nothing is reusable for replay.
- It is never in `ValidatedRequest`, `SanitizedRequest`, or any inspected text. It is in no shared state: the shared client carries no default headers (tested, #23), `Upstream` holds no credential, and concurrent requests with different keys are tested not to cross.
- It is forwarded only to the destination chosen by `RouteId` from the reviewed table. Content-profile changes cannot alter header policy, `Host`, TLS, or origin (tested).
- Secure erasure is **not** promised (SECURITY.md, ADR 0007): copies may remain in allocator, HTTP, and TLS buffers.

## Local gateway authority (frozen design, planned: #63)

A caller token proving the caller may use the gateway is a separate concept and is specified in [local-caller-auth](local-caller-auth.md) ([ADR 0030](../decisions/0030-local-caller-auth-listener-health.md)). The header is `X-Gateway-Local-Token` inside the reserved `X-Gateway-Local-*` namespace (`LOCAL_AUTHORITY_PREFIX`); it has its own type and module, is never accepted as the provider credential, is consumed locally, and is never forwarded. It does not use `Authorization`, which stays the provider credential. **Today nothing enforces it:** every `X-Gateway-Local-*` header, including `X-Gateway-Local-Token`, is ignored and cannot reach the wire (the outbound set is an allowlist). When #63 lands, a missing or invalid token is `401 local_auth_required` / `local_auth_invalid` before any body read, and `vet_inbound` runs only for authenticated requests.

## Framing and malformed-header outcomes

Observed behavior of the served stack (tests in `tests/header_credentials.rs`, `tests/chat_admission.rs`, and the adversarial suite in `src/transport/tests/attack_tests.rs`; the HTTP layer is hyper behind the connection guard of ADR 0019):

| Input | Outcome |
| --- | --- |
| Conflicting `Content-Length` values, `Content-Length: a, b`, signed or non-numeric length | `400` (bare, from the HTTP layer, or `malformed_input`) |
| Identical duplicate `Content-Length` | The HTTP layer collapses to one value (RFC 9112 section 6.3); admitted as an ordinary single length. Reviewed: safe, because the single length is still enforced against the body |
| `Transfer-Encoding` other than exactly `chunked` (for example `gzip, chunked`) | `415 unsupported_input` |
| Obsolete line folding, invalid header name, header without colon, control byte in a value | `400` (bare, from the HTTP layer) |
| Header names plus values over 16,384 bytes, or one value over 8,192 bytes | `431 limit_exceeded` from the route (#41, ADR 0023) |
| More than 100 header fields | Fixed `431 limit_exceeded` written by the head guard, connection closed (#41; before, the HTTP server's own bare `431`) |
| `Content-Length` together with `Transfer-Encoding` (any value, either order, even an invalid length) | **Refused before the HTTP parser sees it with a local `400 malformed_input`, then the connection is closed** (#25, #43, [ADR 0019](../decisions/0019-request-head-guard-and-one-request-per-connection.md), [ADR 0021](../decisions/0021-framing-ambiguity-parser-level-investigation.md)). A byte-stream guard scans the first request head's field names; the parser alone would have discarded the length and framed the body as chunked, invisibly to the handler. The guard writes the fixed contract body itself (`Connection: close`, no request-derived byte). No handler runs and no body is read |
| Head not finished within `body_deadline_ms` | Connection closed, no response, nothing reserved (#25). The deadline is absolute from accept, so trickling bytes cannot extend it |
| Head (request line through the blank line) longer than 64 KiB, whether or not it ends | The same fixed `431 limit_exceeded` written by the guard, then the connection is closed (#43, #41); the outcome does not depend on how the bytes were split across reads; nothing reserved |
| Bytes after the first request (pipelined or smuggled second message) | Never parsed as a request: every response carries `Connection: close`, so a connection serves one request (#25, ADR 0019) |

**Residual (framing).** The guard checks two framing fields on the first head; it is not a proof against all request smuggling. The provider connection is unaffected either way (outbound framing is regenerated from the sealed bytes). The guard's `400` is best effort on delivery: it is written and the write side is shut down, but if the peer is still sending, the operating system may reset the connection and the peer may see a reset instead of the response; the request is refused either way. The guard is a second, structural reading of two field names, because the pinned HTTP stack offers no hook that can expose the ambiguity (ADR 0021). Any intermediary placed in front of the gateway must itself reject ambiguous or obfuscated framing (for example both `Content-Length` and `Transfer-Encoding`, obsolete line folding, bare line feeds, invalid header names) and must not reuse client connections for several users; the gateway's loopback scope assumes there is none. Tested HTTP stack: see the [control map](../qualification/alpha1-threat-control-map.md).

## Safe codes added

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| No usable `Authorization` (absent, other credential header only, or nominated by `Connection`); `WWW-Authenticate: Bearer` | 401 | `missing_credential` | no |
| Duplicate/malformed `Authorization`, malformed or repeated organization/project, malformed or ambiguous `Connection` | 400 | `malformed_input` | no |
| `Expect` other than `100-continue` | 417 | `unsupported_input` | no |
| Request headers over the byte limits | 431 | `limit_exceeded` | no |

`401 missing_credential` refers to the **provider** credential. It is deliberately not a local-authentication outcome.

## Provider response headers (`relay_response_headers`)

Applied when relaying ordinary JSON responses (#20) and to the headers of a streamed response (#21), where it is applied once, when the provider's headers arrive. Relayed: `Content-Type`, `Cache-Control`, `Retry-After`, `X-Request-Id`, `OpenAI-Processing-Ms`, `OpenAI-Version`, and `X-RateLimit-*`. Everything else is dropped, including `Set-Cookie`, hop-by-hop headers and anything the response's `Connection` nominates, `Content-Length` and `Transfer-Encoding` (the gateway's server regenerates framing), `Location` (redirects are not followed), `WWW-Authenticate`, `Server`, `Alt-Svc`, CORS and security headers, and `OpenAI-Organization`. A value over 1 KiB is dropped. A `Content-Encoding` other than `identity` makes the response non-relayable (`502 upstream_invalid_response`): the gateway does not decode, and relaying the bytes without the header would corrupt them. Response header bytes are bounded by `max_response_header_bytes` and the parser's 64-field cap (`502 upstream_response_too_large`) (#20).

## Verification

Unit tests (`src/transport/headers.rs`, `credential.rs`): allowlists, every credential and metadata form, `Connection` nomination, outbound set equality, redacted `Debug`, response filter. Fake-upstream tests (`src/transport/tests/wire_tests.rs`): credential and metadata reach only the fake with regenerated framing and exact length; caller `Host`/`Content-Length`/proxy headers cannot control the wire; 24 concurrent requests with different keys never cross; no marker in `Debug`, errors, or built request objects; profile changes do not alter headers. Served-stack tests (`tests/header_credentials.rs`): every outcome above, and no credential in any gateway response including health.

Gateway logging: the gateway currently emits no request logs; credential absence from `Debug`, errors, responses, and health output is what is tested.
