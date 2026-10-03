# Examples

Everything here uses synthetic placeholders or asks for your own key at run time. No file contains a real credential, and none may.

| Path | What it is | Verified how |
| --- | --- | --- |
| `config.openai.json` | Working static config: loopback listener, `openai` upstream, profile `full`, capacity numbers that are provisional and unmeasured (ADR 0008). | `cargo test` validates it with the real binary; the candidate smoke checks start the candidate binary with it. |
| `config.skeleton.json` | Minimal config with no upstream: health endpoints only; valid proxy-route requests get `501`. Used by smoke tests. | CI. |
| `node/` | OpenAI Node SDK (npm `openai` 7.27.0, lockfile with integrity hashes) pointed at the gateway: one JSON call, one streamed call, optional `--demo-redaction`. Needs Node.js 24+ and `OPENAI_API_KEY`. | Type-checked and run in CI against the qualification build (a separate non-release build with a fake provider and a synthetic key). Never run against the real provider by this repository. |
| `python/` | OpenAI Python SDK (PyPI `openai` 3.24.0, `requirements.txt` with sha256 hashes) doing the same. Needs Python 3.13 and `OPENAI_API_KEY`. | Same. |
| `compose/compose.yaml` | One gateway container for one application, host-loopback port only, no key or env var. Mounts a local caller token file as a Compose secret (the gateway refuses to start without it); the examples above send it from `GATEWAY_LOCAL_TOKEN` in `X-Gateway-Local-Token`. | `docker compose config`, start, health, and rejection probes against the exact candidate image in CI. |
| `perf_workloads.rs`, `core_probe_bench.rs` | Synthetic measurement tools (ADR 0008). Not performance claims. | `--smoke` runs in CI. |
| `keepalive_cost.rs`, `tls_handshake_cost.rs`, `conn_cost.rs` | Loopback connection-cost measurement tools for the connection-reuse decision (#42, ADR 0024, ADR 0022): local keep-alive against one connection per request, TCP and TLS setup against a reused connection, per-connection memory. Standalone: no product or qualification-seam change; run with `cargo run --locked --release --example <name>`. Not performance claims. | Built by `cargo clippy --all-targets`; not run in CI. |

The Responses examples (`node/responses-via-gateway.ts`, `python/responses_via_gateway.py`, #87) check the provider's terminal event (`response.completed`, `response.failed`, `response.incomplete`) instead of `finish_reason`, and fail on a stream with none.

Run order for a clean user is in the root [README](../README.md#try-it). Safety notes: the key is read from `OPENAI_API_KEY` by your application and sent to the gateway as a Bearer token, which forwards it only to the provider; the gateway cannot stop your application from bypassing it; loopback is not authentication (the local caller token, `GATEWAY_LOCAL_TOKEN`, is a separate credential from your provider key); SDK retries are disabled in the examples (see the retry guidance in [errors and telemetry](../docs/contracts/errors-and-telemetry.md)); and Node.js callers should check `finish_reason` on streams, because on some Node.js versions (observed on 22.16.0, not on 24) fetch reports a truncated stream as a normal end.
