# ADR 0023: Request-head size measurement and the size classes

Status: Accepted; implemented (#41). Follow-up of ADR 0019 (#25) and ADR 0021 (#43); sizes the per-connection cost recorded by ADR 0022 (#40). Owned by the transport hardening epic (#10). Date: 2026-10-02.

## Context

Three numbers bound a request head, and until now none was measured (ADR 0008): the route refuses header names plus values over 16 KiB and any value over 8 KiB (`431`, ADR 0016), and the head guard holds at most 64 KiB while waiting for the end of the head (ADR 0019). The HTTP layer's own header-count limit applied first in some cases and answered with a different, bare `431`. #41 asks for measured values, boundary tests on both sides, and one documented outcome per size class.

## Measurement

Tool: `scripts/measure/header-sizes.mjs` (with `header-sizes-python.py`; setup in `scripts/measure/README.txt`). It is outside the product binary and outside the qualification build seam; the seam allowlist is untouched. No gateway is involved: the pinned OpenAI SDKs send one `chat.completions.create` request to a raw TCP listener that records the request head. Credentials are synthetic (`sk-SYNTHETIC-NOT-A-KEY-...` padded to the stated length). Raw results: `docs/qualification/header-size-measurement.json`.

Pins and host: openai Node 7.27.0 (`qualification/sdk/node/package-lock.json`), on Node v22.16.0 (the suites require Node 24 or newer; the SDK's request headers do not depend on the major version beyond the `x-stainless-runtime-version` value, a one-byte difference at most); openai Python 3.24.0 (`qualification/sdk/python/requirements.txt`, hash-locked) on Python 3.13.15; Darwin 25.5.0 arm64. Header sizes are a function of the bytes the SDK writes, so they are independent of host load.

What is measured and what is modelled. The SDK-generated headers (including `OpenAI-Organization` and `OpenAI-Project`, and long tokens) are **measured** from the pinned SDKs. Proxy, tracing, and cookie headers are **modelled**: they are added through the SDK's default-header option at stated sizes, which puts the same bytes in the head that an intermediary inserting them would. No real proxy was captured, so the proxy sizes are assumptions about typical deployments (shapes: `Forwarded` and `X-Forwarded-*` chains, `Via`, `X-Request-ID`; W3C `traceparent`, `tracestate` up to 512 bytes, `baggage` up to the W3C limit of 8192 bytes; cookies of 1, 4, and 8 KiB). Columns: `fields` is the header-line count, `name+value` is what the route's 16 KiB cap counts (Node / Python), `head` is what the guard's 64 KiB bound counts, request line through the blank line (Node / Python), `largest value` is what the 8 KiB cap counts (the same for both SDKs).

| Case | Fields | Name+value N / P | Head N / P | Largest value |
| --- | ---: | ---: | ---: | --- |
| baseline, 51-byte key | 17 | 437 / 462 | 543 / 568 | 58 (`authorization`) |
| plus organization and project | 19 | 527 / 552 | 641 / 666 | 58 |
| 164-byte project-key shape | 19 | 640 / 665 | 754 / 779 | 171 |
| token at the 512-byte cap | 19 | 988 / 1,013 | 1,102 / 1,127 | 519 (`authorization`) |
| custom user-agent addition (+82) | 19 | 717 / 742 | 831 / 856 | 171 |
| proxy: `Forwarded`, `X-Forwarded-*`, `Via`, request id | 27 | 926 / 951 | 1,072 / 1,097 | 171 |
| proxy: 10-hop IPv6 chains, 5 `Via` hops | 27 | 1,585 / 1,610 | 1,731 / 1,756 | 419 (`forwarded`) |
| tracing: `traceparent`, small `tracestate`/`baggage` | 22 | 1,123 / 1,148 | 1,249 / 1,274 | 200 |
| tracing: `baggage` 1 KiB, `tracestate` 512 | 22 | 2,259 / 2,284 | 2,385 / 2,410 | 1,024 |
| tracing: `baggage` 8,192 (W3C limit) | 22 | 9,427 / 9,452 | 9,553 / 9,578 | 8,192 |
| cookies 1 KiB | 20 | 1,670 / 1,695 | 1,788 / 1,813 | 1,024 |
| cookies 4 KiB | 20 | 4,742 / 4,767 | 4,860 / 4,885 | 4,096 |
| cookies 8,192 | 20 | 8,838 / 8,863 | 8,956 / 8,981 | 8,192 |
| typical combination (project key, org/project, UA, proxy, small tracing, 1 KiB cookies) | 31 | 2,516 / 2,541 | 2,678 / 2,703 | 1,024 |
| heavy combination (512-byte token, UA, long chains, 1 KiB baggage, 4 KiB cookies) | 31 | 7,731 / 7,756 | 7,893 / 7,918 | 4,096 |
| extreme combination (heavy with 8,192-byte baggage) | 31 | 14,899 / 14,924 | 15,061 / 15,086 | 8,192 |

Readings. The SDKs alone send 17 to 19 fields and under 1.2 KiB. A realistic deployment with a project key, organization and project headers, a custom user agent, a proxy, tracing, and cookies is about 2.5 KiB. The largest case stacks the credential cap, long forwarding chains, the largest `baggage` the W3C allows, and 4 KiB of cookies: 14.9 KiB of names and values, 31 fields, 15 KiB on the wire. The Python SDK's head is 25 bytes larger than the Node SDK's in every case.

## Decision

The caps are **confirmed**, not changed; the one change is that the field-count limit is stated by the gateway instead of being discovered.

| Limit | Value | Evidence and headroom |
| --- | --- | --- |
| Header names plus values (route) | 16 KiB (16,384) | Typical case 2.5 KiB (6.5x headroom); heavy 7.8 KiB (2.1x); the stacked extreme 14.9 KiB (1.1x, 1,460 bytes). The same figure as the default header-size limit of Node's own HTTP parser (recalled from its documentation, not verified here), so a request that came through a Node-based hop plausibly fits. |
| One header value (route) | 8 KiB (8,192) | Exactly the W3C `baggage` limit (8,192 is admitted, 8,193 is not); the 512-byte token is 519 bytes with its scheme, 16x under. A `Cookie` value over 8 KiB is refused even though the gateway ignores cookies; common servers apply a per-line limit of about this size themselves (stated from vendor documentation as recalled, not verified here), so such a request rarely survives an intermediary anyway. Raising it is a one-constant change if a deployment needs it. |
| Header fields (head guard, new constant `MAX_HEAD_FIELDS`) | 100 | The HTTP server's own limit, verified empirically: the real server admits 100 fields and refuses 101 (a test that moved the guard's constant to 101 failed on the server's refusal). Measured maximum 31 (3.2x). Now stated and enforced by the guard so the answer is the fixed `431`; see below. |
| Head, request line through the blank line (head guard) | 64 KiB (65,536) | The largest head the route can admit is under 17 KiB (16 KiB of names and values, 4 separator bytes per field for at most 100 fields, a request line); the measured extreme is 15 KiB. 64 KiB is about 3.8x that. It is a memory bound, not a size policy: a head between 16 KiB and 64 KiB is refused by the route with a fixed-shape `431`, so lowering the bound would change who answers, not the status. Held memory per connection is therefore unchanged from ADR 0022 (about 71 KiB measured with a nearly full head, about 17 MiB at the default of 256 connections). A 32 KiB bound would halve that and still clear the measured extreme twice over; it was not chosen because it saves memory only on a hostile or broken peer and would move the figures ADR 0022 recorded. |

Provisional or measured. The SDK-generated sizes are measured with the named pins. The intermediary sizes are modelled (above) and the caps remain chosen with headroom over them, so the limits stay **provisional** in the sense of ADR 0008: they follow evidence from two SDKs and modelled intermediaries, not from a deployed fleet. Rerun the tool after an SDK pin change and revisit if a deployed intermediary adds more than about 8 KiB of headers.

### How the limits relate to the HTTP layer

The HTTP server (hyper 1.11.1 under axum 0.8.9, httparse 1.10.1) has its own header-count limit of 100 and a read-buffer limit. The guard now applies both of its own bounds first, in the byte stream, so for every head the guard answers and the route answers, the HTTP server's limits are never the first to speak: the count is the same number (100), and no head over 64 KiB reaches the server at all, so its read-buffer limit is not the first to speak for any head the guard passes (no test probes that buffer limit directly). Before this change a head with more than 100 fields received the server's own `431` (empty body, no `limit_exceeded` code), and a head longer than 64 KiB that ended inside the guard's last read was passed to the route instead of being refused by the guard. Both are now answered by the guard's fixed `431 limit_exceeded`.

### Outcome by size class

| Class | Condition | Who answers | Outcome |
| --- | --- | --- | --- |
| Within all limits | head at most 64 KiB, at most 100 fields, names plus values at most 16,384, each value at most 8,192 | route | admitted (later admission outcomes apply) |
| Route byte caps | head at most 64 KiB and at most 100 fields, but names plus values over 16,384 or one value over 8,192 | route | `431 limit_exceeded` (gateway response with the usual headers) |
| Field count | more than 100 fields | head guard | fixed `431 limit_exceeded` constant, connection closed |
| Head bound | head over 65,536 bytes (ended or not, however the bytes arrive) | head guard | the same fixed `431 limit_exceeded` constant, connection closed |
| Late head | head not finished by `body_deadline_ms` | head guard | connection closed, no response |
| Ambiguous framing | `Content-Length` with `Transfer-Encoding` | head guard | fixed `400 malformed_input` (ADR 0021) |

When one head triggers several guard conditions, the one reached first while scanning the head decides (`400` for ambiguous framing and `431` for a size or count refusal are both refusals; the choice between them for a head that is both is not a contract). In every row except the first, nothing is reserved and no byte reaches the provider; no refusal contains a request-derived byte or credential.

The boundary is exact: a head of exactly 65,536 bytes and exactly 100 fields passes the guard; one byte or one field more is refused by it. Over the route's caps the guard is not consulted and the route answers, so a head of exactly 65,536 bytes made of padding is answered by the route's `431`, not the guard's.

## What this does not claim

- The intermediary sizes are modelled, not captured from real proxies. The caps say nothing about intermediaries that add more than the modelled amounts.
- Measurements cover the `chat.completions.create` call of the two pinned SDK versions on one machine. Other SDK versions and runtimes, and the streaming call, send the same header set but were not captured.
- The `431` for a head that is over a limit is best effort on delivery in the same way as the `400` of ADR 0021: if the peer is still sending, the OS may reset the connection.
- Per-connection memory is unchanged, not re-measured: the held-head bound did not change.
- No refusal counter exists yet (telemetry design, #10).

## Owner

The transport hardening epic (#10); `transport/headers` owns the route caps and `head_guard` owns the count and head bounds. The maintainer approves changes to a cap.

## Invariants

1. Memory held for a head that has not been judged is at most `MAX_HEAD_BYTES` plus one read chunk per connection, regardless of the peer.
2. Every head over a bound is refused with a fixed constant or a fixed-code gateway response; none carries a request-derived byte, and the provider sees nothing.
3. The count limit equals the HTTP server's, so the server's own refusal cannot answer first.
4. The route's byte caps stay below the guard's bound with room for the separators of the most fields it admits (checked at compile time in `header_cap_tests.rs`).

## Verification

`src/transport/tests/header_cap_tests.rs` (production connection handling, real chat route, loopback fake provider): total names plus values at 16,384 admitted and forwarded once and at 16,385 refused; one value at 8,192 admitted and at 8,193 refused; 100 fields admitted and 101 and 1,000 refused with the guard's constant; a head of exactly 65,536 bytes answered by the route and 65,537 by the guard, also delivered in small pieces and when it never ends; a late head closed without a response. Every refusal asserts zero upstream connections and bytes and no credential or padding marker on the wire. `src/head_guard.rs` unit tests (exact scan boundaries for length and count, independence from read splitting). `src/transport/tests/attack_tests.rs` (the guard's `431` answers exactly the over-bound heads). The measurement is reproducible with `scripts/measure/header-sizes.mjs`.

## Deferred

Capturing real intermediaries; re-measuring on SDK pin changes; a refusal counter; per-connection memory under the stated bound on a quiet host (ADR 0022).
