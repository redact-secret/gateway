# Contract: safe errors and telemetry

Status: transport framing, the frozen status mapping, and the SDK retry, duplicate-work and truncation matrix qualified in #60 (sections "Frozen status mapping" and "Alpha 2 transport qualification"). SDK retry behavior and stream truncation behavior verified with the pinned Node.js and Python SDKs in #22 ([ADR 0020](../decisions/0020-sdk-qualification-test-build.md); section "SDK retry guidance" below). Approved categories; code spellings fixed in #4; HTTP status mappings for the Chat Completions admission path fixed in #18 (below); header and credential outcomes added in #24 ([contract](headers-and-credentials.md)). Mappings for transport failures and relayed ordinary JSON responses fixed in #20 ([ADR 0017](../decisions/0017-json-forwarding-deadlines-and-cancellation.md); below); the SSE stream termination contract and stream timings fixed in #21 ([ADR 0018](../decisions/0018-sse-relay-termination-and-stream-bounds.md); below).

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

**Planned codes (not yet in `SafeCode`, so not in the table above).** `local_auth_required` and `local_auth_invalid` (#61, enforced by #63): the local caller token is absent, or present but duplicate, malformed, out of bounds, or wrong. `401`, before any body read, distinct from the provider-credential `missing_credential`. Spec: [local-caller-auth](local-caller-auth.md). #63 moves them into the table when it adds the `SafeCode` variants.

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
| Request head over 64 KiB, ended or not, or more than 100 header fields (written by the head guard, #43, #41) | 431 | `limit_exceeded` | no |
| Invalid JSON, invalid UTF-8, duplicate key, aborted body | 400 | `malformed_input` | no |
| Well-formed but outside the supported subset (unknown or unsupported field, wrong type, out-of-range value) | 422 | `unsupported_input` | no |
| Declared or actual body over the limit, or a depth/node/string/count budget exceeded | 413 | `limit_exceeded` | no |
| Body not received within `body_deadline_ms` | 408 | `limit_exceeded` | yes |
| Receipt or memory capacity unavailable within `admission_wait_ms`, or wait queue full (`Retry-After: 1`) | 503 | `overload` | yes, after `Retry-After` |
| No usable provider `Authorization` (`WWW-Authenticate: Bearer`) (#24) | 401 | `missing_credential` | no |
| Duplicate or malformed `Authorization`, malformed or repeated organization/project, malformed or ambiguous `Connection` (#24) | 400 | `malformed_input` | no |
| `Expect` other than `100-continue` (#24) | 417 | `unsupported_input` | no |
| Request header names plus values over 16,384 bytes or a value over 8,192 bytes (route, #24, #41) | 431 | `limit_exceeded` | no |
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
| Malformed or truncated response; provider closed the connection after the request was sent; response `Content-Encoding` other than `identity`; a final `1xx` status such as `101` (#60) | 502 | `upstream_invalid_response` | yes | yes |
| Response header block over `max_response_header_bytes`, or body over `max_response_body_bytes` (declared or counted) | 502 | `upstream_response_too_large` | yes | yes |
| Caller disconnected, or shutdown cancelled the request | none deliverable / 503 `not_ready` | n/a | maybe | n/a |

SDK retry implications (verified in #22, see "SDK retry guidance"). The OpenAI Python and Node SDKs retry `5xx` and `408`/`409`/`429` by default, and a failed connection. The Gateway itself never retries and never replays a payload after any send attempt began. A retry by the SDK is a **new, independent request**, and where the first attempt reached the provider (a timeout, an invalid or oversized response, a disconnect) the provider may already have run, and may bill for, the first one: with the SDK defaults, an oversize, truncated, or timed-out provider answer reaches the provider three times. Only the rows marked "no" in the "Request bytes sent?" column are known to have transmitted nothing. A caller that cannot tolerate duplicate provider-side work must disable SDK retries (`maxRetries: 0` / `max_retries=0`, as the shipped examples do). No exactly-once or at-most-once delivery is claimed.

Content coding: the Gateway requests `Accept-Encoding: identity` and does not decode. A provider response with any other `Content-Encoding` cannot be relayed faithfully (the coding header is not relayed) and is `upstream_invalid_response`.

Request-head size classes (#41, [ADR 0023](../decisions/0023-header-size-measurement-and-size-classes.md)). One outcome per class, tested on both sides of every limit in `src/transport/tests/header_cap_tests.rs`:

| Class | Condition | Answered by | Outcome |
| --- | --- | --- | --- |
| Within all limits | head at most 65,536 bytes, at most 100 fields, names plus values at most 16,384, each value at most 8,192 | route | admitted |
| Route byte caps | over 16,384 of names plus values, or a value over 8,192 (head and field count within their limits) | route | `431 limit_exceeded`, gateway response |
| Field count | more than 100 fields | head guard | fixed `431 limit_exceeded` constant, connection closed |
| Head bound | head over 65,536 bytes, ended or not | head guard | the same fixed constant, connection closed |
| Late head | not finished by `body_deadline_ms` | head guard | connection closed, no response |
| Ambiguous framing | `Content-Length` with `Transfer-Encoding` | head guard | fixed `400 malformed_input` |

In every refusal row nothing is reserved, nothing reaches the provider, and no request-derived byte or credential is in the response. Delivery of a guard answer is best effort (the peer may see a reset if it is still sending).

Connection bound (#40, [ADR 0022](../decisions/0022-connection-bound-at-accept.md)). A connection that arrives while `resources.limits.max_connections` connections are open is closed at accept: no status, no body, no error code, because nothing has been read and nothing is written. The caller sees a closed or reset connection (an SDK `APIConnectionError`, retried by default like any connection error). This is not the `overload` of request admission, which is a `503` after a complete request head; the two bounds are independent. Health probes at the bound are refused the same way. There is no refusal counter or log yet.

Cancellation and shutdown. When the caller disconnects, the request future is dropped: any wait, inspection await, or upstream exchange is cancelled, the connection to the provider is closed, and the memory reservation and permits are released. Bytes already written to the provider cannot be retracted, so a cancelled request may still have been received (and acted on) by the provider. At shutdown the Gateway stops accepting, reports not ready, and drains for at most `shutdown_drain_ms`; remaining in-flight requests are then cancelled the same way (answering `503 not_ready` if the caller is still connected) and `serve` returns without waiting further than a one-second grace.

## Frozen status mapping (#60)

Status: frozen for Alpha 2 and enforced. This table is the single inventory of every Gateway-generated outcome, one row per outcome. The unit tests in `src/status_contract_tests.rs` build the same rows from the code (`Reject::status_and_code`, the rendered response, the head guard's fixed bytes) and fail when any status, code, `Retry-After`, or the set of outcomes differs from this table, so neither can change without the other. The situational tables above remain the readable view; this one is the exact one. All bodies are the fixed `{"error":{"code":"<code>"}}` and carry no request, header, provider, or credential text.

**Pre-commit versus post-commit.** Every outcome in this table happens *before* any response byte is committed to the caller, so it has a real HTTP status and can be acted on by status. Once a response (a buffered JSON answer or the headers of an event stream) has begun, no status can change: a stream that fails afterwards ends abruptly without its terminating chunk and nothing is written into it (see "SSE streams"). That is a **post-commit truncation**, it has no status and no code, and the only reliable detection is the provider's own completion indicator (see "Detecting an incomplete stream").

Column meanings. `Provider bytes sent`: `no` means nothing of this request reached the provider; `yes` means the request was fully sent; `maybe` means it may have been (a header or total deadline elapsed). `SDK default retry`: whether the pinned OpenAI SDKs retry the status by default (`408`, `409`, `429`, every `5xx`); verified in #22 and re-verified in #60. The gateway itself never retries; a client retry is a new request. Where bytes may have reached the provider, a client retry can duplicate provider-side work.

| Outcome | Status | Code | Retry-After | Provider bytes sent | SDK default retry | Retry guidance |
| --- | --- | --- | --- | --- | --- | --- |
| `Reject::Method` | 405 | `unsupported_input` | - | no | no | Fix the call. |
| `Reject::Target` | 400 | `unsupported_input` | - | no | no | Fix the call. |
| `Reject::ContentType` | 415 | `unsupported_input` | - | no | no | Fix the call. |
| `Reject::Encoding` | 415 | `unsupported_input` | - | no | no | Send an uncompressed body. |
| `Reject::Framing` | 400 | `malformed_input` | - | no | no | Fix the framing. |
| `Reject::TooLarge` | 413 | `limit_exceeded` | - | no | no | Shrink the request. |
| `Reject::LimitExceeded` | 413 | `limit_exceeded` | - | no | no | Shrink the request. |
| `Reject::Deadline` | 408 | `limit_exceeded` | - | no | yes | Safe to retry: nothing was sent upstream. |
| `Reject::Overload` | 503 | `overload` | 1 | no | yes | Safe to retry after `Retry-After`: nothing was sent upstream. |
| `Reject::Malformed` | 400 | `malformed_input` | - | no | no | Fix the body. |
| `Reject::Unsupported` | 422 | `unsupported_input` | - | no | no | Fix the request; includes a `Block` or `Warn` finding. |
| `Reject::NotImplemented` | 501 | `not_implemented` | - | no | yes | No retry helps (no upstream configured); retries are wasted work. |
| `Reject::ShuttingDown` | 503 | `not_ready` | - | no | yes | Retry against another instance; this one is draining. |
| `Reject::MissingCredential` | 401 | `missing_credential` | - | no | no | Supply a provider `Authorization`. |
| `Reject::Header` | 400 | `malformed_input` | - | no | no | Fix the header (duplicate or malformed credential, organization, project, or `Connection`). |
| `Reject::HeaderTooLarge` | 431 | `limit_exceeded` | - | no | no | Shrink the headers. |
| `Reject::Expectation` | 417 | `unsupported_input` | - | no | no | Drop `Expect`, or use `100-continue`. |
| `Reject::Inspection(OutputLimit)` | 413 | `limit_exceeded` | - | no | no | Shrink the request. |
| `Reject::Inspection(Serialization)` | 422 | `unsupported_input` | - | no | no | Fix the request. |
| `Reject::Inspection(RouteMismatch)` | 500 | `incomplete_inspection` | - | no | yes | Wiring defect (the validated protocol is not the route's protocol); refused before inspection, nothing was sent upstream. Never caused by client input. |
| `Reject::Inspection(Core(UnsupportedProfile))` | 422 | `unsupported_input` | - | no | no | Configuration defect; fix the deployment. |
| `Reject::Inspection(Core(InvalidConfiguration))` | 422 | `unsupported_input` | - | no | no | Configuration defect; fix the deployment. |
| `Reject::Inspection(Core(LimitExceeded))` | 413 | `limit_exceeded` | - | no | no | Shrink the request. |
| `Reject::Inspection(Core(Blocked))` | 422 | `unsupported_input` | - | no | no | Remove the blocked content. |
| `Reject::Inspection(Core(Warned))` | 422 | `unsupported_input` | - | no | no | Remove the content, or change `content.on_warn`. |
| `Reject::Inspection(Core(Overload))` | 503 | `overload` | 1 | no | yes | Safe to retry after `Retry-After`: nothing was sent upstream. |
| `Reject::Inspection(Core(Incomplete))` | 500 | `incomplete_inspection` | - | no | yes | Inspection failed closed; nothing was sent upstream, so a retry cannot duplicate provider work. |
| `Reject::Transport(ClientInit)` | 502 | `transport_failure` | - | no | yes | Startup defect; not expected at request time. |
| `Reject::Transport(UnknownRoute)` | 501 | `not_implemented` | - | no | yes | No upstream for the route; retries are wasted work. |
| `Reject::Transport(Timeout)` | 504 | `upstream_timeout` | - | maybe | yes | A retry may duplicate provider work. |
| `Reject::Transport(Connect)` | 502 | `upstream_unavailable` | - | no | yes | Safe to retry: the request never left. |
| `Reject::Transport(Tls)` | 502 | `upstream_tls_failure` | - | no | yes | Retrying does not help until the deployment is fixed. |
| `Reject::Transport(InvalidResponse)` | 502 | `upstream_invalid_response` | - | yes | yes | A retry duplicates provider work that already happened. |
| `Reject::Transport(ResponseTooLarge)` | 502 | `upstream_response_too_large` | - | yes | yes | A retry duplicates provider work that already happened. |
| `HeadGuard::Ambiguous` | 400 | `malformed_input` | - | no | no | Send one of `Content-Length` or `Transfer-Encoding`. Fixed bytes written by the head guard; best effort on delivery. |
| `HeadGuard::TooLarge` | 431 | `limit_exceeded` | - | no | no | Shrink the head (over 64 KiB or over 100 fields). Fixed bytes; best effort on delivery. |
| `HeadGuard::LateHead` | none | none | - | no | yes | Connection closed without a response when the head is not finished by `body_deadline_ms`; the SDK sees a connection error and retries it. |
| `Connection::AtBound` | none | none | - | no | yes | Connection closed at accept when `max_connections` are open; the SDK sees a connection error and retries it. |

A provider's own `4xx`/`5xx` is a provider response, relayed as received, and is not in this table. The `501`, `500`, and `502` rows are retried by the SDK defaults although retrying cannot help; that is the client's status-based policy (the gateway does not relay `x-should-retry`), not a gateway decision. The two closed-connection rows have no status or code by design: nothing has been read or is safe to write.

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
| Abandoned | Caller disconnected, or no write progress for `stream_write_stall_ms`, or blocked-write time over the write budget (the connection is closed; [request-lifecycle](request-lifecycle.md)) | closed |

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
4. Stream truncation: Python raises `APIConnectionError`; Node.js raises on Node 24 but ended normally on Node 22.16.0 (see the SSE section), and any SDK ends normally when the provider ends a stream cleanly without a completion. Require `finish_reason` everywhere ("Detecting an incomplete stream"). The #60 matrix re-measured all of this, with retries disabled as well as at the default.
5. This is not a statement about other SDK versions or other HTTP clients, and not a statement about the real provider.

## Detecting an incomplete stream (#60)

A stream the Gateway could not finish ends abruptly, with no terminating chunk and nothing written into it (see "SSE streams"). Whether that cut is *reported* depends on the client stack, and a stream can also end cleanly and still be incomplete (a provider that closes early, or an intermediary that does). Callers must therefore decide completeness from the content, not from the iteration:

1. **Require the provider's completion indicator.** For Chat Completions, a chunk whose `choices[0].finish_reason` is non-null, and (where you read the raw events) the final `data: [DONE]` line. If you use an SDK, track `finish_reason` while iterating; if the loop ends without one, treat the text as truncated, discard it or mark it partial, and do not act on it.
2. **The end of an SDK iteration is not evidence of a valid completion.** Observed with the pinned SDKs (below): Node.js 22.16.0 ended the iteration normally for every broken stream; every SDK ended the iteration normally for a stream the provider ended cleanly after two events, with no `finish_reason` and no `[DONE]`. `[DONE]` is consumed by the SDKs and is not visible to SDK users, so `finish_reason` is the practical check.
3. **A raised error is a truncation signal, not the only one.** Python raised `APIConnectionError` and Node.js 24 raised in every cut case; neither is a guarantee for other versions.
4. **The Gateway never fabricates a completion**: it adds no event, no `finish_reason`, no `[DONE]` and no error event to any stream. Nothing a caller sees as complete was invented by the Gateway.
5. **Do not retry blindly.** A retried stream is a new request to the provider; see "SDK retry guidance" and the duplicate-work evidence below. Both SDKs never retry once the response headers are committed.

The shipped examples (`examples/node`, `examples/python`) check `finish_reason` and fail when it is absent; both run in the Qualification workflow.

## Alpha 2 transport qualification (2026-10-03, #60)

What was added to the qualification (everything synthetic, loopback only, NON-RELEASE build per [ADR 0020](../decisions/0020-sdk-qualification-test-build.md); nothing here is a statement about a real provider, other SDK versions, other runtimes, or an intermediary):

- **Status mapping frozen and enforced** ("Frozen status mapping (#60)" above; `src/status_contract_tests.rs`).
- **HTTP-level framing, coding and header qualification** in the crate's own test suite (`src/transport/tests/framing_matrix_tests.rs`, plus the request-head matrix already in `attack_tests.rs` and `header_cap_tests.rs`), against the loopback fake provider.
- **SDK matrix** (`qualification/sdk/node/test/matrix.test.ts`, `qualification/sdk/python/tests/test_matrix.py`): every scenario run twice per SDK, once with the SDK's default retries (2) and once with retries explicitly disabled, recording attempts, provider-side requests and bodies, and stream completion. Evidence is uploaded by the Qualification workflow as `matrix-observations-node.json` and `matrix-observations-python.json`.
- **Two Node.js runtimes in CI**: Node.js 24 runs everything; Node.js 22.16.0 (undici 6.21.2) runs the whole Node.js SDK suite, so the runtime difference is a tested fact.

### Request and response framing at the HTTP level

Rejected request framing never reaches the provider (`attack_tests.rs`: every case in `rejected_framing_cases` leaves the fake with zero connections or zero request bytes; `header_cap_tests.rs`: every head-size class). Request `Content-Encoding` of any value and any transfer coding other than `chunked` are `415 unsupported_input`. Provider-side, each case below is a single attempt, leak-scanned, and answered with the fixed body:

| Provider response | Caller sees |
| --- | --- |
| Declared `Content-Length` longer than the bytes sent; truncated chunked body; chunk larger than what follows; headers or status line cut short | `502 upstream_invalid_response` |
| Two different `Content-Length` values; a `Content-Length` that is not a number, has a sign, or overflows; chunk size not hexadecimal; status line that is not HTTP; header line with no colon | `502 upstream_invalid_response` |
| `101 Switching Protocols` (an informational final status; found by this work: it was relayed to the caller before, now refused) | `502 upstream_invalid_response` |
| `Content-Encoding` other than `identity`: `gzip`, `x-gzip`, `deflate`, `br`, `zstd`, `compress`, a list containing a coding, an unknown coding, `identity` followed by `gzip` on a second line | `502 upstream_invalid_response` |
| `Content-Length` shorter than the bytes sent | `200`; only the declared bytes are relayed, the extra provider bytes are dropped |
| Both `Content-Length` and `Transfer-Encoding: chunked` | `200`; the pinned HTTP client lets the chunked framing win and drops the length; neither framing header is relayed. A dependency upgrade that changes this fails `provider_response_ambiguities_the_client_resolves_are_pinned` |
| JSON with no length at all and `Connection: close` | `200`; read to the end of the connection (valid close-delimited framing). Event streams refuse this form (ADR 0018) |
| `Content-Encoding: identity` | `200`; the header itself is not relayed |

Hop-by-hop and connection-nominated headers are stripped in both directions: a provider `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Connection`, `Trailer`, `TE`, `Upgrade`, `Set-Cookie` and any header named in the provider's `Connection` (including an otherwise allowlisted `X-Request-Id`) are not relayed; a caller's `Connection`-nominated header, `Keep-Alive`, `Proxy-Authorization`, `Proxy-Connection`, `TE`, `Trailer` and `X-Forwarded-For` never reach the provider. Error bodies and headers in every case above carry no provider byte, request credential or payload text (leak scan with synthetic markers).

### Observed SDK matrix: failures before the response is committed

Same counts for the Node.js SDK (openai 7.27.0, Node.js 24.21.0 and 22.16.0) and the Python SDK (openai 3.24.0, Python 3.14.7 locally, 3.13 in CI). "Requests" are what the fake provider received; every retry arrived with the same sanitized body (identical bytes), as a fresh request: the Gateway sent each attempt once and replayed nothing.

| Scenario | Gateway answer | Default retries: SDK attempts / provider requests | Retries disabled: SDK attempts / provider requests |
| --- | --- | --- | --- |
| Provider closes before any response byte | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| Partial response head, then close | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| Non-HTTP status line | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| Two different `Content-Length` values | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| `Content-Encoding: gzip` | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| JSON body truncated | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| Provider stops reading a ~200 KB request body part way (partial upstream send; the provider kept about 65,000 bytes per attempt) | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| Response over the body bound | `502 upstream_response_too_large` | 3 / 3 | 1 / 1 |
| Provider never answers (tight limits) | `504 upstream_timeout` | 3 / 3 | 1 / 1 |
| Stream requested, provider closes before any response byte | `502 upstream_invalid_response` | 3 / 3 | 1 / 1 |
| Gateway overload (`Retry-After: 1`) | `503 overload` | 3 / 0 (the SDK waited the relayed second twice) | 1 / 0 |
| Connection to the Gateway refused | none (`APIConnectionError`) | 3 / 0 | 1 / 0 |

**Duplicate upstream work after delivery uncertainty.** In every `502`/`504` row the request had reached the provider, the Gateway could not tell whether the provider acted on it, and it answered once with a safe error. With the SDK default the application then sent it again twice: the provider received three requests, with identical bodies except in the partial-send row (where each attempt was cut at a different point). The Gateway performed no retry and no replay; the duplication is the client's. Explicitly disabling retries (`maxRetries: 0` / `max_retries=0`) limited every row to one provider request. Retries of the rows that never reached the provider (overload, refused) are safe.

### Observed SDK matrix: streams after the response is committed

Retry counts: every stream scenario below was one SDK attempt and one provider request, with the SDK default and with retries disabled; the SDK never retries once the headers are committed. "Raised" is whether the SDK iteration threw. Completeness is decided by `finish_reason`, not by "raised".

| Scenario | `finish_reason` seen | Python 3.24.0 | Node.js 24 (undici 7.29.1) | Node.js 22.16.0 (undici 6.21.2) |
| --- | --- | --- | --- | --- |
| Complete stream | yes (8 events) | ends normally | ends normally | ends normally |
| `finish_reason` arrives, no `[DONE]`, clean end | yes (8 events) | ends normally | ends normally | ends normally |
| Provider ends cleanly after two events (no `finish_reason`, no `[DONE]`) | **no** (2 events) | **ends normally** | **ends normally** | **ends normally** |
| Provider cut after two events | no (2) | raised `APIConnectionError` | raised | **ended normally** |
| Provider cut after the headers, before any event | no (0) | raised | raised | **ended normally** |
| Invalid chunk framing after the headers | no (1) | raised | raised | **ended normally** |
| Gateway idle deadline cut a stalled stream | no (2) | raised | raised | **ended normally** |

Reading the table: an iteration that ends normally says nothing about completeness. Node.js 22.16.0 never raised for a cut stream through the Gateway (which sends `Connection: close`); the same Node.js 22.16.0 fetch does raise for a cut on a keep-alive response (control in `streaming.test.ts`). The "ended cleanly after two events" row is not a transport fault at all and is invisible to any iteration-based check. No row shows a fabricated completion event.

A provider that closes in the same instant as it sends the headers can be observed either as a `502` before the commit or as a committed stream that is then truncated; which one the caller sees depends on whether the Gateway read the headers first. The matrix scenario waits 150 ms after the headers to be deterministic. Either way no completion is invented, and a retried pre-commit `502` duplicates provider work as above.

### Residual limitations

- Versions: pinned `openai` 7.27.0 (Node.js) and 3.24.0 (Python); Node.js 24 as resolved by `setup-node` and 22.16.0; Python 3.13 (CI). Nothing is claimed for other versions, other runtimes, other HTTP clients, or a real provider.
- The fake provider and a loopback network only: no TLS, no intermediary, no proxy, no real provider hints (`x-should-retry` and `retry-after-ms` are dropped by the Gateway, so a provider "do not retry" does not reach the SDK).
- Timing-dependent races (a provider closing exactly as headers are sent; delivery of a head-guard answer to a peer still sending) are documented as best effort, not asserted beyond the deterministic scenarios.
- No retry broker, no exactly-once or at-most-once claim, and no transparent interception claim. The Gateway never retries.

## Stage timings (ADR 0008)

`telemetry::Metrics` records, as count, total, and maximum microseconds, only these stages: admission wait, parse, inspection (including worker queueing), serialization (inside inspection, measured on the worker), upstream first response (send to response headers), and upstream total (send to last buffered byte or failure), plus a counter of upstream send attempts (and, for streams, the stages and counters above). The vocabulary is a closed enum: no payload, route, credential, URL, or caller-supplied value can become a label. There is no exporter yet; the counters are in-process (#20 adds the minimum, not a metrics platform).

## Status

Implemented: health, local rejection, config diagnostics, the Chat Completions admission/parse/limit mappings, inspection rejections (#19), and ordinary JSON forwarding, relay, transport-error mappings, and stage timings (#20). SSE relay, the stream termination contract, and stream timings and counters (#21). SDK qualification of both with the pinned Node.js and Python SDKs, and the observed retry and truncation behavior below (#22).
