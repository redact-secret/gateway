# ADR 0013: Fixed HTTPS destinations and outbound authority

Status: Accepted (design); implemented for the destination, client, and address layer (#23). Request-body forwarding is implemented in #20 ([ADR 0017](0017-json-forwarding-deadlines-and-cancellation.md)), with idle pooling disabled so a request is never replayed on a reused connection. Refines [ADR 0009](0009-credential-and-upstream-trust-model.md); implements its "Upstream" section.

## Context

ADR 0009 requires static origins, exact routes, mandatory TLS verification, no redirects, no inherited proxy, and SSRF/rebinding protection, but does not choose the first provider, the origin spelling rules, the address policy, or how tests reach a loopback fake without weakening production. Issue #23 owns those choices.

## Decision

### First provider and route mapping

The first reviewed provider is the OpenAI public API (README "OpenAI Chat Completions"). The reviewed profile `openai` maps exactly one route:

| Route id | Method | Origin | Path |
| --- | --- | --- | --- |
| `openai.chat_completions` | `POST` | `https://api.openai.com` (port 443) | `/v1/chat/completions` |

The origin allowlist is compiled in. Deployment authority selects only the profile name (`deployment.upstream.provider`); configuration has no hostname, URL, port, TLS, proxy, or test field, and unknown fields are rejected. Adding a provider or host is a reviewed code change with an ADR update (private provider endpoints are a non-goal for Alpha 1). The schema stays at `schema_version` 1: the field is additive and optional, the build is unreleased (`0.1.0-alpha.0`), and absence means "no upstream configured" (no route exists, so nothing can be forwarded). Config is static and restart-activated.

### Origin policy

`transport::destination::Origin::parse` accepts exactly `https://<host>[:443]` in one canonical spelling and then requires the host to be in the allowlist. It does not canonicalize; it rejects: any other scheme or an uppercase scheme, userinfo, path/query/fragment, backslash, percent escapes, whitespace, bracketed or any IP-literal host in decimal, octal, hex, dotted, or IPv6 form (the final host label must begin with a letter), single-label hosts, uppercase, non-ASCII, `xn--` labels, trailing dots, empty labels, any port other than 443 (including `:0443`, `:80`), and unreviewed hosts. As a final check the URL library's parser must agree with the strict parser on scheme, host, port, userinfo, path, query, and fragment, else the origin is rejected (parser differential).

### Typed API

`Upstream::destination(&RouteId)` and `Upstream::post(&RouteId)` are the only ways to obtain a destination or request builder. No function accepts a caller URL; `Destination` has no public constructor; its URL is crate-private. Compile-fail tests pin these.

### Client

One `reqwest::Client` is built once at startup from the immutable plan and shared by all routes (all routes have identical trust settings: redirects off, no proxy, HTTPS only, verified TLS, same resolver). It has no default headers: credentials are request-local (ADR 0009). Settings: `redirect(Policy::none())`, `no_proxy()` (inherited `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` ignored, and the `system-proxy` reqwest feature stays off), `https_only(true)`, `referer(false)`, certificate and hostname verification at the verified default through the platform verifier (no `danger_*` call exists anywhere, and no config or flag can add one). Only `POST` is representable, so `CONNECT` is unreachable; the client has no proxy, and #20's header allowlist must drop `Connection: upgrade`/`Upgrade` (planned). Content profile or route selection cannot alter any of this because those inputs do not reach client construction.

### Address policy and pinning

The client resolves only through `PolicyResolver`, which (1) refuses any name outside the allowlist, (2) resolves with the system resolver, (3) rejects the whole answer if it is empty or contains any disallowed address, and (4) returns exactly the validated addresses to the client, which connects only to those. Validation and connection therefore share one resolution (no second lookup, so a rebinding answer cannot be swapped in between). Disallowed: IPv4 `0/8`, `10/8`, `100.64/10`, `127/8`, `169.254/16` (cloud metadata), `172.16/12`, `192.0.0/24`, `192.0.2/24`, `192.88.99/24`, `192.168/16`, `198.18/15`, `198.51.100/24`, `203.0.113/24`, `224/4`, `240/4`; IPv6 is an allowlist of global unicast `2000::/3` minus `2001::/23`, `2001:db8::/32`, `2002::/16`, `3fff::/20`, which rejects unspecified, loopback, IPv4-mapped/compatible, NAT64, unique-local, link-local, and multicast. This is the public-provider deployment policy; it is not configurable.

### Test-only upstream access

Tests reach a loopback fake only through `#[cfg(test)]` items (`Origin::for_test_http`, `Origin::for_test_https`, `AddressPolicy::PublicOrLoopback`, `Scheme::Http`) compiled only into the library's own unit-test build (`src/transport/tests.rs`). They are not cargo features, are not in the library or binary of any other build, and no config field, flag, or environment variable names them; the code reads no environment variables. `tests/destination_policy.rs` checks from outside that: the seams are `cfg(test)`-gated in source; `Cargo.toml` has no `[features]` and no CI/release tooling passes `--features` or `--cfg`; the real binary rejects test/insecure/origin/proxy fields and ignores environment; and compile-fail tests show external crates cannot build a test origin or a destination. The TLS rejection tests generate throwaway certificates at test time with `rcgen` and serve them with `tokio-rustls` (dev-dependencies only, exact pins, aws-lc-rs provider already in the lock; the normal dependency graph is unchanged).

## Owner

`transport` enforces; `config` selects the provider profile; the maintainer approves any change to the allowlist, address policy, or test seams.

## Invariants

1. Only a `RouteId` selects a destination; no request, config, or environment value supplies a host.
2. Every outbound connection is HTTPS to the reviewed host on 443 with verified certificate and hostname.
3. Redirects are never followed; the environment cannot add a proxy.
4. A connection address is one the policy validated in the same resolution.
5. No production path reaches a test seam.

## Failure behavior

An unreviewed provider name or a failed constant check fails startup (`Services::init`, readiness stays false). Per-request failures return `TransportError` with a fixed code and no host, address, URL, header, or body. A denied address, wrong certificate, or any transport failure is an error to the caller, never a retry or fallback.

## Residual risks and operator assumptions

- The gateway cannot prevent a compromised or misconfigured host from bypassing it; direct egress is stopped only by operator egress controls. Recommended: allow outbound TCP 443 only to the provider's published ranges or through the operator's approved egress path, deny all else, block metadata endpoints at the network layer too.
- The system resolver and network path are trusted to return the real provider address. A poisoned resolver that returns a different public address is stopped only by TLS hostname verification; it can still cause denial of service. DNS-level controls (validating resolver, DoT/DoH, split-horizon denial of private answers) are operator responsibility.
- A mandatory operator-side outbound proxy is out of scope for Alpha 1 (no proxy is supported). Deployments that force traffic through a transparent TLS-intercepting proxy must install that proxy's CA in the platform trust store; the gateway then trusts it. That is a deployment trust decision, not a gateway feature.
- Platform trust roots are used (platform verifier). A compromised root store defeats hostname pinning; certificate or SPKI pinning is not implemented.
- The `rustls-platform-verifier` behavior on each OS is trusted; revocation checking follows the platform.
- Address policy applies at connect time per connection; established keep-alive connections are not re-validated for their lifetime.
- Resolver latency/timeout values are measured later (ADR 0008); none are invented here.
- Header allowlist, hop-by-hop and upgrade removal, credential forwarding, and body transmission are #20/#24 (header and credential rules landed with #24, [ADR 0016](0016-header-allowlists-and-request-local-credentials.md); transmission is #20); this ADR guarantees only that they run on a client that can reach nothing else.

## Implementation handoff

- #20 forwards through `Upstream::post(&RouteId)` and must not build a client or URL.
- #24/#25 extend header and credential handling on request builders, never on the shared client.
- A second provider needs an ADR and an allowlist entry.

## Verification

Unit tests in `src/transport/` (origin table, address table, resolver, redirects with 301/302/303/307/308 and zero follow-up requests, HTTPS-only, wrong-hostname/self-signed/unknown-CA rejection with a valid-chain control, denied-address rejection with zero connections, proxy environment in a child process, header-override and no-default-credential tests, profile variation); `tests/destination_policy.rs` and `tests/ui/` from outside; `cargo deny check`.

## Deferred measured choices

Connect/response timeouts and pool tuning (ADR 0008).
