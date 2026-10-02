# Contract: safe errors and telemetry

Status: approved categories; code spellings fixed in #4; HTTP status mappings for the Chat Completions admission path fixed in #18 (below). Mappings for transport failures and relayed responses land with #20/#21.

## Gateway-owned error categories

| Category | When |
| --- | --- |
| `malformed_input` | Bad JSON, duplicate keys, invalid UTF-8, bad framing |
| `unsupported_input` | Unknown route/method/field/content form, compression, unsupported content |
| `limit_exceeded` | Any resource-limit category |
| `incomplete_inspection` | Core did not report complete inspection |
| `overload` | Admission refused or timed out |
| `transport_failure` | Upstream connect/TLS/transmission failure (skeleton: also listener bind/serve failure at startup) |
| `invalid_config` | Startup only: static configuration failed validation. Never a request outcome. |
| `not_ready` | Readiness is false: validated plan or required initialization is missing. |
| `not_implemented` | A request passed admission, validation, inspection, and approval but forwarding does not exist yet (#18/#19; removed when #20 lands). Never forwarded. |

Errors never echo payload fragments, credentials, or offending text. Provider response and error bodies are relayed under the response contract and may contain sensitive data. Document status mappings and SDK-retry implications. After response bytes start, errors cannot change the HTTP status; terminate per the stream error contract without fabricating completion events.

## Telemetry

Allowed: bounded route IDs, version identifiers, coarse outcomes, timing, aggregate counters.

Excluded: bodies, raw URLs and query values, keys and credentials, raw findings, matched snippets, unbounded labels, detector scoring internals.

Health reports liveness. Readiness reports valid plan, initialized core, and ability to accept work. Neither performs a credentialed upstream probe by default.

## Implemented in the skeleton (#4)

Spellings are the `as_str()` values of `telemetry::SafeCode` (`malformed_input`, `unsupported_input`, `limit_exceeded`, `incomplete_inspection`, `overload`, `transport_failure`, `invalid_config`, `not_ready`). Gateway-generated bodies are `{"error":{"code":"<code>"}}`.

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
| Admitted and validated; forwarding not implemented yet | 501 | `not_implemented` | yes (5xx) |

After admission, inspection and approval (#19) add these rejections, all local, with no upstream byte and the same fixed body:

| Situation | Status | Code | SDK retries |
| --- | --- | --- | --- |
| `Block` finding; `Warn` finding while `content.on_warn` is `reject` (default); a finding in `model` | 422 | `unsupported_input` | no |
| Request-wide text-byte or finding limit exceeded; transformed output over its bound | 413 | `limit_exceeded` | no |
| Detector, policy, or placeholder failure; discarded or panicked inspection job | 500 | `incomplete_inspection` | yes (5xx) |
| No inspection permit or queue slot (`Retry-After: 1`) | 503 | `overload` | yes, after `Retry-After` |

Rationale for the mappings: a caller cannot fix `5xx`/`408` by changing the request, so those are the retryable ones (transient capacity, slow client); everything the caller must change is a non-retried `4xx`. `413` is used for every limit rather than `400` so a caller can tell "too big" from "wrong". `422` separates a request that parses but is not in the supported subset from one that does not parse at all.

Rejections made by the HTTP layer before the handler runs (for example a `Content-Length` that is not a valid integer) are bare `400` responses without this body; they are still local and still send nothing upstream.

## Status

Implemented: health, local rejection, config diagnostics, and the Chat Completions admission/parse/limit mappings above. Planned: transport-failure and relayed-response mappings (#20, #21), and the `incomplete_inspection` outcome (#19).
