# Configuration and CLI (skeleton)

Status: implemented for the loopback health skeleton (#4). Governing decisions: [ADR 0006](decisions/0006-runtime-plan-and-authority-separation.md), [ADR 0009](decisions/0009-credential-and-upstream-trust-model.md). Proxy routes, upstream origins, TLS, and credential rules are **planned** (#8, #23-#25) and are not part of this schema yet.

## CLI

| Command | Behavior |
| --- | --- |
| `redact-secret-gateway --version` (`-V`) | Print the gateway and pinned core versions. |
| `redact-secret-gateway --help` (`-h`) | Print usage. |
| `redact-secret-gateway validate-config <path>` | Parse and validate once; print `config valid (schema_version 1)` and exit 0, or print a safe diagnostic on stderr and exit 1. Binds nothing. |
| `redact-secret-gateway serve <path>` | Validate, initialize, bind the configured listener, print `listening <addr>` on stdout, serve health endpoints, and shut down gracefully on SIGINT or SIGTERM (exit 0, prints `shutdown complete`). |

Exit codes: 0 success, 1 validation/startup/runtime failure, 2 usage error. Arguments, paths, and file content are never echoed. There are no environment-variable settings, and no proxy settings are inherited.

Configuration is static: it is read once at startup and restart-activated. There is no hot reload, no per-request config read, and no request-time policy selection.

## Skeleton configuration

`examples/config.skeleton.json` is a **skeleton**: its numbers are minimal placeholders that make the file valid. They are not recommended or measured values (ADR 0008) and contain no credentials. Format: one strict JSON document (duplicate keys, trailing bytes, and invalid UTF-8 are rejected; maximum file size 64 KiB).

```json
{
  "schema_version": 1,
  "deployment": { "listener": { "address": "127.0.0.1:8787" } },
  "content": { "profile": "common" },
  "resources": {
    "capacity": { "receipt": 1, "memory_units": 1, "inspection": 1, "upstream": 1, "stream": 1 }
  }
}
```

| Field | Authority | Rules |
| --- | --- | --- |
| `schema_version` | n/a | Required; must equal `1`. Checked before anything else. |
| `deployment.listener.address` | deployment | Required IP-literal socket address (no hostnames). Port 0 asks the OS for a free port. |
| `deployment.listener.allow_non_loopback` | deployment | Optional boolean, default `false`. A non-loopback address is rejected unless this is `true`. Setting it `true` on a loopback address is also rejected. Non-loopback exposure is **not a supported deployment** (ADR 0009); the flag only acknowledges that. |
| `content.profile` | content | Required; a profile name the pinned core accepts (`full`, `common`). Cannot change the listener, origins, TLS, or credential rules. |
| `resources.capacity.{receipt,memory_units,inspection,upstream,stream}` | resource | Each required, integer in `1..=4294967295`. There are no defaults; measured values are owed by ADR 0008. Deadlines and the remaining limit categories in `docs/contracts/resource-limits.md` are planned. |

Any key not listed above is rejected at every depth, so credentials, tokens, and URLs cannot be placed in configuration. Invalid values and invalid combinations fail startup before the listener is bound.

## Diagnostics

Failures print `invalid_config: <kind> at <location>`, where `<kind>` is one of `unreadable`, `too_large`, `malformed`, `unsupported_schema_version`, `missing_field`, `unknown_field`, `invalid_type`, `invalid_value`, `invalid_combination` and `<location>` is a static schema path (for an unknown field, its parent object, never the offending key). Values, file content, and paths are never printed.

## HTTP surface

Served on the configured listener only. See [errors and telemetry](contracts/errors-and-telemetry.md).

| Request | Response |
| --- | --- |
| `GET /healthz` | `200 {"status":"live"}` whenever the process serves. |
| `GET /readyz` | `200 {"status":"ready"}` only when the validated plan is held, required initialization completed, and the server is accepting; otherwise `503 {"error":{"code":"not_ready"}}`. Readiness turns false when shutdown begins. |
| anything else (including `POST /v1/chat/completions`) | `404 {"error":{"code":"unsupported_input"}}`, rejected locally. No upstream call, no forwarding, no body read. |

Health endpoints make no upstream calls and use no credentials. Core initialization is a stub until #5; readiness already depends on the explicit initialized flag.
