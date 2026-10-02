# Configuration and CLI (skeleton)

Status: implemented for the loopback health skeleton (#4). Governing decisions: [ADR 0006](decisions/0006-runtime-plan-and-authority-separation.md), [ADR 0009](decisions/0009-credential-and-upstream-trust-model.md). The upstream provider profile is implemented (#23, [ADR 0013](decisions/0013-fixed-https-destinations-and-outbound-authority.md), [contract](contracts/upstream-destinations.md)); proxy routes, credential rules, and body forwarding are **planned** (#8, #20, #24, #25).

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
| `deployment.upstream.provider` | deployment | Optional object `{"provider": "openai"}`; when present `provider` is required and must be a reviewed profile name (`openai`, exact case). It fixes the HTTPS origin `https://api.openai.com`, port 443, and route `openai.chat_completions` (`POST /v1/chat/completions`). No other key is accepted (no URL, host, port, proxy, TLS, or test field). Absent means no upstream is configured and no route exists. Additive to `schema_version` 1 (unreleased build). |
| `content.profile` | content | Required; a profile name the pinned core accepts (`full`, `common`). Cannot change the listener, origins, TLS, or credential rules. |
| `resources.capacity.{receipt,memory_units,inspection,upstream,stream}` | resource | Each required, integer in `1..=4294967295`. There are no defaults; measured values are owed by ADR 0008. Deadlines other than the body deadline and the remaining limit categories in `docs/contracts/resource-limits.md` are planned. One memory unit is 1 KiB. A request reserves `4*B + min(max_nodes, B)*128` bytes up front (see resource-limits), so a budget below about 6,144 units (6 MiB) clamps the largest accepted body below `max_body_bytes`, and the skeleton value `1` accepts no real request. |
| `resources.limits.{max_body_bytes, max_depth, max_nodes, max_string_bytes, max_messages, admission_wait_ms, admission_queue, body_deadline_ms}` | resource | Optional object (#18); every field optional and keeps its provisional value when absent. Integers within the ceilings in `docs/contracts/resource-limits.md`. The values there are **provisional pending quiet-host measurement**, not measured or recommended tunings. |

Any key not listed above is rejected at every depth, so credentials, tokens, and URLs cannot be placed in configuration. Invalid values and invalid combinations fail startup before the listener is bound.

## Diagnostics

Failures print `invalid_config: <kind> at <location>`, where `<kind>` is one of `unreadable`, `too_large`, `malformed`, `unsupported_schema_version`, `missing_field`, `unknown_field`, `invalid_type`, `invalid_value`, `invalid_combination` and `<location>` is a static schema path (for an unknown field, its parent object, never the offending key). Values, file content, and paths are never printed.

## HTTP surface

Served on the configured listener only. See [errors and telemetry](contracts/errors-and-telemetry.md).

| Request | Response |
| --- | --- |
| `GET /healthz` | `200 {"status":"live"}` whenever the process serves. |
| `GET /readyz` | `200 {"status":"ready"}` only when the validated plan is held, required initialization completed, and the server is accepting; otherwise `503 {"error":{"code":"not_ready"}}`. Readiness turns false when shutdown begins. |
| `POST /v1/chat/completions` | Admission, bounded receipt, strict parse, and field matrix ([chat-completions-request](contracts/chat-completions-request.md)). Failures are local `4xx`/`503` safe errors; a request that passes everything still ends in `501 {"error":{"code":"not_implemented"}}`. **Nothing is forwarded** until #19/#20. Status mappings: [errors and telemetry](contracts/errors-and-telemetry.md). |
| other methods on that path | `405` with `Allow: POST`, rejected locally. |
| anything else | `404 {"error":{"code":"unsupported_input"}}`, rejected locally. No upstream call, no forwarding, no body read. |

Health endpoints make no upstream calls and use no credentials. Core initialization is a stub until #5; readiness already depends on the explicit initialized flag.
