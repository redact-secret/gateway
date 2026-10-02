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
| `transport_failure` | Upstream connect/TLS/transmission failure |

Errors never echo payload fragments, credentials, or offending text. Provider response and error bodies are relayed under the response contract and may contain sensitive data. Document status mappings and SDK-retry implications. After response bytes start, errors cannot change the HTTP status; terminate per the stream error contract without fabricating completion events.

## Telemetry

Allowed: bounded route IDs, version identifiers, coarse outcomes, timing, aggregate counters.

Excluded: bodies, raw URLs and query values, keys and credentials, raw findings, matched snippets, unbounded labels, detector scoring internals.

Health reports liveness. Readiness reports valid plan, initialized core, and ability to accept work. Neither performs a credentialed upstream probe by default.

## Status

Planned.
