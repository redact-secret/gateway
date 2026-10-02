---
name: owasp-review
description: Review the current code against OWASP guidance (ASVS 5.0, API Security Top 10, Top 10, LLM Top 10, relevant Cheat Sheets) and report which requirements it meets, misses, or cannot be judged. Use when asked for an OWASP review or compliance check ("owasp-review", "/owasp-review src/transport", "does this meet OWASP?"). Read-only; changes nothing.
---

# owasp-review

Review code against OWASP guidance. Report findings only; do not edit files.

## Scope

- Target: the path given as an argument, or else the current diff (`git diff main...HEAD`), or else the whole source tree.
- Read `SECURITY.md` (threat table, objective, non-goals), `ARCHITECTURE.md`, and `CONVENTIONS.md` first. Judge each control only within that boundary. What counts as a secret is the core's decision, so detector accuracy is not a finding. Documented residual risks (plaintext before the gateway, no traversal enforcement, unredacted responses, compromised host, same-host callers on loopback) are not findings.
- The repo is in scaffolding. `ARCHITECTURE.md` describes the target contract. Review implemented code against it. For a control whose code does not exist yet, mark it `not implemented` and say which issue or milestone owns it, never `pass`.
- This is a network service that terminates HTTP from a local caller and makes outbound HTTPS calls. Authentication of users and sessions is mostly `n/a`; the local caller token arrives in Beta 1. The risks are plaintext reaching the provider, SSRF and credential misrouting, request smuggling, resource exhaustion, and diagnostic leakage.

## Checklist

Map the code to the OWASP areas that apply. Skip areas that do not.

| Area | Source | Check |
| --- | --- | --- |
| Sensitive data exposure | ASVS data-protection chapter, Logging Cheat Sheet | Bodies, provider keys, caller tokens, raw findings, and matched snippets never appear in logs, errors, telemetry, health output, panics, or crash messages. Telemetry labels are bounded route IDs and coarse outcomes only |
| Fail-closed processing | ASVS error-handling chapter, Error Handling Cheat Sheet | No body bytes go upstream before complete inspection and transformation succeed; every failure rejects with a stable safe code that echoes no payload; incomplete core results reject |
| Input validation | ASVS validation chapter, Input Validation Cheat Sheet | Exact route and method match; strict JSON (duplicate keys, invalid UTF-8, depth); recursive field classification with unknown-field rejection; structural values constrained; unsupported content types, compression, and encodings rejected; query strings rejected |
| Request framing | ASVS HTTP security chapter, HTTP Request Smuggling guidance | Conflicting `Content-Length` and `Transfer-Encoding`, duplicate headers, and obsolete folding are rejected; outbound length is recomputed; hop-by-hop headers removed; CONNECT and upgrades rejected |
| SSRF and outbound routing | SSRF Prevention Cheat Sheet, API Security Top 10 (SSRF) | Upstream origins and routes are static config only; no destination from headers, query, body, or absolute-form targets; redirects disabled; inherited proxy env ignored; upstream TLS certificate and hostname verification mandatory; DNS and network restrictions documented |
| Credential handling | ASVS authentication and secrets guidance, Secrets Management Cheat Sheet | Provider key forwarded only to the configured upstream and never echoed; local caller token separate and stripped before forwarding; no credentials in config examples, defaults, or CLI args that leak via process listings; no persistent key storage |
| Resource consumption | API Security Top 10 (unrestricted resource consumption), Denial of Service Cheat Sheet | Explicit limits for body bytes, depth, nodes, inspected text, findings, output, connections, queues, and deadlines; global memory and concurrency budget; no unbounded queue, task, or buffer; overload rejects before allocation; CPU inspection does not monopolize the async reactor |
| Streaming and cancellation | ASVS business-logic chapter | SSE relay bounded in buffering, lifetime, and idle time; downstream disconnect cancels upstream; every spawned task has a cleanup owner; no fabricated completion event after interruption; no replay of the original payload on transport failure |
| Listener and deployment | ASVS configuration chapter, Docker and Kubernetes Cheat Sheets | Loopback by default; non-loopback needs explicit documented configuration; config validated before traffic; unknown fields rejected; non-root, minimal-capability container; health and readiness leak nothing and issue no credentialed probe |
| Untrusted content and LLM risks | OWASP Top 10 for LLM Applications (sensitive information disclosure, excessive agency), LLM Prompt Injection Prevention Cheat Sheet | Tool results, tool arguments, tool descriptions, and metadata reaching the provider are classified and inspected; redaction is not treated as permission to execute tools; the gateway never executes tools |
| Memory-safe Rust practice | ASVS dependency and code-quality guidance | No `unsafe` code (`CONVENTIONS.md` prohibits it; `forbid(unsafe_code)` is set in every crate root and no `allow(unsafe_code)` exists); no panics, `unwrap`, or unchecked indexing and arithmetic on untrusted input; typed errors |
| Supply chain | ASVS dependency chapter, Software Supply Chain Security guidance | `Cargo.lock` committed; exact core pin; minimal dependency features; `--locked` builds; pinned actions and base image; checksums for artifacts; no provenance or signing claimed unless implemented |
| Detector boundary | `CONVENTIONS.md` § Dependency boundaries | No provider token regexes, scoring, or PII logic in this repo; core is used through pinned public APIs only |

Cite the requirement text itself. Look up the current ASVS 5.0 requirement number rather than quoting one from memory.

## Output

One table, most severe first:

| Status | Severity | OWASP ref | file:line | Evidence | Fix |
| --- | --- | --- | --- | --- | --- |

- Status: `pass`, `fail`, `not implemented` (with the owning milestone), or `n/a` (with the reason).
- Every `fail` needs a concrete scenario: input → wrong outcome.
- End with a one-line verdict and the requirements that could not be judged without runtime testing. Hand those to `vulnerability-test`.

## Rules

- Use synthetic values only. Never paste real secrets or a finding's plaintext.
- Name the specific requirement or Cheat Sheet for every row.
- Do not claim compliance or certification. Say "meets the reviewed requirements".
