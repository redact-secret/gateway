# ADR 0001: Repository, ownership, and independent versioning

Status: Accepted (design). Implementation status: planned (no code exists).

## Context

Core issue [redact-secret/redact-secret#1001](https://github.com/redact-secret/redact-secret/issues/1001) holds the architecture origin. Gateway implementation ownership moves to `redact-secret/gateway`. This ADR records who owns what so that gateway code does not absorb detection logic and core does not absorb network concerns.

## Decision

- Gateway depends directly on the pinned Rust core. Core has no reverse dependency on Gateway, adapters, or Vault.
- Gateway owns network parsing, route selection, JSON field classification, transport, deployment, and operational behavior.
- Core owns detection and redaction. Gateway never reimplements detectors, token regexes, scoring, PII logic, or detector registries.
- Gateway is one private binary crate (`redact-secret-gateway`). No internal crate is published as an API without an ADR.
- Gateway versions independently of core. Each release records the exact core version and source identity. No floating git dependency and no automatic following of core betas.
- The first stable release has one application trust domain, no Vault requirement, no shared multi-tenant mode, and no response redaction.
- Complete request buffering, unknown-input rejection, response relay without response redaction, and no Gateway retries are baseline behavior (see ADRs 0002 and 0009 and the request-state contract).

| Repository | Responsibility |
| --- | --- |
| `redact-secret/redact-secret` | Detection and redaction engine |
| `redact-secret/redact-secret-adapters` | In-process host integrations |
| `redact-secret/redact-secret-vault` | Mapping storage and restore authorization |
| `redact-secret/redact-secret-benchmarks` | Measurement and evidence |
| `redact-secret/gateway` | HTTP deployment and protocol boundary |

## Owner

Gateway maintainer (repository owner). Cross-repository contract blockers are filed against core and linked from the blocking gateway issue.

## Invariants

1. `Cargo.lock` is committed. The core dependency is an exact release or immutable source commit with provenance.
2. No detector logic exists in this repository.
3. No Vault or adapters dependency is required to build or run the first stable release.
4. A core change required by Gateway goes through a core issue, not a local workaround.

## Failure behavior

A missing core capability (see the core-completeness contract) is a tracked blocker. Gateway fails closed on affected paths and does not invent a guarantee.

## Implementation handoff

- #2: pin toolchain, dependencies, and core; create the private binary crate and module skeleton from ADR 0005.
- #5: verify required core capabilities against the exact pin.

## Verification

Dependency-graph review shows core has no reverse dependency. A repository review finds no detector regexes or scoring. Release records show the exact core pin.

## Deferred measured choices

None. Transport stack versions are chosen in the #2 dependency ADR.

## Cross-links and open acceptance

Linked to core #1001. This baseline does not satisfy #1001's prototype and threat-model acceptance. That remains outstanding pending #5 evidence and a reviewed threat model.
