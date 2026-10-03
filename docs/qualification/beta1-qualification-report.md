# Beta 1 qualification report: frozen configuration and caller/provider credential separation

Status: evidence for issue #65 (epic #12, Beta 1), **extendable by #88**. Everything here is synthetic: invented prompts, revoked-looking tokens, a scripted fake provider on loopback, no provider key, no network beyond 127.0.0.1. The candidate is unpublished and this report is not a release statement. Related: [Alpha 1 report](alpha1-qualification-report.md), [Alpha 2 report](alpha2-qualification-report.md), [local caller auth](../contracts/local-caller-auth.md), [upgrade and rollback](../contracts/config-upgrade-rollback.md), [config schema](../contracts/config-schema.md), [ADR 0020](../decisions/0020-sdk-qualification-test-build.md), [ADR 0030](../decisions/0030-local-caller-auth-listener-health.md), [ADR 0033](../decisions/0033-config-upgrade-rollback.md).

How #88 extends this file: add `POST /v1/responses` as a second column in the endpoint matrix (section 2), add its rows to the evidence tables (section 4) and its entries to the pins (section 1) and the release gates (section 8). Every section below is written per endpoint; "Responses" is marked not qualified, and nothing here may be read as qualifying it.

## 1. Pins

| Item | Value |
| --- | --- |
| Gateway source | base `e0538dcdac5252ea4c659b9b133191d9bc16b222` (`main` when this work started) plus the #65 change; the merge commit is recorded on the pull request and the issue |
| Core | `redact-secret =0.1.0-beta.12` (direct dependency, `Cargo.lock` checksum `2cc951e8b991e9ec27343a872627f53a25b252cc8148cb4ec7160192ed9d856d`), unchanged by this issue |
| Node.js SDK | `openai` 7.27.0 (lockfile integrity-pinned), `zod` 4.6.5 |
| Python SDK | `openai` 3.24.0 (sha256-locked requirements) |
| Runtimes in CI | Node.js 24 (suites `node,python,examples`) and 22.16.0 (suite `node`); Python 3.13. Local reproduction for this report: Node.js 22.16.0, Python 3.14.7 (the suites are runtime independent; CI is the record) |
| Qualification build | separate generated crate from the shipped `src/` plus `qualification/seam.patch` (ADR 0020), `qualification/build.sh`; its `[dependencies]` and lock are the shipped ones byte for byte |
| Exact-binary smoke | `scripts/smoke-binary.sh`, which now ends with `scripts/smoke-local-auth.sh`, run in the `Candidate artifacts` workflow against the built candidate bytes |
| Config schema | `schema_version` 1, [`docs/schema/gateway-config.v1.schema.json`](../schema/gateway-config.v1.schema.json) |

## 2. Supported deployment and endpoint subsets (exactly what is qualified)

