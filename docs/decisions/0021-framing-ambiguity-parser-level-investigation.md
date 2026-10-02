# ADR 0021: Framing ambiguity at the parser level: investigation and a local 400

Status: Accepted; implemented (#43). Follow-up of ADR 0019 (#25); owned by the transport hardening epic (#10). Date: 2026-10-02.

## Context

ADR 0019 closes the `Content-Length` plus `Transfer-Encoding` ambiguity with a byte-stream guard that scans the first request head's field names. Two residuals were recorded: a rejected client sees a closed connection instead of a `400`, and the guard is a second, independent reading of the head. #43 asked whether the pinned HTTP stack can expose or reject the ambiguity itself, so the check would not be a second reading and the gateway could answer locally.

## Investigation (pinned stack: axum 0.8.9, hyper 1.11.1, hyper-util 0.1.21, httparse 1.10.1, http 1.5.0; sources read from the registry copies of those exact versions)

1. **hyper `http1::Builder` options.** The options are `half_close`, `keep_alive`, `title_case_headers`, `allow_multiple_spaces_in_request_line_delimiters`, `ignore_invalid_headers`, `preserve_header_case`, `max_headers`, `header_read_timeout`, `writev`, `max_buf_size`, `auto_date_header`, `pipeline_flush`, `timer`. None rejects or reports a request that carries both framing fields.
2. **The parser resolves it silently, before any hook.** `proto/h1/role.rs` (server `parse`): on a `Transfer-Encoding` field it executes `if is_cl && con_len.take().is_some() { headers.remove(header::CONTENT_LENGTH); }`, and on a `Content-Length` field after a `Transfer-Encoding` it executes `if is_te { continue; }`. The length is therefore discarded in both orders (and an invalid length after `Transfer-Encoding` is never even validated), and the request is framed as chunked. The `http::Request` that reaches a service, or a middleware, has no `Content-Length` and no trace that one existed. A service-level or `tower` layer check is impossible for this reason.
3. **`axum::serve` offers no hook.** `axum 0.8.9` `serve/mod.rs` builds `hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())` inline and calls `serve_connection_with_upgrades`; the only configuration it exposes is graceful shutdown and the `Listener`. The builder cannot be customised, so the `http1` options above are unreachable anyway.
4. **A manual hyper-util accept/serve loop** would reach the `http1::Builder`, but the options in (1) are all there is, and (2) holds for every entry point of hyper: the discarded length is lost inside the parser. A manual loop would additionally add direct `hyper`/`hyper-util` dependencies, and replace the accept loop, graceful shutdown, and drain handling now provided by `axum::serve` (shared with the accept-time connection work of #40).
5. **An IO wrapper that parses with `httparse`** (the same parser hyper uses) would read the same head twice. It is no more "one reading" than the byte scan; it adds a full header parser and its allocation and limits on untrusted bytes in front of hyper, with a risk of the two parsers disagreeing, which is the smuggling class the guard exists to close.

Conclusion: the pinned stack cannot expose or reject the ambiguity itself, through any entry point. The check must read the head before hyper does. The second-reading residual stays and is documented.

## Decision

Keep the byte-scan guard as the single detector, and change what it does on a hit so the caller gets a safe local answer:

1. **Ambiguous head: fixed `400 malformed_input`.** `head_guard::AMBIGUOUS_FRAMING_RESPONSE` is a constant byte string with the error-contract body, `Content-Type: application/json`, `Cache-Control: no-store`, `Connection: close`, and an exact `Content-Length`. No request-derived byte is in it. The guard writes it through the connection's IO (which sits above the write-stall wrapper), flushes, shuts down the write side, and then fails the read with an IO error, so the HTTP parser never sees a byte of the head, no handler runs, and nothing is read from the body.
2. **Head over 64 KiB without ending: fixed `431 limit_exceeded`**, the code the contract already gives for header bytes over the limit, written the same way.
3. **Late head (slowloris): still closed silently.** A peer that is not delivering is not answered.
4. **Bounded.** The refusal write runs under the same absolute head timer started at accept, so a peer that does not read its refusal is cut at `body_deadline_ms`; no task and no queue are added. Held head bytes are dropped when the refusal starts and nothing after the head is ever released to the reader.
5. **Detector unchanged.** The scan, the head deadline, the 64 KiB hold bound, and `Connection: close` on every response are unchanged; the guard still judges only the first head, and a pipelined or smuggled second message is never parsed.

## What this does not claim

- The guard remains a second, structural reading of two field names; it is not a general parser and not a proof against all smuggling. The deployment-chain requirements in the control map are unchanged.
- Delivery of the `400` is best effort. The write side is shut down after the response, but if the peer is still sending (a large body in flight) when the connection is dropped, the operating system may answer with a reset and the peer may see a reset instead of the response. The request is refused either way. The gateway does not read and discard the rest of the stream: that would keep a hostile peer's connection alive.
- HTTP/2 remains disabled; the guard is HTTP/1 specific.
- If a future hyper or axum release exposes the ambiguity (a parser option, or a server entry point that hands the raw head to a hook), this ADR should be revisited; the pins are re-recorded in the control map for that purpose.

## Owner

`transport`-adjacent connection handling in the server module; the maintainer approves changes to what the guard rejects and answers.

## Invariants

1. The HTTP parser never sees a first head that carries both length and transfer-coding fields.
2. The only bytes the guard ever writes are the two fixed constants; they contain nothing from the request.
3. The refusal path is bounded by the head deadline and takes no memory reservation or receipt.
4. Every response, including the refusals, carries `Connection: close`.

## Failure behavior

Ambiguous head: `400 malformed_input`, connection closed. Head over 64 KiB: `431 limit_exceeded`, connection closed. Late head: closed without a response. Everything else is unchanged.

## Verification

`src/head_guard.rs` unit tests (refusal constants are well formed with exact lengths; byte-at-a-time ambiguous head is answered; pipelined bytes after an ambiguous head are never released; a peer that never reads the refusal is cut at the deadline; over-bound head gets the `431`). `src/transport/tests/attack_tests.rs::ambiguous_and_malformed_framing_never_delivers_upstream_bytes` (every case in `rejected_framing_cases` is now answered, none closes silently; every ambiguous head receives exactly the fixed response; zero upstream bytes). `tests/chat_admission.rs`, `tests/header_credentials.rs`, `tests/attack_surface.rs` (real binary) pin the changed outcome.

## Deferred

Connection-count and per-peer limits (#10, #40), measured header bounds, and a parser-level rejection if the stack ever exposes one.
