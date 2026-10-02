# Security policy and threat-model baseline

## Status and supported versions

The gateway is in architecture/scaffolding development. No stable release or production support is currently claimed. Alpha and beta artifacts, when published, are experimental and have only the explicitly recorded qualification coverage. This policy must be updated with supported versions when releases exist.

## Reporting a vulnerability privately

Do not disclose sensitive exploit details, real credentials, or personal data in a public issue.

Use GitHub's private vulnerability reporting flow for `redact-secret/gateway` (Security tab, "Report a vulnerability"). The GitHub API reports it as enabled (`private-vulnerability-reporting` returned `enabled: true`, checked 2026-10-02). If no private reporting flow is available, contact the maintainer through an already established private channel. If none is known, a public request for a private contact may contain only the request, with no exploit details or sensitive payload.

The channel is enabled and verified through the GitHub API; repository admins receive reports through GitHub's advisory workflow. No unverified security email, response SLA, bug bounty, or disclosure deadline is promised here.

Private reports should provide affected versions/commits, deployment assumptions, a synthetic reproduction, expected boundary behavior, observed behavior, and impact. Do not use live credentials to demonstrate a leak.

## Security objective

For qualified request forms that traverse Gateway, process the complete bounded model-bound text according to the pinned core and configured policy before forwarding any request-body bytes upstream. Reject unsupported or incomplete processing. Protect credential routing and prevent unsafe diagnostics.

This objective does not guarantee detection of every secret, protect plaintext before Gateway, enforce network traversal, inspect provider-stored content, sanitize provider responses, or protect against a compromised Gateway host.

## Assets and trust boundaries

Assets include original request text, provider credentials, optional local caller tokens, transformed requests, core/configuration integrity, and process/network resources. The Gateway handles plaintext in memory. It does not promise secure erasure of every allocator/TLS buffer, encrypted RAM, or protection from privileged host inspection.

The first stable trust domain is one application with a localhost companion or Kubernetes sidecar. Loopback is an address restriction, not caller authentication. Same-host or same-Pod processes and operators must be considered in the deployment assumptions. Multi-tenant and remote caller operation are not supported initially.

## Threats and required controls

| Threat | Required control / evidence |
| --- | --- |
| Partial request disclosure before a later scan failure | Receive and inspect the entire bounded request before sending upstream body bytes; fake-upstream negative tests |
| Inspection truncation, finding exhaustion, detector failure | Verify core completion semantics; reject incomplete results; never silently pass remaining text |
| Unknown fields or structural-value covert channels | Explicit recursive field classification, constrained control values, unknown-field rejection |
| JSON ambiguity or escaping bypass | Reject duplicate keys/malformed UTF-8; scan decoded text; preserve structure and Unicode |
| SSRF, redirects, credential destination changes | Fixed route mapping, HTTPS validation, redirects disabled, DNS/network restrictions, no caller-chosen URLs or inherited proxy routing |
| Header/request smuggling ambiguity | Strict framing validation, vetted header forwarding, hop-by-hop removal, regenerated outbound length, deployment-chain tests |
| Secret exposure in logs, errors, telemetry | No bodies/keys/snippets/raw findings; safe error taxonomy; bounded metric labels; leakage tests |
| Resource exhaustion | Per-request and global limits, bounded queues/CPU work, deadlines, concurrency control, overload rejection |
| Slow downstream or abandoned SSE streams | Backpressure, bounded buffering, idle/lifetime limits, upstream cancellation and cleanup |
| Credential confusion | Separate provider key from local caller token; strip local authority header; no persistent credential storage |
| Duplicate billable calls | Gateway retries disabled; SDK retry behavior documented; no exactly-once claim |
| Direct upstream bypass | Operator-enforced egress restrictions; explicit residual risk in deployment documentation |
| Vulnerable or substituted release artifact | Exact candidate qualification, dependency review, checksums and planned provenance/SBOM controls |

## Request and response handling

Unsupported images, audio, files, external/stored content references, opaque payloads, compression, realtime inputs, arbitrary routes, and binary inspection are rejected initially. New forms require a reviewed contract and evidence; they must not become default pass-through exceptions.

Provider authentication headers are intentionally delivered to the configured provider and must not appear in diagnostics. Request redaction is separate from credential transport. Redacted text can alter model/tool behavior; Gateway does not authorize tool execution.

Initial responses, including provider error bodies and SSE events, are not content-sanitized. Applications must not assume they are safe to log or store. Cancellation cannot retract bytes already transmitted. After streaming starts, failure follows the documented termination contract ([ADR 0018](docs/decisions/0018-sse-relay-termination-and-stream-bounds.md)): the stream ends abruptly with no completion event, so it cannot be disguised as a successful response.

## Operations and residual risks

Default to loopback; validate deployment exposure; run containers without root or unnecessary capabilities; keep configuration and credentials outside public artifacts. Bound and document process memory, including buffered plaintext. Operators are responsible for access control around crash dumps, diagnostics, host memory, and any surrounding log collectors.

Outbound destination policy (fixed HTTPS origin, address policy, no redirects, no inherited proxy) is implemented in the transport layer ([contract](docs/contracts/upstream-destinations.md)). Operators must still enforce egress controls (direct upstream bypass is not preventable by the gateway), trust the host resolver and platform trust store, and understand that a transparent TLS-intercepting proxy requires its CA in the trust store; see the residual risks in [ADR 0013](docs/decisions/0013-fixed-https-destinations-and-outbound-authority.md).

Connection handling ([ADR 0019](docs/decisions/0019-request-head-guard-and-one-request-per-connection.md), #25): a request head that carries both `Content-Length` and `Transfer-Encoding` is answered with a local `400` and the connection is closed before the HTTP parser sees it (the stack cannot expose this itself, [ADR 0021](docs/decisions/0021-framing-ambiguity-parser-level-investigation.md)); a head that is not complete within the head deadline is closed silently; and every response asks the connection to close, so one request is served per connection. This is a structural check, not proof against all request smuggling; any intermediary placed in front of the gateway must reject ambiguous framing itself. The connection count is bounded at accept by `resources.limits.max_connections` (provisional default 256; [ADR 0022](docs/decisions/0022-connection-bound-at-accept.md), #40); a connection over the bound is closed without a response. The threat-to-test mapping, tested stack pins, environment-specific cases, and residual risks are in [docs/qualification/alpha1-threat-control-map.md](docs/qualification/alpha1-threat-control-map.md); it distinguishes rejection proof from detector recall and claims no third-party audit.

Health/readiness must reveal no secrets and must not issue credentialed upstream probes by default. No provider key persistence, shared authorization platform, inbound TLS platform, restore service, or Vault coupling is included in the first stable contract.

## Security release gates

Before Alpha 1 distribution: private reporting flow verified (enabled, checked through the GitHub API on 2026-10-02), license selected (MIT, done), foundational threat model and upstream/credential contracts reviewed, synthetic no-forward and diagnostic-leak tests passed (done; see the [Alpha 1 qualification report](docs/qualification/alpha1-qualification-report.md)), and the exact candidate artifacts smoke-tested (done for the unpublished candidate; re-run on any published artifact).

Before stable 0.1.0: protocol coverage and unknown-field gates, core completion/failure evidence, header/SSRF tests, stream failure/cancellation/backpressure tests, measured aggregate resource bounds, pinned SDK compatibility, dependency/artifact review, and documented upgrade/rollback are complete. Beta 3 owns final reconciliation; unresolved blockers prevent stable promotion.

No claimed third-party audit or penetration test exists unless its scope, reviewer, date, and evidence are explicitly recorded.
