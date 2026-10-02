# Contract: safe errors and telemetry

Status: approved categories; exact code spellings and HTTP status mappings are fixed in #4/#18 and documented here when implemented.

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

Errors never echo payload fragments, credentials, or offending text. Provider response and error bodies are relayed under the response contract and may contain sensitive data. Document status mappings and SDK-retry implications. After response bytes start, errors cannot change the HTTP status; terminate per the stream error contract without fabricating completion events.

## Telemetry

Allowed: bounded route IDs, version identifiers, coarse outcomes, timing, aggregate counters.

Excluded: bodies, raw URLs and query values, keys and credentials, raw findings, matched snippets, unbounded labels, detector scoring internals.

Health reports liveness. Readiness reports valid plan, initialized core, and ability to accept work. Neither performs a credentialed upstream probe by default.

## Implemented in the skeleton (#4)

Spellings are the `as_str()` values of `telemetry::SafeCode` (`malformed_input`, `unsupported_input`, `limit_exceeded`, `incomplete_inspection`, `overload`, `transport_failure`, `invalid_config`, `not_ready`). Gateway-generated bodies are `{"error":{"code":"<code>"}}`.

| Situation | Status | Code |
| --- | --- | --- |
| Unknown route or method (includes every proxy/model route in the skeleton) | 404 | `unsupported_input` |
| `GET /readyz` while not ready | 503 | `not_ready` |

Other status mappings and SDK-retry implications land with #18 and the routing issues. Configuration diagnostics (`invalid_config: <kind> at <schema location>`) are described in [configuration](../configuration.md). Health endpoints never call an upstream.

## Status

Partially implemented (health, local rejection, config diagnostics). The rest is planned.