| Dimension | Qualified | Not qualified |
| --- | --- | --- |
| Endpoint | `POST /v1/chat/completions` (Alpha 2 subset) with local authentication | `POST /v1/responses`: routed with its fixed destination behind the same local-authentication boundary (#86), but its relay lifecycle is not built (#87) and nothing about it is qualified here (#88); the authentication tests in this report do not exercise it |
| Listener | loopback with `local_auth` absent, `disabled` or `token`; non-loopback with `allow_non_loopback: true` **and** `token` (the container and same-Pod shape, started and probed on the exact candidate, not a supported shared or remote deployment) | any shared, remote, multi-tenant or internet-facing gateway; inbound TLS |
| Token delivery | `file` (mode without "other" bits) and `env`, both through the real loader, resolved once at startup | inline value, CLI argument, hot reload, rotation without a restart, overlap of two tokens |
| Content policy | `full` (`on_warn` reject) and `common` (`on_warn` forward) instances, both authenticated | every other profile and PII selection combination (the Alpha 2 report covers their policy outcomes without authentication; local authentication is independent of them by construction and is checked here on two) |
| Providers | fixed `openai` route to a fake upstream (test build) | any real provider |
| Probes | `GET /healthz`, `GET /readyz` without the token | any admin, metrics or debug route (none exists) |

## 3. Evidence layers

| Layer | What it proves | Where |
| --- | --- | --- |
| Contract and unit tests | Token syntax and comparison, ordering, startup failure kinds, schema fixtures, migration pairs, docs agree with fixtures | `src/transport/local_auth.rs`, `src/transport/tests/local_auth_tests.rs`, `tests/local_auth_startup.rs`, `tests/config_schema.rs`, `tests/config_migration.rs`, `tests/shipped_examples.rs` |
| Fake-upstream test build, pinned SDK runs (new) | Node and Python SDK requests, raw malformed headers and bounded unauthenticated load against two authenticated gateway instances, with the provider's own record as the oracle | `qualification/sdk/node/test/local-auth.test.ts`, `qualification/sdk/python/tests/test_local_auth.py`, instances `authfile` and `authenv` of `qualification/run-suites.sh` |
| README examples, dual credential (new) | `examples/node` and `examples/python` run exactly as documented against the authenticated instance | `qualification/run-examples.sh` |
| Exact candidate binary smoke (new) | The shipped bytes validate the examples and shipped configs, start, serve readiness, enforce authentication ordering, change the token only at a restart, roll back, and refuse unsupported combinations before readiness. **No accepted request reaches a provider** (the binary can only reach the real HTTPS provider, and the script never sends a provider `Authorization`) | `scripts/smoke-local-auth.sh` via `scripts/smoke-binary.sh` |
| Real provider | none | none |

## 4. Results (Chat Completions)

Each row is executed for **both** SDKs and **both** authenticated instances (`authfile`: token file, `full` profile; `authenv`: token environment variable, `common` profile with `on_warn` forward). 41 Node tests and 28 Python tests, plus the example runs.

| Acceptance item | Evidence | Result |
| --- | --- | --- |
| Caller and provider secrets cannot substitute for each other | SDK with both credentials: `200`. Provider key sent as the local token: `401 local_auth_invalid`. Local token sent only as the provider key: `401 local_auth_required` (`Authorization` is never read for local authentication). Valid local token with no provider `Authorization`: `401 missing_credential`. A different well-formed token: `401 local_auth_invalid` | Met |
| Fake upstream never receives local auth | The fake provider flags any request in which either synthetic local token (valid or decoy) appears in a header value, the target or the body (`local_token_seen`), and lists the header names it received. Accepted calls (JSON, streamed, lowercase header name) show `local_token_seen: false`, no `x-gateway-local-token` header, and the provider key's `Authorization` hash unchanged | Met |
| Rejection sends no upstream body | Every rejection (no token, wrong token, provider key as token, 9 malformed shapes, `Connection`-nominated removal, missing provider credential) leaves the provider with **zero connections and zero requests**, including when the body carries a planted secret and a private-key marker (authentication precedes policy) | Met |
| Malformed headers | duplicate (both valid), `Bearer` prefix, comma list, embedded space, empty, 31 bytes, 129 bytes, quoted, characters outside the alphabet: `401 local_auth_invalid`, byte-identical fixed body `{"error":{"code":"local_auth_invalid"}}` | Met |
| Bounded unauthenticated load | 240 raw requests per instance and SDK in waves of 40, half without a token and half with a wrong one, each announcing a 4 MiB body that is never sent: all `401` with the two fixed bodies, provider untouched, and an authenticated request afterwards succeeds. This is a smoke of the pre-body ordering, **not a throughput or denial-of-service result** | Met, bounded |
| Logs and errors expose no secret values | SDK-visible errors (message, body, headers) scanned for both tokens, the provider key, planted secrets and prompt markers; the captured stdout and stderr of all ten gateway instances scanned for the same markers (`log_forbidden` now includes both local tokens); the evidence JSON the tests write is scanned too | Met (the gateway emits no request logs) |
| Health and readiness | `200` with no token and with a wrong one, no upstream contact | Met |
| Policy and profile variations | Authentication outcomes are identical under `full`/reject and `common`/forward; the authenticated `common` instance forwards a GitHub-style token unchanged (the Alpha 2 observation), the `full` one redacts it | Met on two profiles |
| Examples validate against the schema and the compiled loader | `scripts/smoke-local-auth.sh` runs `validate-config` on every `examples/config.*.json` and on generated Alpha, Beta, environment-token and rollback configs with the exact binary; `tests/shipped_examples.rs` and `tests/config_schema.rs` validate the shipped and schema fixtures in CI | Met |
| Dual-credential examples | `examples/node` and `examples/python`: both credentials succeed; provider key only, provider key as token and token as provider key are refused locally with the safe code, print neither credential, and leave the provider untouched | Met |
| Migration, rollback, readiness reproducible | See section 5 | Met on the exact binary |

## 5. Configuration lifecycle on the exact binary

`sh scripts/smoke-local-auth.sh <binary> <dir>` (also run by `scripts/smoke-binary.sh`) walks, with `401 missing_credential` as the "authentication passed, stopped before the provider" oracle:

1. Alpha-shaped config (no `local_auth`): starts; `/healthz` and `/readyz` are `200`; a request needs no local header.
2. Upgrade by adding a `token.file` reference and restarting: no header `401 local_auth_required`; wrong or malformed `401 local_auth_invalid`; the right token passes authentication; the local token in `Authorization` does not.
3. Restart activation: after the token file is rewritten, the running gateway still accepts the old token and refuses the new one; after the restart the reverse holds. There is no hot reload and no overlap.
4. Token from the environment.
5. Rollback: the config without `local_auth` starts on loopback with authentication disabled (a reviewed security change, see the contract).
6. Refused before readiness (exit 1, no listener, nothing echoed): non-loopback with acknowledgement and no token (the container shape rolled back), non-loopback without acknowledgement, missing token file, token file readable by other, token under 32 bytes, unset variable, `env` and `file` together, an inline value, a newer `schema_version`.

The pairs and fixtures are those of [config-upgrade-rollback](../contracts/config-upgrade-rollback.md) and `tests/fixtures/config/migration/`.

## 6. Reproduce

```sh
sh qualification/build.sh
(cd qualification/sdk/node && npm ci --ignore-scripts)
(cd qualification/sdk/python && python3 -m venv .venv && .venv/bin/pip install --require-hashes --no-deps -r requirements.txt)
(cd examples/node && npm ci --ignore-scripts)
(cd examples/python && python3 -m venv .venv && .venv/bin/pip install --require-hashes --no-deps -r requirements.txt)
sh qualification/run-suites.sh                       # includes local-auth tests and the dual-credential examples
cargo build --locked && sh scripts/smoke-local-auth.sh target/debug/redact-secret-gateway evidence/
```

The Node run writes `local-auth-node.json` and the Python run `local-auth-python.json` into `qualification/evidence/` (status codes, safe codes and counts only; no token value).

## 7. Known limits

- The gateway cannot tell whether an `Authorization` value is a provider key: an application that sends the local token as its provider key sends it to the provider. Only the SDK configuration prevents that.
- Same-host and same-Pod processes that can read the token source, or share the user, are trusted. The local hop is plain HTTP. Loopback without a token is not authentication. No lockout, per-caller identity or rate limiting.
- A token change is a restart followed by an application update; between the two every proxy request is `401`. No seamless rotation.
- Constant-time comparison is a design property (reviewed code and unit tests); this report measures no timing.
- The unauthenticated load is a bounded ordering check on a loaded developer host, not a capacity claim; no number here is a budget (ADR 0008).
- The exact binary is not driven with an accepted request. Accepted-request behavior is established on the qualification build only.
- Detection remains the pinned core's; no statement here promises that every secret is found. Responses from the provider are relayed unredacted.

## 8. Unresolved release gates

- Responses: relay lifecycle (#87), qualification and the final Beta 1 evidence (#88).
- Real-provider evidence: none, and none is planned in this repository.
- Quiet-host performance record, third-party security review, publishing, signing, SBOM and provenance: not done (Beta 2 and Beta 3 scope).
- Nothing here calls schema v1, the limits or any pair stable or supported for production (checked by `tests/config_migration.rs`).
