# ADR 0032: Configuration schema v1 freeze and machine-readable schema

Status: Accepted (design and implementation), by maintainer delegation to the #62 author under epic #12. Implementation status: **implemented** (schema file, fixtures, and drift tests); the `deployment.local_auth` keys it excludes are **planned** (#63). Date: 2026-10-03. Builds on [ADR 0006](0006-runtime-plan-and-authority-separation.md), [ADR 0008](0008-performance-measurement-gate.md), [ADR 0030](0030-local-caller-auth-listener-health.md), [ADR 0031](0031-responses-stateless-text-contract.md). Contract: [config-schema](../contracts/config-schema.md). Issue: #62.

## Context

The loader has parsed `schema_version` 1 since Alpha 1 and has grown additively (upstream provider, PII, `on_warn`, `max_findings`, limits, connection bound). The shape lived only in `src/config.rs` and prose. Beta 1 adds two things that touch it: the local caller token (#61, planned) and the Responses route (#82, planned). #63 and #86 implement those and consume the frozen schema, so #62 must not wait for them.

## Decisions

| # | Decision | Why |
| --- | --- | --- |
| S1 | One JSON Schema file, `docs/schema/gateway-config.v1.schema.json`, describes exactly what the loader accepts at the recorded base revision. Rust remains authoritative. | A schema that also described unimplemented keys would accept files the loader rejects, or the reverse: two authorities. |
| S2 | `schema_version` stays `1`. The freeze is a documented compatibility regime, not a bump. | No release exists, so no deployed v1 file can be reinterpreted; all changes so far were additive. A bump would invalidate the Alpha configs for no gain. Rules for when v2 is required are in the contract. |
| S3 | Planned keys are not in the schema's accepted shape. `deployment.local_auth` is kept as `$defs.planned_local_auth` with `x-gateway-status: planned`, and the loader keeps rejecting it as `unknown_field`. | Fail closed: a config requesting a token cannot start a gateway that does not enforce one. No dependency cycle: the freeze contains only implemented keys; #63 adds its keys as an optional, additive change inside v1 and moves the definition. |
| S4 | The Responses route adds no configuration key. The `openai` profile's routes (`openai.chat_completions` implemented, `openai.responses` planned) are recorded under `x-gateway.routes`, with the `implemented` set pinned to the real route table. Responses item count reuses `max_messages`. | ADR 0031 R11 forbids a new operator key; routing authority stays in reviewed profiles. |
| S5 | Constraints a schema cannot express (duplicate keys, address syntax, loopback/acknowledgement agreement, core-validated profile and PII, three cross-field limit rules) stay in the loader and are listed in `x-gateway.loader_only_constraints` and the contract. Validation stages are `syntax`, `version`, `structure`, `semantic`. | Honest division of labor. Stage-tagged invalid fixtures prove each stage and that the schema neither over- nor under-claims. |
| S6 | Drift is prevented by tests, not review: examples and fixtures load through the real loader and validate against the schema; schema key sets equal the loader's `only` lists; defaults equal `RequestLimits::provisional()` by exhaustive destructure; boundaries are exercised at and past each end; `resource-limits.md` rows equal the schema. The test validator implements a small keyword set and fails on any other keyword. | No new dependency (a validator crate would enter the normal or dev graph under ADR 0011 for little benefit), and an ignored keyword cannot silently weaken the check. |
| S7 | Capacities and limits remain provisional (ADR 0008); freeze status names them. | The numbers are finite and validated but unmeasured on a quiet host. |
| S8 | Additive changes (new optional key, provider, route) stay in v1 when absence preserves behavior; breaking changes need v2; rollback to an older binary fails closed with `unknown_field`. | Never silently reinterpret configuration or weaken rejection (CONVENTIONS.md). |

## Invariants

1. Unknown fields and versions are rejected before readiness at every depth; diagnostics never echo values, keys, or paths.
2. Deployment, content, and resource authority stay separate; no content or resource key can name or change a listener, provider, route, TLS, or credential rule.
3. No configuration key carries a secret value.
4. The schema never accepts a file the loader rejects at the `version` or `structure` stage, and never rejects one the loader accepts.

## Failure behavior

A file that fails any stage stops startup with `invalid_config` and exit 1; no listener is bound.

## Implementation handoff

#63: add `local_auth` to loader and schema together (procedure in the contract); #64: upgrade and rollback examples over the compatibility table; #65: Node and Python examples of both credentials; #86 flips `openai.responses` to `implemented` in the schema; #89/#90: record probe or listener keys as additive keys or a version bump.

## Verification

`tests/config_schema.rs` (schema checks, stage fixtures, boundaries, drift, planned-key rejection), `tests/shipped_examples.rs` (every shipped config through the real binary), `tests/fixtures/config/`.

## Residual limits

The in-test validator is not a general JSON Schema implementation; external validators may differ on edge keywords, which is why the schema uses only the basic ones. Capacity and limit numbers are provisional.
