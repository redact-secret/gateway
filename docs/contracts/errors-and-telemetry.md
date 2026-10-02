# Contract: safe errors and telemetry

Status: approved categories; code spellings fixed in #4; HTTP status mappings for the Chat Completions admission path fixed in #18 (below); header and credential outcomes added in #24 ([contract](headers-and-credentials.md)). Mappings for transport failures and relayed ordinary JSON responses fixed in #20 ([ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md); below); SSE stream errors land with #21.

## Gateway-owned error categories

| Category | When |
| --- | --- |
| `malformed_input` | Bad JSON, duplicate keys, invalid UTF-8, bad framing |
| `unsupported_input` | Unknown route/method/field/content form, compression, unsupported content |
| `limit_exceeded` | Any resource-limit category |
| `incomplete_inspection` | Core did not report complete inspection |
| `overload` | Admission refused or timed out |
| `transport_failure` | Generic transport failure; also listener bind/serve failure at startup. Forwarding failures use the specific `upstream_*` codes below (#20). |
| `upstream_timeout` | A connect, response-header, or total upstream deadline elapsed (#20) |
| `upstream_unavailable` | The provider could not be reached: resolution, address policy, or refused connection (#20) |
| `upstream_tls_failure` | TLS to the provider failed: certificate, hostname, or handshake (#20) |
| `upstream_invalid_response` | Malformed or truncated provider HTTP, disconnect after the request was sent, or a content coding the Gateway cannot relay (#20) |
| `upstream_response_too_large` | Provider response headers or body over the configured bounds (#20) |
| `invalid_config` | Startup only: static configuration failed validation. Never a request outcome. |
| `not_ready` | Readiness is false: validated plan or required initialization is missing. |
| `missing_credential` | No usable provider `Authorization` on a request (#24). The provider credential, not a local-auth result. |
| `not_implemented` | The request cannot be served by this build or deployment: `stream: true` until SSE relay lands (#21), or no `deployment.upstream` is configured (#20). Never forwarded. |

Errors never echo payload fragments, credentials, or offending text. Provider response and error bodies are relayed under the response contract and may contain sensitive data. Document status mappings and SDK-retry implications. After response bytes start, errors cannot change the HTTP status; terminate per the stream error contract without fabricating completion events.

## Telemetry

Allowed: bounded route IDs, version identifiers, coarse outcomes, timing, aggregate counters.

Excluded: bodies, raw URLs and query values, keys and credentials, raw findings, matched snippets, unbounded labels, detector scoring internals.

Health reports liveness. Readiness reports valid plan, initialized core, and ability to accept work. Neither performs a credentialed upstream probe by default.

## Implemented in the skeleton (#4)

Spellings are the `as_str()` values of `telemetry::SafeCode` (`malformed_input`, `unsupported_input`, `limit_exceeded`, `incomplete_inspection`, `overload`, `transport_failure`, `invalid_config`, `not_ready`, `not_implemented`, `missing_credential`). Gateway-generated bodies are `{"error":{"code":"<code>"}}`.

| Situation | Status | Code |
| --- | --- | --- |
| Unknown route, or any method on a path that is not a served route | 404 | `unsupported_input` |
| `GET /readyz` while not ready | 503 | `not_ready` |

Configuration diagnostics (`invalid_config: <kind> at <schema location>`) are described in [configuration](../configuration.md). Health endpoints never call an upstream.

## `POST /v1/chat/completions` admission (#18)

Every rejection below is generated locally before anything is sent upstream, has a fixed body `{"error":{"code":"<code>"}}` with `Content-Type: application/json`, `Cache-Control: no-store`, and `Connection: close` (a body that was not read must not be parsed as the next request). No response contains request text, header values, field names, or parser messages. Field-level detail is deliberately not reported: names and values are caller payload.

SDK retry column: the OpenAI Python and Node SDKs retry `408`, `409`, `429`, and every `5xx` by default (twice, with backoff, honoring `Retry-After`); they do not retry other `4xx`. (Documented SDK defaults, not yet verified against pinned SDK versions; that qualification is #22.) This is SDK behavior, not gateway behavior: the gateway performs no retries. A retried request is safe here because nothing from a rejected request reached the provider. Retries of the `501` below are wasted work, not a hazard.

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| Method other than `POST` on the route (`Allow: POST`) | 405 | `unsupported_input` | no |
| Query string, absolute-form target, or `Upgrade` | 400 | `unsupported_input` | no |
| `Content-Type` not exactly `application/json` | 415 | `unsupported_input` | no |
| `Content-Encoding` present or a transfer coding other than `chunked` | 415 | `unsupported_input` | no |
| Malformed or conflicting `Content-Length` / empty body | 400 | `malformed_input` | no |
| Invalid JSON, invalid UTF-8, duplicate key, aborted body | 400 | `malformed_input` | no |
| Well-formed but outside the supported subset (unknown or unsupported field, wrong type, out-of-range value) | 422 | `unsupported_input` | no |
| Declared or actual body over the limit, or a depth/node/string/count budget exceeded | 413 | `limit_exceeded` | no |
| Body not received within `body_deadline_ms` | 408 | `limit_exceeded` | yes |
| Receipt or memory capacity unavailable within `admission_wait_ms`, or wait queue full (`Retry-After: 1`) | 503 | `overload` | yes, after `Retry-After` |
| No usable provider `Authorization` (`WWW-Authenticate: Bearer`) (#24) | 401 | `missing_credential` | no |
| Duplicate or malformed `Authorization`, malformed or repeated organization/project, malformed or ambiguous `Connection` (#24) | 400 | `malformed_input` | no |
| `Expect` other than `100-continue` (#24) | 417 | `unsupported_input` | no |
| Request headers over the byte limits (#24) | 431 | `limit_exceeded` | no |
| `stream: true` (admitted and validated; SSE relay is #21), or no upstream configured | 501 | `not_implemented` | yes (5xx; wasted work) |

After admission, inspection and approval (#19) add these rejections, all local, with no upstream byte and the same fixed body:

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| `Block` finding; `Warn` finding while `content.on_warn` is `reject` (default); a finding in `model` | 422 | `unsupported_input` | no |
| Request-wide text-byte or finding limit exceeded; transformed output over its bound | 413 | `limit_exceeded` | no |
| Detector, policy, or placeholder failure; discarded or panicked inspection job | 500 | `incomplete_inspection` | yes (5xx) |
| No inspection permit or queue slot (`Retry-After: 1`) | 503 | `overload` | yes, after `Retry-After` |

Rationale for the mappings: a caller cannot fix `5xx`/`408` by changing the request, so those are the retryable ones (transient capacity, slow client); everything the caller must change is a non-retried `4xx`. `413` is used for every limit rather than `400` so a caller can tell "too big" from "wrong". `422` separates a request that parses but is not in the supported subset from one that does not parse at all.

Rejections made by the HTTP layer before the handler runs (for example a `Content-Length` that is not a valid integer) are bare `400` responses without this body; they are still local and still send nothing upstream.

## Forwarding and relayed responses (#20)

An approved request is sent once. Two kinds of answer must never be confused by a caller or an operator:

1. **A provider response**, relayed as received: the provider's status (`2xx`, and also `4xx`/`5xx` such as `401`, `429`, `500`), the allowlisted provider headers ([headers contract](headers-and-credentials.md)), and the provider's body, **unredacted**. A provider `401` or `429` is a provider answer, not a Gateway error, and has the provider's JSON error shape, not `{"error":{"code":...}}`.
2. **A Gateway-generated error**, below, always the fixed body `{"error":{"code":"<code>"}}` with `Content-Type: application/json` and `Cache-Control: no-store`.

Provider response and error bodies can contain sensitive data. They are never logged, never placed in telemetry, never echoed into a Gateway error, and have no `Debug` rendering beyond a status and a length. The Gateway does not redact them in this release (non-goal for Alpha 1).

The response is buffered under hard bounds and relayed only when complete, so every failure below is reported with a real status code before any response byte is committed to the caller.

| Situation | Status | Code | Request bytes sent? | SDK retries |
| --- | --- | --- | --- | --- |
| No upstream permit free (`Retry-After: 1`) | 503 | `overload` | no | yes, after `Retry-After` |
| Connect refused, name not allowed or not resolvable, address policy refusal | 502 | `upstream_unavailable` | no | yes |
| TLS failure (certificate, hostname, handshake) | 502 | `upstream_tls_failure` | no | yes |
| Connect, response-header, or total deadline elapsed | 504 | `upstream_timeout` | maybe (see below) | yes |
| Malformed or truncated response; provider closed the connection after the request was sent; response `Content-Encoding` other than `identity` | 502 | `upstream_invalid_response` | yes | yes |
| Response header block over `max_response_header_bytes`, or body over `max_response_body_bytes` (declared or counted) | 502 | `upstream_response_too_large` | yes | yes |
| Caller disconnected, or shutdown cancelled the request | none deliverable / 503 `not_ready` | n/a | maybe | n/a |

SDK retry implications. The OpenAI Python and Node SDKs retry `5xx` and `408`/`409`/`429` by default (documented SDK behavior; qualification is #22). The Gateway itself never retries and never replays a payload after any send attempt began. A retry by the SDK is a **new, independent request**, and where the first attempt reached the provider (a timeout, an invalid or oversized response, a disconnect) the provider may already have run, and may bill for, the first one. Only the rows marked "no" are known to have transmitted nothing. A caller that cannot tolerate duplicate provider-side work must disable SDK retries. No exactly-once or at-most-once delivery is claimed.

Content coding: the Gateway requests `Accept-Encoding: identity` and does not decode. A provider response with any other `Content-Encoding` cannot be relayed faithfully (the coding header is not relayed) and is `upstream_invalid_response`.

Cancellation and shutdown. When the caller disconnects, the request future is dropped: any wait, inspection await, or upstream exchange is cancelled, the connection to the provider is closed, and the memory reservation and permits are released. Bytes already written to the provider cannot be retracted, so a cancelled request may still have been received (and acted on) by the provider. At shutdown the Gateway stops accepting, reports not ready, and drains for at most `shutdown_drain_ms`; remaining in-flight requests are then cancelled the same way (answering `503 not_ready` if the caller is still connected) and `serve` returns without waiting further than a one-second grace.

## Stage timings (ADR 0008)

`telemetry::Metrics` records, as count, total, and maximum microseconds, only these stages: admission wait, parse, inspection (including worker queueing), serialization (inside inspection, measured on the worker), upstream first response (send to response headers), and upstream total (send to last buffered byte or failure), plus a counter of upstream send attempts. The vocabulary is a closed enum: no payload, route, credential, URL, or caller-supplied value can become a label. There is no exporter yet; the counters are in-process (#20 adds the minimum, not a metrics platform).

## Status

Implemented: health, local rejection, config diagnostics, the Chat Completions admission/parse/limit mappings, inspection rejections (#19), and ordinary JSON forwarding, relay, transport-error mappings, and stage timings (#20). Planned: SSE stream errors and stream timings (#21).
