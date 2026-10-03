# Contract: configuration schema v1 (frozen)

Status: **frozen for the Beta 1 line** (#62, [ADR 0032](../decisions/0032-configuration-schema-freeze.md), epic #12). Frozen against main revision `f7936fe000b7c6c1a5675d98675c39fae29b01d7` (after #82, PR #97): the shape below is what the loader accepts at that revision plus nothing else. The schema file is `docs/schema/gateway-config.v1.schema.json` (JSON Schema 2020-12). The Rust loader (`src/config.rs`) remains authoritative; the schema is its machine-readable description and is checked against it on every `cargo test`.

Remaining provisional (not frozen as tunings): every capacity value (no defaults, unmeasured), every `resources.limits.*` default and ceiling, and the `content.max_findings` default and ceiling. They are finite and validated, but ADR 0008 still owes quiet-host measurement; changing them is a release-note item, not a version change (see [compatibility](#compatibility-and-version-strategy)).

## What is frozen

| Part | Frozen content |
| --- | --- |
| Version | `schema_version` is the integer `1`. This build accepts only `1`; any other number is `unsupported_schema_version`, checked before every other field. |
| Objects | Eight closed objects: root, `deployment`, `deployment.listener`, `deployment.upstream`, `content`, `resources`, `resources.capacity`, `resources.limits`. Unknown keys are rejected at every depth. |
| Authorities | `deployment` (listener, provider profile), `content` (core profile, PII, `on_warn`, `max_findings`), `resources` (capacity, limits, deadlines) stay separate. Content and resource values cannot change an address, provider, route, TLS rule, or credential rule (tested). |
| Routes | No configuration key names a route, origin, URL, port, TLS setting, or credential. `deployment.upstream.provider: "openai"` selects a reviewed profile whose fixed routes are `openai.chat_completions` (`POST /v1/chat/completions`, implemented) and `openai.responses` (`POST /v1/responses`, **planned**, #86, [responses-request](responses-request.md), [ADR 0031](../decisions/0031-responses-stateless-text-contract.md)). Adding the Responses route adds no key; the Responses item count reuses `resources.limits.max_messages`. Both are listed under `x-gateway.routes` in the schema with their status, and a test pins the `implemented` set to the real route table. |
| Credentials | Provider keys and the local token never appear in configuration. The only planned reference form is `deployment.local_auth.token` (below). |
| Activation | Static, read once at startup, restart-activated. No hot reload, no per-request configuration, no request-supplied policy or routing. |

The key tables, defaults, and ceilings are in the schema and in [configuration](../configuration.md); the numeric ceilings and provisional defaults for `resources.limits` are the table in [resource-limits](resource-limits.md), and a test compares the two documents row by row.

## Validation stages

A file is rejected in the first stage it fails. Diagnostics are `invalid_config: <kind> at <static location>` and never carry values, key names, or paths.

| Stage | Covers | Kinds | In the JSON Schema |
| --- | --- | --- | --- |
| `syntax` | size (64 KiB), UTF-8, JSON syntax, duplicate keys, trailing bytes | `unreadable`, `too_large`, `malformed` | No: JSON Schema cannot see duplicate keys or file size |
| `version` | `schema_version` present, a number, equal to 1 | `missing_field`, `invalid_type`, `unsupported_schema_version` | Yes (`const: 1`) |
| `structure` | unknown, missing and mistyped fields; enum members; integer ranges; array bounds | `unknown_field`, `missing_field`, `invalid_type`, `invalid_value` | Yes |
| `semantic` | listener address syntax and loopback/acknowledgement agreement; core profile and PII selectors; `stream_idle_ms <= stream_lifetime_ms`; `upstream_header_ms <= upstream_total_ms`; `capacity.stream * stream_buffer_bytes <= 4 GiB` | `invalid_value`, `invalid_combination` | No: the schema accepts these files and the loader rejects them (listed under `x-gateway.loader_only_constraints`) |

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

## Planned keys: `deployment.local_auth` (#63)

`deployment.local_auth` is a frozen design ([local-caller-auth](local-caller-auth.md), [ADR 0030](../decisions/0030-local-caller-auth-listener-health.md)) and is **not implemented**. Until #63 lands, neither the loader nor the schema accepts it:

- The schema does not list it under `deployment.properties`; its shape is kept as `$defs.planned_local_auth` (`x-gateway-status: planned`) and indexed under `x-gateway.planned`, so a reader can see the intended form without any validator treating it as accepted.
- The loader rejects every `local_auth` object, valid-looking or not, as `unknown_field` at `deployment`. This is the existing fail-closed behavior and is deliberately not weakened: a config that asks for a token cannot start a gateway that does not enforce one. `tests/fixtures/config/planned/` holds synthetic valid and invalid `local_auth` files (only references, a placeholder env name, an absolute path; never a value), and the test asserts each is currently rejected by both the loader and the schema.
- There is no dependency from #62 on #63: the freeze contains no unimplemented key. #63 is an additive, optional-key change inside schema version 1 (absent means `disabled`, the Alpha behavior), and it must in one PR: add `local_auth` to the loader and its `only` list; move the definition into `deployment.properties` (the test validator then needs `pattern`, `minProperties` and `maxProperties`, and `assert_supported` will fail until it implements them); move each `planned/` fixture into `valid/` or `invalid/` with the kind and location from the contract; add the `invalid_combination` fixture for `allow_non_loopback: true` with `mode` absent or `disabled`; update `base_revision`; and amend [ADR 0032](../decisions/0032-configuration-schema-freeze.md). The `schema_key_sets_equal_the_loaders_accepted_key_lists` and `planned_local_auth_is_rejected_everywhere_until_it_is_implemented` tests fail until it does, which is the intended forcing function.
- One tightening is already frozen in the contract and is not a silent change: a non-loopback address with `allow_non_loopback: true` and no token (absent or `disabled`) becomes `invalid_combination` when #63 lands. This is the single semantic change planned inside v1, and it affects only the unreleased container-style config in `container/config.container.json`, which #63 must update in the same PR. No release has shipped, so no deployed v1 config is invalidated.

## Compatibility and version strategy

The rules apply from this freeze on. Nothing is silently reinterpreted: an unrecognized key or version is a startup error, never ignored.

| Change | Rule |
| --- | --- |
| New optional key, new optional object, new `provider` profile, new route under an existing provider | Additive: stays `schema_version` 1 only if absence keeps today's behavior and defaults, and a config valid before stays valid and means the same. Needs a schema edit, loader edit, fixtures, ADR amendment, and a new `base_revision`. `local_auth` (#63) and any Beta 2 probe or listener key (#89, #90) must meet this, otherwise they require version 2. |
| Remove or rename a key; change a key's meaning, type, or absent-value behavior; make an optional key required; change how a value selects a destination or authority | Breaking: `schema_version` 2. A build never accepts a version it does not implement and never reads a version-1 file as version 2. |
| Raise a ceiling, loosen a validation | Additive, release note. |
| Lower a ceiling or change a provisional default or capacity guidance | Allowed only while the value is listed under `x-gateway.freeze.provisional`; release note, and the schema, `resource-limits.md`, and the code change in one PR. |
| Tighten a combination (as in the `local_auth` note above) | Needs an ADR and a release note; it fails closed at startup, never at request time. |

Upgrade and rollback are restart-based. A newer binary accepts every older config that follows the additive rules. An older binary given a newer config rejects the new key as `unknown_field` before readiness, so the rollback failure mode is a refused start with a static diagnostic, not a weakened gateway; rolling back means removing the newer keys and restarting. The upgrade and rollback procedure with worked examples is #64.

## Not in scope

No hot reload, no arbitrary upstream routing, no configuration control plane, no inline provider-key management, no per-request policy. The schema describes configuration only, not request bodies (those are the endpoint contracts).
