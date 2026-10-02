# Contract: upstream destinations and outbound authority

Status: implemented for destination selection, client construction, and address policy (#23). Body forwarding is planned (#20). Rationale: [ADR 0013](../decisions/0013-fixed-https-destinations-and-outbound-authority.md), [ADR 0009](../decisions/0009-credential-and-upstream-trust-model.md).

## Rules

1. Destinations come from a reviewed provider profile selected by `deployment.upstream.provider` (`openai`). Configuration cannot carry a hostname, URL, port, proxy, TLS setting, or test-mode field; such keys are rejected as unknown fields.
2. Reviewed routes (exact match, no prefix, case-sensitive):

   | Route id | Method | URL |
   | --- | --- | --- |
   | `openai.chat_completions` | `POST` | `https://api.openai.com/v1/chat/completions` |

3. A destination is obtained only by `RouteId` (`Upstream::destination`, `Upstream::post`). Request path, query, headers (including `Host`, `Forwarded`, `X-Forwarded-*`, `X-Original-URL`), body, and absolute-form targets never influence origin, path, or method. Unknown route ids fail closed with `TransportError::UnknownRoute`; with no upstream configured every route is unknown.
4. Origin spelling is strict and not normalized: `https://<lowercase-ascii-host>[:443]` and nothing else (no userinfo, path, query, fragment, backslash, percent escape, IP literal in any notation, IDN/`xn--`, trailing dot, other port). Accepted hosts: `api.openai.com`.
5. Client: one shared client built at startup; redirects never followed; inherited proxy environment ignored; HTTPS only; certificate and hostname verification mandatory with no disabling option; no default headers; `CONNECT` unrepresentable; only `POST`. Upgrade and hop-by-hop header removal is part of #20's header allowlist (planned).
6. Address policy: names resolve only through the policy resolver; every address in an answer must be public (see ADR 0013 for the exact ranges); one bad address rejects the answer; the client connects only to addresses validated in that resolution.
7. Test-mode upstream (loopback, plain HTTP) exists only as `cfg(test)` items in the library's unit-test build. No config, flag, feature, or environment variable enables it in any other build.
8. Errors carry fixed codes (`transport_failure`) only; never hosts, addresses, URLs, headers, credentials, or bodies.

## Not covered (see ADR 0013 residual risks)

Egress firewalling, resolver/DNS integrity, platform trust-store integrity, TLS-intercepting proxies, certificate pinning, timeouts, header allowlists, credential forwarding, and body transmission.

## Evidence

`src/transport/{destination,resolver,tests}.rs`, `src/config.rs` tests, `tests/destination_policy.rs`, `tests/ui/*origin*|*post_with_url*|*destination*`.
