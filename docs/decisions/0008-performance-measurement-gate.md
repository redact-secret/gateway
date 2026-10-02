# ADR 0008: Performance measurement gate

Status: Accepted (design); all numeric choices deferred. Implementation status: planned. Covers issue #3 section G.

## Context

Worker strategy, limits, parser optimization, and pool tuning depend on evidence. No performance-superiority, zero-copy, or fast-path claim is approved without it.

## Decision

Define coarse, safe measurements for these phases, recorded separately:

1. admission wait
2. receipt and parsing
3. core inspection and transformation
4. serialization
5. gateway processing (total excluding upstream)
6. upstream first response and stream relay

Benchmark synthetic workloads: no-findings, many-findings, large input, concurrent requests, slow SSE consumer. Record p50, p95, p99, and peak memory, with source commit, core pin, configuration, and platform pins.

Telemetry and reports contain no raw payloads, credentials, snippets, or detector scoring internals.

Measured decisions (worker/pool choice, finite numeric budgets, parser optimization, HTTP pool tuning) each need written criteria and an owner. Baseline performance measurements do not prove boundary correctness.

## Owner

#5 owns the core-side probe and baseline. #6 owns the harness and workloads. The maintainer approves any measured decision.

## Invariants

1. A number enters configuration defaults only with a recorded measurement and pins.
2. Measurements never include payload content.
3. Performance results are not used as correctness evidence.

## Failure behavior

If a measurement cannot be produced, the related choice stays open and the documented bound stays conservative. It is not guessed.

## Implementation handoff

- #5: core setup versus steady-state; no-findings, many-findings, large-input cases.
- #6: concurrency and slow-SSE workloads; coarse timing and peak memory without payloads.
- #18-#25: consume the recorded decisions.

## Verification

Reproducible benchmark commands with recorded pins. Review that outputs contain no payload data.

## Deferred measured choices

All of them. This ADR records only the gate and the owners, never a value.
