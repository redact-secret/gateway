# Contract: configuration schema v1 (frozen)

Status: **frozen for the Beta 1 line** (#62, [ADR 0032](../decisions/0032-configuration-schema-freeze.md), epic #12). Frozen against main revision `bde57af98a64c9fe3bb55e2fa50d3572f554b82f` (after #84, PR #100) plus the #63 change that adds `deployment.local_auth` (additive to version 1 while unreleased, see [Local caller authentication](#local-caller-authentication-deploymentlocal_auth-63)): the shape below is what the loader accepts at that revision plus that change and nothing else. The schema file is `docs/schema/gateway-config.v1.schema.json` (JSON Schema 2020-12). The Rust loader (`src/config.rs`) remains authoritative; the schema is its machine-readable description and is checked against it on every `cargo test`.

Remaining provisional (not frozen as tunings): every capacity value (no defaults, unmeasured), every `resources.limits.*` default and ceiling, and the `content.max_findings` default and ceiling. They are finite and validated, but ADR 0008 still owes quiet-host measurement; changing them is a release-note item, not a version change (see [compatibility](#compatibility-and-version-strategy)).

## What is frozen

| Part | Frozen content |
| --- | --- |
| Version | `schema_version` is the integer `1`. This build accepts only `1`; any other number is `unsupported_schema_version`, checked before every other field. |
| Objects | Ten closed objects: root, `deployment`, `deployment.listener`, `deployment.upstream`, `deployment.local_auth`, `deployment.local_auth.token`, `content`, `resources`, `resources.capacity`, `resources.limits`. Unknown keys are rejected at every depth. |
| Authorities | `deployment` (listener, provider profile), `content` (core profile, PII, `on_warn`, `max_findings`), `resources` (capacity, limits, deadlines) stay separate. Content and resource values cannot change an address, provider, route, TLS rule, or credential rule (tested). |
| Routes | No configuration key names a route, origin, URL, port, TLS setting, or credential. `deployment.upstream.provider: "openai"` selects a reviewed profile whose fixed routes are `openai.chat_completions` (`POST /v1/chat/completions`, implemented) and `openai.responses` (`POST /v1/responses`, **planned**, #86, [responses-request](responses-request.md), [ADR 0031](../decisions/0031-responses-stateless-text-contract.md)). Adding the Responses route adds no key; the Responses item count reuses `resources.limits.max_messages`. Both are listed under `x-gateway.routes` in the schema with their status, and a test pins the `implemented` set to the real route table. |
| Credentials | Provider keys and the local token never appear in configuration. The only reference form is `deployment.local_auth.token` (below): a name or path, never a value. |
| Activation | Static, read once at startup, restart-activated. No hot reload, no per-request configuration, no request-supplied policy or routing. |

The key tables, defaults, and ceilings are in the schema and in [configuration](../configuration.md); the numeric ceilings and provisional defaults for `resources.limits` are the table in [resource-limits](resource-limits.md), and a test compares the two documents row by row.

## Validation stages

A file is rejected in the first stage it fails. Diagnostics are `invalid_config: <kind> at <static location>` and never carry values, key names, or paths.

| Stage | Covers | Kinds | In the JSON Schema |
| --- | --- | --- | --- |
| `syntax` | size (64 KiB), UTF-8, JSON syntax, duplicate keys, trailing bytes | `unreadable`, `too_large`, `malformed` | No: JSON Schema cannot see duplicate keys or file size |
| `version` | `schema_version` present, a number, equal to 1 | `missing_field`, `invalid_type`, `unsupported_schema_version` | Yes (`const: 1`) |
| `structure` | unknown, missing and mistyped fields; enum members; integer ranges; array bounds | `unknown_field`, `missing_field`, `invalid_type`, `invalid_value` | Yes |
| `semantic` | listener address syntax and loopback/acknowledgement agreement; `allow_non_loopback` requires `local_auth.mode` `token`; `local_auth` mode and token agreement; token source resolution (startup, below); core profile and PII selectors; `stream_idle_ms <= stream_lifetime_ms`; `upstream_header_ms <= upstream_total_ms`; `capacity.stream * stream_buffer_bytes <= 4 GiB` | `invalid_value`, `invalid_combination` | No: the schema accepts these files and the loader rejects them (listed under `x-gateway.loader_only_constraints`) |

All stages finish before the listener is bound, before readiness, and before any traffic. `validate-config` runs exactly the same code. `tests/fixtures/config/invalid/index.json` records the stage, kind and location of each synthetic invalid fixture, and `tests/config_schema.rs` asserts the loader's diagnostic, that the schema rejects `version` and `structure` fixtures, and that it accepts `semantic` ones.

Unsupported providers (`anthropic`, wrong case, a route id used as a provider), unknown route or authority keys (`deployment.routes`, `deployment.upstream.url`, `.route`, `.api_key`, a `proxy` or `tls` key, an authority key under `content`) fail in `structure` before readiness.

## Drift protection

`cargo test` fails if any of these diverge:

1. Every shipped example (`examples/config.*.json`, `container/config.container.json`) and every `tests/fixtures/config/valid/*.json` loads into the real `RuntimePlan` and validates against the schema.
2. The schema's property set for each object equals the key list the loader passes to `only(...)` (the test reads `src/config.rs`).
3. Every `resources.limits` default equals `RequestLimits::provisional()` (an exhaustive destructure, so a new limit is a compile error here); every minimum, maximum, and capacity or finding bound is exercised at, and one past, the boundary through the loader.
4. The ceilings and provisional values in `resource-limits.md` equal the schema.
5. The schema's `implemented` routes equal the reviewed route table.
6. The schema uses only keywords the test validator implements, and closes every object.

## Local caller authentication: `deployment.local_auth` (#63)

Implemented in #63 as an additive, optional object inside schema version 1 ([local-caller-auth](local-caller-auth.md), [ADR 0030](../decisions/0030-local-caller-auth-listener-health.md)). Absent means `mode: "disabled"`, the Alpha behavior, which is supported only on a loopback listener, so every config valid before still loads and means the same, with the one tightening below.

| Key | Shape | Stage |
| --- | --- | --- |
| `deployment.local_auth` | optional object, closed (`mode`, `token`) | `structure` |
| `deployment.local_auth.mode` | required in the object: `"disabled"` or `"token"` (exact case) | `structure` (enum); `disabled` forbids `token`, `token` requires it: `semantic` (`invalid_combination` / `missing_field` at `deployment.local_auth.token`) |
| `deployment.local_auth.token` | closed object with exactly one of `env` or `file` (both or neither: `invalid_combination`); a reference, never a value | `structure` (`minProperties`/`maxProperties` 1, `additionalProperties: false`) |
| `token.env` | `^[A-Z_][A-Z0-9_]{0,63}$` | `structure` (`pattern`) |
| `token.file` | absolute path (`^/`, at most 4096 bytes) | `structure` (`pattern`) |

The token source is resolved once, during validation and before the listener is bound (`validate-config` does the same): the variable must be set and non-empty, or the file must be a regular file of at most 256 bytes that is not readable, writable or executable by "other", and the token (one trailing `\n` or `\r\n` removed from a file) must be 32 to 128 bytes of `A-Za-z0-9-._~`. Failures are `invalid_config: unreadable|invalid_value at deployment.local_auth.token` with a static location; neither the variable name, the path, nor any content is echoed. Resolution is restart-activated like every other key.

- The schema carries the whole shape the JSON Schema vocabulary can express; the test validator now implements `pattern`, `minProperties` and `maxProperties`, and still fails on any other keyword. Mode/token agreement, the non-loopback rule and source resolution are loader-only constraints (`x-gateway.loader_only_constraints`).
- `tests/fixtures/config/valid/local-auth-*.json` and `tests/fixtures/config/invalid/local-auth-*.json` hold the synthetic fixtures (references only: placeholder env names and absolute paths). Because a committed file cannot carry a secret or the required mode, the schema tests load fixtures through `config::load_from_path_with` with a synthetic token resolver; `tests/local_auth_startup.rs` exercises the real resolution with the real binary.
- Only `deployment.local_auth` may name or influence authentication: the schema defines no `local_auth` or `token` key under `content` or `resources`, and a test varies profile, `on_warn`, PII, findings and limits to show `deployment.local_auth` is unchanged.
- The tightening frozen with the design is in force: a non-loopback address with `allow_non_loopback: true` and no token (absent or `disabled`) is `invalid_combination` at `deployment.local_auth.mode`. `container/config.container.json` was updated in the same change to enforce a token from `/run/secrets/gateway-local-token`, so the container shape needs that mount (see [artifacts](../artifacts.md)). No release has shipped, so no deployed v1 config is invalidated.

## Compatibility and version strategy

The rules apply from this freeze on. Nothing is silently reinterpreted: an unrecognized key or version is a startup error, never ignored.

| Change | Rule |
| --- | --- |
| New optional key, new optional object, new `provider` profile, new route under an existing provider | Additive: stays `schema_version` 1 only if absence keeps today's behavior and defaults, and a config valid before stays valid and means the same. Needs a schema edit, loader edit, fixtures, ADR amendment, and a new `base_revision`. `local_auth` (#63, done) and any Beta 2 probe or listener key (#89, #90) must meet this, otherwise they require version 2. |
| Remove or rename a key; change a key's meaning, type, or absent-value behavior; make an optional key required; change how a value selects a destination or authority | Breaking: `schema_version` 2. A build never accepts a version it does not implement and never reads a version-1 file as version 2. |
| Raise a ceiling, loosen a validation | Additive, release note. |
| Lower a ceiling or change a provisional default or capacity guidance | Allowed only while the value is listed under `x-gateway.freeze.provisional`; release note, and the schema, `resource-limits.md`, and the code change in one PR. |
| Tighten a combination (as the `local_auth` non-loopback rule above) | Needs an ADR and a release note; it fails closed at startup, never at request time. |

Upgrade and rollback are restart-based. A newer binary accepts every older config that follows the additive rules. An older binary given a newer config rejects the new key as `unknown_field` before readiness, so the rollback failure mode is a refused start with a static diagnostic, not a weakened gateway; rolling back means removing the newer keys and restarting. The upgrade and rollback procedure with worked examples is #64.

## Not in scope

No hot reload, no arbitrary upstream routing, no configuration control plane, no inline provider-key management, no per-request policy. The schema describes configuration only, not request bodies (those are the endpoint contracts).
