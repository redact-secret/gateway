# ADR 0019: Request-head guard and one request per connection

Status: Accepted; implemented (#25). Implements the Alpha 1 minimum slice of the "slowloris on headers" and "`Content-Length` with `Transfer-Encoding` is invisible to the handler" known gaps recorded by #24 and #21. Wider transport hardening (connection-count limits, per-peer limits, header-size measurement, HTTP/2 policy) remains #10 (Alpha 2). Date: 2026-10-02. Amended by [ADR 0022](0021-framing-ambiguity-parser-level-investigation.md) (#43): the guard now answers an ambiguous head with a local `400 malformed_input` (and an over-long head with `431`) instead of closing silently; the "no response" statements below describe the original decision. The pinned stack cannot expose the ambiguity itself (evidence in ADR 0021).

## Context

Two properties of the served stack could not be controlled from the request handler:

1. **Framing ambiguity.** The HTTP layer (hyper, through axum) resolves a request that carries both `Content-Length` and `Transfer-Encoding: chunked` itself (RFC 9112 section 6.3): it discards the length and frames the body as chunked before any handler runs, and it keeps the connection alive. The handler therefore never sees the conflict, so the gateway's own rejection (`Reject::Framing`) was unreachable. The provider connection was never affected (outbound framing is regenerated), but a front proxy that frames the same bytes differently would disagree with the gateway about where the request ends: a request-smuggling precondition on the inbound side.
2. **No read-side bound before the handler.** The write-stall deadline (#21) bounds a stuck *write*. A peer that connects and sends nothing, or sends a request head one byte at a time, held a socket (and a connection task) indefinitely, because no timeout applies until the handler runs and reserves a body-receipt budget.

The route's own header limits (16 KiB total, 8 KiB per value, `431`) are applied after parsing, so they do not bound what is read before the head is complete either.

## Decision

1. **Head guard on the byte stream** (`src/head_guard.rs`). Every accepted connection is wrapped so that the *first* request head is held back, scanned, and only then handed to the HTTP parser. The guard reads field names only (never values), treats `\n` with an optional `\r` as a line end (the more permissive reading, so a head the parser would end is ended here too), ignores leading blank lines like the parser, and decides:
   - a head carrying both a `Content-Length` and a `Transfer-Encoding` field (any spelling of the case, any whitespace before the colon, in either order, even with an invalid length) is **ambiguous**: the connection is failed with an IO error before the parser sees a byte. No handler runs, nothing is read from the body, **no response is written** (the connection is closed);
   - a head that is not complete within the head deadline, or longer than 64 KiB without ending, fails the connection the same way;
   - otherwise the held bytes are handed to the parser unchanged, followed by the rest of the stream.
2. **Head deadline reuses `resources.limits.body_deadline_ms`** (provisional 10,000 ms, ceiling 300,000). It is an absolute deadline from accept to the end of the head, not an idle timer, so trickling bytes cannot extend it. No new configuration field and no new schema version: the existing value now also bounds the time to receive the request head, and the contract says so. A separate field can be introduced with measurement (ADR 0008) if the two should differ.
3. **One request per connection.** Every response, including gateway-generated errors, health, and unmatched routes, carries `Connection: close` (`server::guarded_app`). Hyper then closes the connection after the response, so bytes after the first request (a pipelined or smuggled second message) are never parsed as a request, and the guard needs to inspect only the first head. This costs loopback clients one TCP handshake per request, which is small next to a model call, and matches the already-disabled provider connection pooling (ADR 0017). Keep-alive for local callers is not a stated requirement. (Measured in #42, [ADR 0024](0024-connection-reuse-measurement-and-decision.md): the setup cost is about 37 us for one client and about 90 us under eight; one request per connection stays.)
4. **No task and no queue.** The deadline is a timer polled inside the read that is pending, like the write-stall wrapper. Held bytes are bounded by 64 KiB per connection and are released to the parser or dropped with the connection.
5. **Tests use the production wiring.** `server::guarded_listener` and `server::guarded_app` are the single place the connection handling is composed; the in-crate adversarial suite serves its router through them, so the tests exercise the same guard as the binary.

## What this does not claim

- It is a structural check on two framing fields, not a general HTTP parser and not a proof against all request smuggling. Other differences between a front proxy and hyper (header-name tokenization, line-ending leniency, `Transfer-Encoding` obfuscation that both treat as unknown) remain the operator's to eliminate by not placing a non-validating intermediary in front of the gateway, or by using one that rejects ambiguous requests (see the control map).
- A rejected-by-guard request gets **no response**. A client sees a closed connection, not a `400`. This is the price of acting before the parser; a gateway-written `400` would require either a second parser or parsing the head twice. SDKs generally surface it as a connection error; their retry behavior is SDK-specific and is qualified in #22, and the control map lists it.
- (Superseded by #40, [ADR 0022](0022-connection-bound-at-accept.md): the connection count is now bounded at accept.) It did not limit the number of concurrent connections. An attacker who can open many sockets still consumes file descriptors and a connection task each until the head deadline closes them. Connection-count and per-peer limits are tracked for #10.
- HTTP/2 is not enabled (`http1` only); the guard is HTTP/1 specific.
- (Measured and confirmed by #41, [ADR 0023](0023-header-size-measurement-and-size-classes.md).) The 16 KiB / 8 KiB header bounds and the 64 KiB hold bound were not measured values (ADR 0008).

## Owner

`transport`-adjacent connection handling in the server module; the maintainer approves changes to what the guard rejects.

## Invariants

1. The HTTP parser never sees a first head that carries both length and transfer-coding fields.
2. A connection that does not deliver a head within the head deadline is closed without holding a receipt or memory reservation (none is taken before the head is complete).
3. Every response asks for the connection to close, so at most one request per connection reaches the route.
4. The guard stores no header value and logs nothing.

## Failure behavior

Ambiguous, oversized, or late heads close the connection silently. Everything else is unchanged: the route returns its documented safe codes.

## Verification

`src/head_guard.rs` unit tests (scan table including bare-LF, spacing, early detection, name-only matching; byte-at-a-time heads; absolute deadline under trickle; hold bound; truncated head; write pass-through). `src/transport/tests/attack_tests.rs` (served stack with a fake provider: every ambiguous case closes with zero upstream bytes, pipelined and smuggled second messages never run, silent, partial, and trickling heads are cut). `tests/attack_surface.rs` (real binary with a hostile environment). `tests/header_credentials.rs` and `tests/chat_admission.rs` pin the changed outcome of `Content-Length` plus `Transfer-Encoding`.

## Deferred

Connection-count and per-peer limits, measured header bounds, a separate head-deadline field, and an HTTP/1 parser option that rejects the ambiguity itself (not exposed by the pinned stack through `axum::serve`) are #10 or later.
