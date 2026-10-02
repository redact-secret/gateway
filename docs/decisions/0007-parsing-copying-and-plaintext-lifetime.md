# ADR 0007: Parsing, copying, and plaintext lifetime

Status: Accepted (design); parser selection deferred to measured need. Implementation status: baseline implemented in #18 (budgeted duplicate-key-rejecting parse, one working structure moved into the typed request, original buffer released after parsing; [ADR 0014](0014-chat-completions-admission.md)); borrowed/SIMD parsing and measured budgets remain deferred. #19 adds the in-place text replacement on that one structure, a single bounded serialization of a fresh document from it, and the transformed-output bound ([ADR 0015](0015-core-inspection-and-request-transformation.md)). Covers issue #3 section F.

## Context

The gateway holds plaintext secrets in memory by design. Extra copies increase both memory pressure and exposure time. JSON ambiguity (duplicate keys, escapes) can bypass inspection.

## Decision

- Use a duplicate-key-rejecting JSON path. Inspect decoded strings.
- Start with one parsed structure as the working representation. No full-body clone between phases. Serialize bounded output once.
- Release the original receive buffer as soon as it is no longer needed. If the parser borrows from it, the buffer lifetime extends, and the `MemoryReservation` must account for that.
- Budget parsed nodes, decoded strings, findings, and transformed output, in addition to wire bytes.
- Diagnostics and error values do not own sensitive bodies or credentials.
- Secure erasure of all allocator and TLS copies is not promised. Do not claim it.
- Borrowed or SIMD parsing is adopted only on measured need and must keep every rejection semantic.

## Owner

`protocol` (parsing and classification), `boundary` (budgets and approval). #5 supplies measurements.

## Invariants

1. Duplicate keys, invalid UTF-8, and malformed JSON are rejected.
2. Inspection operates on decoded text, not raw escaped bytes.
3. No raw-serialized-JSON text replacement.
4. Error and diagnostic types hold no body or credential content.
5. Any optimized parser passes the identical rejection test suite as the baseline parser.

## Failure behavior

Parse, budget, or serialization-limit failure rejects with a safe code and no upstream body.

## Implementation handoff

- #5: measure parse versus core versus serialization cost; report whether parser optimization is warranted.
- #6: baseline parser harness for duplicate keys and decoded Unicode; the same tests gate later optimized paths.
- #19: MVP parser wiring.

## Verification

Duplicate-key, escape, and Unicode test vectors (synthetic). Leakage tests on error values. Memory accounting tests for buffer lifetime.

## Deferred measured choices

Parser library, borrowed/SIMD parsing, node/string/finding/output budgets. Owner: #5 measures, maintainer approves. Criteria: measurable benefit on ADR 0008 workloads without weakening rejection behavior.
