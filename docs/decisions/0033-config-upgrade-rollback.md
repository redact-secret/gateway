# ADR 0033: Configuration upgrade, rollback and restart activation

Status: Accepted (design and implementation), by maintainer delegation to the #64 author under epic #12. Implementation status: **implemented** (contract, fixtures, tests). Date: 2026-10-03. Builds on [ADR 0030](0030-local-caller-auth-listener-health.md) and [ADR 0032](0032-configuration-schema-freeze.md). Contract: [config-upgrade-rollback](../contracts/config-upgrade-rollback.md). Issue: #64.

## Context

ADR 0032 froze schema v1 and stated that upgrade and rollback are restart-based, leaving worked examples and tests to #64. #63 added `deployment.local_auth` and one announced tightening (non-loopback needs a token), so an Alpha file can now be refused and a Beta file cannot run on an Alpha build.

## Decisions

| # | Decision | Why |
| --- | --- | --- |
| U1 | Every change is activated by stopping and starting the process. No hot reload, signal reload or control plane. | ADR 0006 immutable plan; a running gateway never changes authority. |
| U2 | `schema_version` stays 1. Compatibility pairs are named by source revision (Alpha: up to `bde57af`; Beta: `d7c74d5` on) until a release records binary, core and schema together. | No release exists; inventing version numbers would be a stability claim. |
| U3 | Upgrade fixtures live under `tests/fixtures/config/migration/` with an index naming each pair, whether it adds auth, and each rejected combination with its kind and location. Tests load both ends through the real loader and run the real binary. | Examples that the loader does not accept would be documentation drift. |
| U4 | An older binary rejects newer keys (`unknown_field`), and any non-1 version is `unsupported_schema_version`; neither is worked around. Rollback to a build without `local_auth` requires restoring a config without it, an explicit and documented removal of authentication that is allowed only on loopback. | Fail closed; auth is never dropped by a binary ignoring a key. |
| U5 | A failed start prints nothing on stdout, binds nothing, is never ready and forwards nothing; `validate-config` is the pre-flight. | Readiness must mean validated and serving. |
| U6 | Token change is a restart of the gateway plus an application update, with no overlap and no seamless rotation claim. | Single-application scope; rotation is a non-goal of the epic. |

## Invariants

1. A config valid only for a newer build is never partly applied by an older one.
2. The loader cannot tell "auth never configured" from "auth removed"; removal is a reviewed operator edit.
3. No fixture or document holds a credential.

## Verification

`tests/config_migration.rs`, plus the existing `tests/config_schema.rs`, `tests/local_auth_startup.rs` and `tests/shipped_examples.rs`.

## Residual limits

An Alpha binary is not built or run by this repository's tests; the Alpha-side results in the pair table follow from the pre-#63 loader's closed key lists and are verified here only through the equivalent unknown-key rejection. Brief downtime during restart is not mitigated.
