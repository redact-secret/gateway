# Contract: configuration upgrade, rollback and restart activation

Status: implemented as documentation, fixtures and tests (#64, [ADR 0033](../decisions/0033-config-upgrade-rollback.md), epic #12). It extends the compatibility rules in [config-schema](config-schema.md) (`docs/contracts/config-schema.md`, [ADR 0032](../decisions/0032-configuration-schema-freeze.md)) and the token rules in [local-caller-auth](local-caller-auth.md). Nothing here is a stable-compatibility, support or production-deployment claim: no release exists, the schema freeze covers the Beta 1 line only, and upgrade and rollback qualification on release artifacts remains Beta 3 work.

The fixtures are under `tests/fixtures/config/migration/` and are loaded by `tests/config_migration.rs` through the real loader and the real binary. They hold references only (placeholder env names and absolute paths); no file contains a credential, and none may.

## Activation is a restart

- The configuration file is read once, at startup. There is no hot reload, no signal that re-reads it, no per-request config read and no control plane. Editing the file, or replacing the token file or environment variable, does nothing to a running process.
- Activation of any change (config, token, binary) is: stop the old process (SIGTERM or SIGINT, graceful, exit 0), then start the new one. The old process stops reporting ready as soon as shutdown begins, then drains in-flight requests within `resources.limits.shutdown_drain_ms`. The listener is not shared between the two processes, so there is a gap in which nothing accepts connections; a single application sees refused connections during it and must retry its own call. The gateway claims no zero-downtime handoff and no seamless rotation.
- Startup order is fixed: parse and validate the file, resolve the token reference, initialize required resources, bind the listener, print `listening <addr>`, serve. `GET /readyz` answers 200 only after all of that. A failure at any step exits 1 with a static `invalid_config` or startup diagnostic, prints nothing on stdout, never binds a listener, never reports ready and never forwards a byte. Orchestrators should gate traffic on `/readyz`, and treat a process that exits 1 on start as a failed activation: the previous binary and config are the recovery path.
- `validate-config <path>` runs the same checks, including token source resolution, without binding. Run it with the same environment and mounts as the real process before swapping.

## Version semantics

- `schema_version` is the integer `1` for the whole Beta 1 line. A build accepts only the versions it implements; any other value (older, newer, string, missing) is `unsupported_schema_version` at `schema_version`, checked before every other field. A build never reads a newer file as an older one.
- Additions inside version 1 are optional keys whose absence keeps the old behavior (`deployment.local_auth`, #63). A newer build accepts every older file that follows these rules, except where a tightening was announced (below).
- An older build given a file with a key it does not know rejects it as `unknown_field` (location is the parent object, never the key) before readiness. That is the intended rollback failure mode: a refused start, never a gateway that ignores a setting.
- The gateway binary version (`--version`, independent of core) and the schema version are separate. A binary version never implies a schema version; the pair table below is by source revision until a release records both.

## Compatibility pairs

"Alpha" means builds before #63 (up to and including main `bde57af`, schema v1 without `deployment.local_auth`). "Beta" means builds from main `d7c74d5` (#63) on, schema v1 with `deployment.local_auth`. There is no published binary of either; the revision is the identity.

| Binary | Config | Result |
| --- | --- | --- |
| Alpha | Alpha-shaped (no `local_auth`) | Starts. No caller token. |
| Alpha | Beta config containing `deployment.local_auth` | Refused: `unknown_field` at `deployment`, before readiness. Auth is never silently dropped. |
| Beta | Alpha loopback config (`alpha/loopback-openai.json`) | Starts unchanged. `local_auth` absent means `disabled`, the Alpha behavior, supported on loopback only. |
| Beta | Alpha non-loopback config with `allow_non_loopback: true` and no token (`alpha/container-non-loopback.json`, identical to `rejected/beta-alpha-non-loopback-no-auth.json`) | Refused: `invalid_combination` at `deployment.local_auth.mode`. This is the one announced tightening. |
| Beta | Beta config | Starts when the token reference resolves; otherwise refused (`unreadable` or `invalid_value` at `deployment.local_auth.token`). |
| any | `schema_version` other than 1 (`rejected/newer-schema-version.json`, `rejected/older-schema-version.json`) | Refused: `unsupported_schema_version` at `schema_version`. |
| any | A provider credential in configuration (`rejected/alpha-with-inline-credential.json`) | Refused: `unknown_field` at `deployment.upstream`. No version accepts a credential value. |
| Beta | A key a later build adds (`rejected/future-key-under-local-auth.json` is the stand-in) | Refused: `unknown_field` at `deployment.local_auth`. |

## Upgrade: Alpha to Beta (synthetic examples)

Each pair is in `tests/fixtures/config/migration/index.json`; both ends are loaded by the real loader.

1. `alpha/loopback-openai.json` to `beta/loopback-openai-disabled-explicit.json`: add `"local_auth": {"mode": "disabled"}`. Optional and behaviorally identical; it records the choice. No application change. Loopback without a token is an address restriction, not authentication.
2. `alpha/loopback-openai.json` to `beta/loopback-openai-token-env.json`: add `local_auth` with `mode: token` and `token.env` naming a variable. Provide the token by the process environment (or a file reference), then restart. The application must send `X-Gateway-Local-Token`; the provider key stays separate. Until the application is updated, its proxy requests receive `401 local_auth_required` (probes are unaffected).
3. `alpha/container-non-loopback.json` to `beta/container-token-file.json` (the shipped container shape): add `local_auth` with `token.file` at `/run/secrets/gateway-local-token` and mount that file (mode and ownership rules in [artifacts](../artifacts.md)). Without the mount the container exits 1 on start.

Procedure: `validate-config` the new file in the target environment; stop the old process; start the new binary with the new file; wait for `/readyz`; send the token from the application. Keep the old binary and file until the new one has served a request.

## Rollback

Rollback is also a restart, and it needs a compatible pair. The previous config and the secret reference are separate things to restore:

- Rolling back the binary only, keeping a Beta config: refused by an Alpha binary (`unknown_field` at `deployment`). This is safe: nothing starts, nothing forwards. It is not a fault to work around by deleting the key without a decision.
- Rolling back to an Alpha binary requires restoring the Alpha config, which has no `local_auth`. That is an explicit removal of authentication. Do it only on a loopback listener with the understanding that the same-host trust boundary is then address-only; a non-loopback deployment cannot be rolled back to Alpha without first moving to loopback. Remove the token header from the application or leave it: an Alpha build does not check it.
- Rolling back the config only, keeping a Beta binary: dropping `local_auth` from a Beta file is accepted on loopback and means `disabled`, and is refused with `invalid_combination` when `allow_non_loopback` is true. The loader makes no distinction between "never had auth" and "auth removed", so treat the edit as a reviewed security change, not routine.
- Restoring a Beta config after a token change means restoring both the config and the token source it references. A config whose reference no longer resolves is refused at start (`unreadable at deployment.local_auth.token`). To roll a token back, put the previous token value back at the same reference, then restart. There is no dual-token overlap, so while the application and gateway disagree, every proxy request is `401 local_auth_invalid`.
- `tests/config_migration.rs` checks that each upgrade target minus `local_auth` equals its Alpha source: the rollback config is exactly the source file.

## Token change (single application)

Changing the token is: write the new value to the source (file or environment), restart the gateway, and update the application to send the new value. The two steps are not atomic and there is no overlap window; expect proxy requests to fail with 401 between them. Order them so the application restarts immediately after the gateway, or accept the brief failure. Rolling back is the reverse. The gateway does not claim seamless rotation; see [local-caller-auth](local-caller-auth.md).

## Release artifact check

Artifacts and docs describe Alpha 1 candidates that are unpublished ([artifacts](../artifacts.md)); no document may call schema v1, the limits or any pair stable, supported or fit for production before the Beta 3 qualification. `tests/config_migration.rs` fails if this contract, the README or the configuration reference lose the links or gain such a phrase.

## Not in scope

No live config control plane, no hot reload, no seamless token rotation, no multi-region rollout and no promise that an older binary understands a newer file.
