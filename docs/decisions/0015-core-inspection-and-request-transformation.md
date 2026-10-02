# ADR 0015: Core inspection and validated request transformation

Status: Accepted. Implementation status: implemented for inspection, policy decisions, serialization, and `boundary::approve` (#19); the approved request is forwarded by #20 ([ADR 0017](0017-json-forwarding-deadlines-and-cancellation.md)); `stream: true` is inspected exactly like any request and relayed as SSE by #21 ([ADR 0018](0018-sse-relay-termination-and-stream-bounds.md)). Date: 2026-10-02. Implements the #3 design contracts for issue #19: [ADR 0002](0002-request-states-and-forwarding-authority.md), [ADR 0004](0004-cancellation-and-synchronous-core-work.md), [ADR 0007](0007-parsing-copying-and-plaintext-lifetime.md). Evidence: [core bridge probe](../probes/core-bridge-probe.md) and the tests listed below.

## Context

#18 produces a `ValidatedRequest` (typed `ChatRequest`, memory reservation, receipt permit). #19 must turn it into a `SanitizedRequest` only after every allowed model-bound text has been inspected by the pinned core with complete success, and must make several policy decisions the probe left open: what a `Block` finding does, what a `Warn` finding does, whether object keys are free text, what already redacted input means, and how a fresh body is produced and bounded.

## Decision

1. **Core and profile.** The pinned core is `redact-secret =0.1.0-beta.12` (ADR 0011). The profile (`full` or `common`) and an optional PII selection come from the startup `content` policy and are validated by the core's own parsers (`pii:kr` is refused). Detection stays in core; the gateway implements no detector, regex, or scoring.
2. **Where inspection runs.** Dedicated worker threads in `core_bridge::pool::InspectionPool`, started in `Services::init` (readiness depends on every worker building its core registry; the explicit initialized flag stays). One registry per worker, no shared registry, no global scanner lock. Workers are `min(inspection permits, available CPUs, 16)` and the queue is `min(inspection permits, 1024)`: both derive from the existing `resources.capacity.inspection`, so there is no new capacity setting, and queued plus running jobs can never exceed the permits. These numbers are provisional pending ADR 0008 measurement; `spawn_blocking` remains an unmeasured alternative.
3. **Job-owned capacity.** `Inspection::inspect_and_approve` takes an `InspectionPermit` (`try_inspection`, no waiting: a refusal is `503 overload`) and moves the whole `ValidatedRequest`, including its `MemoryReservation`, into the job (`InspectionPool::submit_job`). The job returns the request and the completeness proof. Dropping the awaiting future cancels only the wait: a queued job is skipped when dequeued, a started job runs to its real end, then drops its permits and memory, and its result is discarded and never becomes a `SanitizedRequest` (ADR 0004). The cancellation race (bytes already sent cannot be retracted) is #20's concern.
4. **Traversal and scope.** Every allowed text is visited by `ChatRequest::for_each_text_mut` in a fixed order (messages in order, parts in order, then `stop`, then `user`). Placeholder numbering and the request-wide byte and finding limits live in a request-local `RequestScope`, so numbering restarts at `<SECRET_1>` per request and never leaks between requests. After traversal the job checks that exactly `text_count()` texts were inspected.
5. **Complete success only.** Every core `Err` (limit, finding limit, detector, policy, placeholder), a `Block` or rejected `Warn`, overload, shutdown, and a panicking or discarded job is a hard failure with a fixed safe code and no payload. No partial result and no original-body path exists.
6. **Block findings reject.** Any `Block` finding (for example a private key) rejects the request: `422 unsupported_input` (the code the contract already named provisionally; no new code is introduced).
7. **Warn findings: `content.on_warn`.** The default policy leaves `Warn` findings (medium confidence, for example `password=hunter2xyz`) in the text. The static content-policy setting `on_warn` takes `"reject"` (default) or `"forward"`. `reject` fails closed with `422 unsupported_input`. `forward` sends the text unchanged and is a deliberate operator choice that accepts the false-negative risk of medium-confidence detections; it is documented as such, not as a redaction. Both values are tested at the boundary and over HTTP.
8. **Object keys are not inspected, and why that is safe here.** The Chat Completions matrix admits only fixed schema keys at every depth (`model`, `messages`, `role`, `content`, `type`, `text`, `stream`, `stream_options.include_usage`, six sampling controls, `n`, `seed`, `stop`, `user`, `response_format.type`). Every other key, including every free-form-keyed object (`metadata`, `logit_bias`, tool schemas), is rejected by #18, so no user-controlled key text can reach upstream. Keys are therefore structural. If a later contract admits free-form keys it must reject or inspect them in the same change.
9. **`model` is checked, never rewritten.** `model` is a validated identifier whose character set also admits token-shaped strings. It is passed through the core in detect-only mode: any finding of any action rejects the request (`unsupported_input`). It is never rewritten, since a placeholder is not a valid model name. This closes the structural-string channel for detectable secrets; it is not a claim that arbitrary short values cannot ride in `model`.
10. **Already redacted input.** A client-supplied `<SECRET_n>`-shaped string is ordinary text and is not a finding. A real secret beside it is still redacted, so the literal and the generated placeholder can be equal strings and numbering is not a unique mapping back to values (security is unaffected: nothing trusts or restores from it). No "already scanned" header or body claim is read anywhere (unknown fields are rejected, and no code reads such a header; a source scan test remains).
11. **Fresh serialization.** `ChatRequest::serialize_bounded` writes a new JSON document from the typed request: same keys, types, and array order for messages, parts, and stop; canonical key order within objects (JSON object order is not significant, and the original order is not retained); strings escaped by the JSON writer (non-ASCII is emitted as UTF-8, `\u` escapes are decoded first); numbers keep their parsed literal. Output is produced once into a writer that refuses to exceed its bound. There is no regex or replacement over raw JSON, no whole-body clone between stages beyond ADR 0007, and the original buffer was already released after parsing.
12. **Transformed-output bound.** The output may not exceed `min(resources.limits.max_body_bytes, bytes covered by the request's memory reservation)`; the reservation already budgets one output copy (`BUFFER_COPIES`). Over the bound is `413 limit_exceeded`, never truncation. `boundary::approve` re-checks non-empty and within-reservation, and it now compares bytes to `units * 1 KiB` (it previously compared bytes to units).
13. **Request-wide limits.** Total decoded text bytes are bounded by `max_body_bytes`; findings by `content.max_findings` (default 1024, ceiling 50,000, provisional; ADR 0008). Per-call core limits are applied by the core, and the scope sums them across texts. Exceeding either rejects the request.
14. **Sealed type.** Only `boundary::approve` constructs `SanitizedRequest`, and only from a `ValidatedRequest`, a `CompleteInspection` (minted only by `RequestScope::finish`), and the `RouteId` bound at admission. `CompleteInspection` has private fields and no public constructor (compile-fail test).

## Owner

`boundary` (orchestration, approval, serialization bound), `core_bridge` (core semantics, pool), `protocol::chat` (traversal, serialization), `config` (content policy), `server` (startup initialization).

## Invariants

1. A `SanitizedRequest` exists only after every allowed text returned `Ok` from the core, the visited text count matches, serialization fit its bound, and approval ran.
2. A core or boundary failure sends zero upstream bytes; there is no transport call in the inspection path.
3. The job, not the HTTP future, owns the inspection permit and memory reservation until the core call really ends.
4. Placeholder numbering and limit counters are request-local.
5. No error, `Debug`, or response carries request text, findings, or core messages.

## Failure behavior

| Outcome | Status | Code |
| --- | --- | --- |
| `Block` finding; `Warn` finding with `on_warn = reject`; any finding in `model` | 422 | `unsupported_input` |
| Request-wide byte or finding limit, output over its bound | 413 | `limit_exceeded` |
| Detector, policy, or placeholder failure; discarded or panicked job | 500 | `incomplete_inspection` |
| No inspection permit, queue full, pool closing | 503 | `overload` (`Retry-After: 1`) |

## Verification

`tests/inspection_transform.rs` (real pool and registries, synthetic secrets in every field, escapes, Korean, structure, `Block`, `Warn` both ways, limits, output bound, request isolation, cancellation and repeated-cancellation capacity), `tests/inspection_route.rs` (HTTP, fake upstream asserted empty, readiness depends on core init), `tests/core_probe_scheduling.rs` (`submit_job` ownership), unit tests in `core_bridge.rs`, `protocol/chat.rs` (round trip, bound), `config.rs`, and the compile-fail tests in `tests/ui/`.

## Deferred measured choices

Worker count and queue sizing, `max_findings`, shutdown deadline for the pool (the pool currently stops accepting and drops queued work when the process stops; a bounded drain at shutdown is not wired), inline fast path for tiny inputs, and `spawn_blocking` comparison (ADR 0008).

## Consequences and limits

`on_warn = forward` leaves medium-confidence findings in forwarded text. The `common` profile has no GitHub token detector; choosing it is an operator decision (a synthetic `ghp_` token passes `common`). Detection coverage is the core's; nothing here claims all secrets are found. Placeholders are not restored. Canonical key order differs from the caller's.
