# Contract: safe errors and telemetry

Status: SDK retry behavior and stream truncation behavior verified with the pinned Node.js and Python SDKs in #22 ([ADR 0020](../decisions/0020-sdk-qualification-test-build.md); section "SDK retry guidance" below). Approved categories; code spellings fixed in #4; HTTP status mappings for the Chat Completions admission path fixed in #18 (below); header and credential outcomes added in #24 ([contract](headers-and-credentials.md)). Mappings for transport failures and relayed ordinary JSON responses fixed in #20 ([ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md); below); the SSE stream termination contract and stream timings fixed in #21 ([ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md); below).

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
| `not_implemented` | The request cannot be served by this build or deployment: no `deployment.upstream` is configured (#20). Never forwarded. |

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

SDK retry column: **verified in #22** with the pinned SDKs (npm `openai` 7.27.0, PyPI `openai` 3.24.0; evidence table in "SDK retry guidance" below). Both retry `408`, `409`, `429`, every `5xx`, and connection errors, twice by default (three attempts), and do not retry other `4xx`; the decision is by status code, because the gateway does not relay the provider's `x-should-retry` and `retry-after-ms` hints. This is SDK behavior, not gateway behavior: the gateway performs no retries. A retried request that the gateway rejected locally is safe because nothing from it reached the provider. The guidance table lists the situations exercised directly through the SDKs; the other rows follow from their status code. Retries of the `501` below are wasted work, not a hazard.

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| Method other than `POST` on the route (`Allow: POST`) | 405 | `unsupported_input` | no |
| Query string, absolute-form target, or `Upgrade` | 400 | `unsupported_input` | no |
| `Content-Type` not exactly `application/json` | 415 | `unsupported_input` | no |
| `Content-Encoding` present or a transfer coding other than `chunked` | 415 | `unsupported_input` | no |
| Malformed or conflicting `Content-Length` / empty body | 400 | `malformed_input` | no |
| `Content-Length` together with `Transfer-Encoding` in the first request head (written by the connection-level head guard before the HTTP layer, #43; same body and `Connection: close`) | 400 | `malformed_input` | no |
| Request head over 64 KiB without ending (written by the head guard, #43) | 431 | `limit_exceeded` | no |
| Invalid JSON, invalid UTF-8, duplicate key, aborted body | 400 | `malformed_input` | no |
| Well-formed but outside the supported subset (unknown or unsupported field, wrong type, out-of-range value) | 422 | `unsupported_input` | no |
| Declared or actual body over the limit, or a depth/node/string/count budget exceeded | 413 | `limit_exceeded` | no |
| Body not received within `body_deadline_ms` | 408 | `limit_exceeded` | yes |
| Receipt or memory capacity unavailable within `admission_wait_ms`, or wait queue full (`Retry-After: 1`) | 503 | `overload` | yes, after `Retry-After` |
| No usable provider `Authorization` (`WWW-Authenticate: Bearer`) (#24) | 401 | `missing_credential` | no |
| Duplicate or malformed `Authorization`, malformed or repeated organization/project, malformed or ambiguous `Connection` (#24) | 400 | `malformed_input` | no |
| `Expect` other than `100-continue` (#24) | 417 | `unsupported_input` | no |
| Request headers over the byte limits (#24) | 431 | `limit_exceeded` | no |
| No upstream configured (`stream: true` or not) | 501 | `not_implemented` | yes (5xx; wasted work) |
| `stream: true` from an HTTP/1.0 caller (a cut stream could not be told from a finished one) | 422 | `unsupported_input` | no |

After admission, inspection and approval (#19) add these rejections, all local, with no upstream byte and the same fixed body:

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| `Block` finding; `Warn` finding while `content.on_warn` is `reject` (default); a finding in `model` | 422 | `unsupported_input` | no |
| Request-wide text-byte or finding limit exceeded; transformed output over its bound | 413 | `limit_exceeded` | no |
| Detector, policy, or placeholder failure; discarded or panicked inspection job | 500 | `incomplete_inspection` | yes (5xx) |
| No inspection permit or queue slot (`Retry-After: 1`) | 503 | `overload` | yes, after `Retry-After` |

Rationale for the mappings: a caller cannot fix `5xx`/`408` by changing the request, so those are the retryable ones (transient capacity, slow client); everything the caller must change is a non-retried `4xx`. `413` is used for every limit rather than `400` so a caller can tell "too big" from "wrong". `422` separates a request that parses but is not in the supported subset from one that does not parse at all.

The two head-guard rows are written by `src/head_guard.rs` as fixed bytes (the same body and headers as above, `Content-Length` set, no request-derived byte) and are best effort on delivery: a peer that is still sending when the connection closes may see a reset instead. A head not finished within the head deadline is closed without a response. Rejections made by the HTTP layer before the handler runs (for example a `Content-Length` that is not a valid integer) are bare `400` responses without this body; they are still local and still send nothing upstream.

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

SDK retry implications (verified in #22, see "SDK retry guidance"). The OpenAI Python and Node SDKs retry `5xx` and `408`/`409`/`429` by default, and a failed connection. The Gateway itself never retries and never replays a payload after any send attempt began. A retry by the SDK is a **new, independent request**, and where the first attempt reached the provider (a timeout, an invalid or oversized response, a disconnect) the provider may already have run, and may bill for, the first one: with the SDK defaults, an oversize, truncated, or timed-out provider answer reaches the provider three times. Only the rows marked "no" in the "Request bytes sent?" column are known to have transmitted nothing. A caller that cannot tolerate duplicate provider-side work must disable SDK retries (`maxRetries: 0` / `max_retries=0`, as the shipped examples do). No exactly-once or at-most-once delivery is claimed.

Content coding: the Gateway requests `Accept-Encoding: identity` and does not decode. A provider response with any other `Content-Encoding` cannot be relayed faithfully (the coding header is not relayed) and is `upstream_invalid_response`.

Cancellation and shutdown. When the caller disconnects, the request future is dropped: any wait, inspection await, or upstream exchange is cancelled, the connection to the provider is closed, and the memory reservation and permits are released. Bytes already written to the provider cannot be retracted, so a cancelled request may still have been received (and acted on) by the provider. At shutdown the Gateway stops accepting, reports not ready, and drains for at most `shutdown_drain_ms`; remaining in-flight requests are then cancelled the same way (answering `503 not_ready` if the caller is still connected) and `serve` returns without waiting further than a one-second grace.

## SSE streams (#21; [ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md))

`stream: true` is admitted, parsed, inspected, and sealed exactly like any request (every rejection above applies and sends zero upstream bytes), then forwarded once. A `2xx` answer with `Content-Type: text/event-stream` is relayed incrementally and unredacted; any other answer (a provider `4xx`/`5xx`, a JSON answer) is the ordinary buffered relay above. Provider events are relayed byte for byte: the Gateway never parses, merges, splits, reorders, or adds events, and never adds `data: [DONE]` or any other completion or error event.

**Before the response headers are committed** every failure is a normal Gateway error with a real status, from the table above plus: stream capacity unavailable (`503 overload`, `Retry-After: 1`, nothing sent upstream), and a provider event stream with no explicit end of message, that is close-delimited (`502 upstream_invalid_response`; a cut could not be told from a finish).

**After the response headers are committed** no status can change. The termination contract: every failure ends the stream abruptly. The server closes the connection without the terminating chunk, so the caller sees a truncated, errored stream and never a normal end. Nothing is written into the stream. The causes (closed counters, never visible to the caller beyond the truncation):

| Cause | Trigger | Provider connection |
| --- | --- | --- |
| Provider error | Connection error, malformed or truncated chunked framing, or a provider disconnect before the final chunk (recorded as `upstream_invalid_response`) | closed |
| Idle | No provider chunk within `stream_idle_ms` (recorded as `upstream_timeout`) | closed |
| Lifetime | Stream older than `stream_lifetime_ms` (recorded as `upstream_timeout`) | closed |
| Buffer | One provider chunk over `stream_buffer_bytes` (recorded as `upstream_response_too_large`) | closed |
| Shutdown | Drain deadline passed (recorded as `not_ready`) | closed |
| Abandoned | Caller disconnected, or no write progress for `stream_write_stall_ms` (the connection is closed) | closed |

Only a clean provider end (the provider's own final chunk) produces a normal end. Bytes already transmitted to the caller cannot be retracted.

SDK behavior (**observed in #22**; the earlier unverified text here claimed both SDKs raise and may retry, which was wrong in two respects). On the wire a broken stream ends without its terminating chunk (no `0` chunk), and nothing is ever added to it. The **Python** SDK (httpx) raises `APIConnectionError` after delivering only the events the provider really sent. The **Node.js** SDK depends on Node's built-in fetch (undici) and so on the Node.js version, because every gateway response carries `Connection: close` ([ADR 0019](../decisions/0019-request-head-guard-and-one-request-per-connection.md)): on **Node.js 24** (the version CI uses and the examples require) it raises, and on **Node.js 22.16.0** (undici 6.21.2) it did **not**: the SDK yielded a shorter stream and ended normally. A control in the suite shows the same Node 22 fetch raises for the same truncation on a keep-alive response. Neither SDK retried a stream cut after the headers (one request each). **The version-independent check is the provider's own completion indicator: require `finish_reason` on the last chunk and treat a stream without it as truncated**; the shipped Node example does. A caller that cannot tolerate duplicate provider-side work must disable SDK retries; a provider `429`/`5xx` before the stream starts is retried like any other. An HTTP/1.0 caller would not see truncation at all, so `stream: true` is refused for HTTP/1.0. Whether the gateway should signal truncation more forcefully for older undici versions (for example with an abortive close) is a follow-up decision, not made here.

Cancellation and shutdown. A caller that disconnects makes the server drop the response body, which closes the provider connection and returns the upstream and stream permits; the provider may stop shortly after (it sees the close). On shutdown the drain deadline applies as above; open streams are then cancelled and end abruptly. Nothing is replayed.

Telemetry for streams: stage timings for first byte, total, upstream wait (time spent waiting on the provider), and downstream wait (time between handing a chunk to the server and the server asking for the next, that is consumer and socket backpressure; relay overhead is total minus both), counters for streams started and ended by cause, provider bytes relayed, and the bytes the relay holds now and at peak. No stream content, key, header, or route is ever a label or a field.

## SDK retry guidance (observed in #22)

Method: the pinned SDKs at their default retry setting (`maxRetries` / `max_retries` = 2) call the NON-RELEASE qualification build ([ADR 0020](../decisions/0020-sdk-qualification-test-build.md)) wired to a scripted fake provider. Each row counts the HTTP attempts the SDK made to the gateway (an SDK fetch hook, an httpx request hook) and the requests that actually reached the fake provider. Both SDKs gave identical counts on every row. Evidence files (`retry-observations-node.json`, `retry-observations-python.json`, `stream-truncation-observations-*.json`) are uploaded by the qualification workflow; the assertions in `qualification/sdk/*/` fail if the SDK policy ever changes, and this table must then be reconciled.

| Situation | SDK attempts | Requests that reached the provider |
| --- | --- | --- |
| Provider `400`, `401`, `403`, `404`, `422` (relayed unchanged) | 1 (not retried) | 1 |
| Provider `408`, `409`, `429`, `500`, `502`, `503`, `504` (relayed unchanged) | 3 (retried twice) | 3 |
| Provider `429` with `Retry-After: 0` (the header is relayed) | 3 | 3 |
| Provider `429`, `429`, then `200` | 3, and the call succeeds | 3 |
| Provider `500` with `x-should-retry: false` and `retry-after-ms: 10`, through the gateway | 3 (the hints are not relayed, so the SDK uses its status policy); directly to the provider the SDK makes 1 | 3 |
| Gateway `422 unsupported_input`, `413 limit_exceeded` | 1 | 0 |
| Gateway `501 not_implemented` (no upstream configured) | 3 (wasted work) | 0 |
| Gateway `503 overload` (`Retry-After: 1` is relayed) | 3, and the SDK waited the relayed second between attempts | 0 (nothing was sent for the refused attempts) |
| Gateway `502 upstream_unavailable` (provider unreachable) | 3 | 0 |
| Gateway `502 upstream_response_too_large`, `502 upstream_invalid_response` | 3 | 3 (each attempt reached the provider) |
| Gateway `504 upstream_timeout` (provider never answered) | 3 | 3 |
| Connection refused to the gateway | 3 (`APIConnectionError`) | 0 |
| Stream cut after the response headers (provider cut, or gateway idle cut) | 1 (not retried) | 1 |
| Provider `429` before a stream starts | 3 | 3 |

What to tell integrators:

1. The default retry policy is status-based and the gateway does not change it, except that the gateway drops the provider's `x-should-retry` and `retry-after-ms` hints (its response-header allowlist relays only `content-type`, `cache-control`, `retry-after`, `x-request-id`, `openai-processing-ms`, `openai-version`, and `x-ratelimit-*`). A provider that says "do not retry" is therefore retried through the gateway. This was observed with the fake provider; the headers a real provider sends were not observed.
2. With the defaults, a failure after the request reached the provider is sent to the provider up to three times (duplicate and possibly billed work). Set `maxRetries: 0` (Node) or `max_retries=0` (Python) unless duplicates are acceptable; the shipped examples do.
3. A retry of a request the gateway rejected locally (`4xx`, `501`, `503 overload`, `502 upstream_unavailable`) never reaches the provider, so it is safe; retries of `501` are wasted work.
4. Stream truncation: Python raises `APIConnectionError`; Node.js raises on Node 24 but ended normally on Node 22.16.0 (see the SSE section). Require `finish_reason` on Node.js.
5. This is not a statement about other SDK versions or other HTTP clients, and not a statement about the real provider.

## Stage timings (ADR 0008)

`telemetry::Metrics` records, as count, total, and maximum microseconds, only these stages: admission wait, parse, inspection (including worker queueing), serialization (inside inspection, measured on the worker), upstream first response (send to response headers), and upstream total (send to last buffered byte or failure), plus a counter of upstream send attempts (and, for streams, the stages and counters above). The vocabulary is a closed enum: no payload, route, credential, URL, or caller-supplied value can become a label. There is no exporter yet; the counters are in-process (#20 adds the minimum, not a metrics platform).

## Status

Implemented: health, local rejection, config diagnostics, the Chat Completions admission/parse/limit mappings, inspection rejections (#19), and ordinary JSON forwarding, relay, transport-error mappings, and stage timings (#20). SSE relay, the stream termination contract, and stream timings and counters (#21). SDK qualification of both with the pinned Node.js and Python SDKs, and the observed retry and truncation behavior below (#22).
