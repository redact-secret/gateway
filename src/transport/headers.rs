//! Header and framing policy (#24; ADR 0016, docs/contracts/headers-and-credentials.md).
//!
//! Three one-way steps, each an allowlist rather than a denylist:
//!
//! 1. [`vet_inbound`] reads a caller's request headers and yields a [`VettedHeaders`]
//!    holding only the request-local provider credential and the two reviewed metadata
//!    values. Everything else the caller sent is *ignored* (and therefore never
//!    forwarded), except the few forms that make the request ambiguous, which are
//!    *rejected*.
//! 2. [`wire_headers`] builds the complete outbound header set from a [`VettedHeaders`], a
//!    fixed set of gateway-chosen values, and the exact length of the sanitized body.
//!    Nothing is copied from the inbound request, so `Host`, `Content-Length`,
//!    `Transfer-Encoding`, `Connection`, `Expect`, `Upgrade`, `Proxy-*`, `Forwarded`,
//!    `X-Forwarded-*`, cookies, SDK telemetry, and any future local-authority header
//!    cannot reach the provider. The URL authority (hence `Host`) comes only from the
//!    reviewed destination.
//! 3. [`relay_response_headers`] allowlists provider response headers for #20/#21.
//!
//! No function here sees a request body, a `ValidatedRequest`, or the original bytes, so
//! header and transport state cannot retain an original-body reference.

use std::fmt;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

use super::credential::ProviderCredential;

/// Reserved prefix for local gateway authority headers (Beta 1 #12; the token header is
/// [`super::local_auth::LOCAL_TOKEN_HEADER`], #63).
///
/// A caller token proving the caller may use the gateway is *not* the provider
/// credential, travels in a different header (name fixed by #12 under this prefix), and
/// is consumed locally. Any header with this prefix is ignored by [`vet_inbound`] and,
/// because [`wire_headers`] is an allowlist, can never be forwarded. [`is_local_authority`]
/// is the one predicate for the reservation.
pub const LOCAL_AUTHORITY_PREFIX: &str = "x-gateway-local-";

/// Total bytes of header names plus values accepted from a caller. Larger requests are
/// refused before any body capacity is reserved.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Longest accepted single header value, in bytes.
pub const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;

/// Longest accepted organization/project identifier, in bytes.
pub const MAX_METADATA_BYTES: usize = 128;

/// At most this many `Connection` tokens are examined.
const MAX_CONNECTION_TOKENS: usize = 32;

/// Longest provider response header value that is relayed; longer values are dropped.
pub const MAX_RELAYED_VALUE_BYTES: usize = 1024;

const ORGANIZATION: HeaderName = HeaderName::from_static("openai-organization");
const PROJECT: HeaderName = HeaderName::from_static("openai-project");

/// Hop-by-hop headers (RFC 9110 section 7.6.1 plus legacy `Proxy-Connection`). Never
/// forwarded in either direction.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers a `Connection` header may not nominate: removing them would change framing,
/// content type, or the request's meaning, so the combination is ambiguous.
const NOT_NOMINATABLE: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "content-type",
    "content-encoding",
    "expect",
    "upgrade",
];

/// Provider response headers relayed to the caller (exact names). Everything else is
/// dropped.
const RESPONSE_ALLOWLIST: &[&str] = &[
    "content-type",
    "cache-control",
    "retry-after",
    "x-request-id",
    "openai-processing-ms",
    "openai-version",
];

/// Provider response header prefix relayed (rate-limit reporting).
const RESPONSE_PREFIX_ALLOWLIST: &[&str] = &["x-ratelimit-"];

/// Why inbound headers were refused. Fixed set; carries no header name or value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeaderReject {
    /// Header bytes over [`MAX_HEADER_BYTES`] or a value over [`MAX_HEADER_VALUE_BYTES`].
    TooLarge,
    /// No usable `Authorization` header (absent, or nominated away by `Connection`).
    MissingCredential,
    /// More than one `Authorization`, or one that is not `Bearer <token>`.
    Credential,
    /// A repeated or malformed `OpenAI-Organization` / `OpenAI-Project`.
    Metadata,
    /// Malformed or ambiguous `Connection`, or an `Upgrade`.
    Connection,
    /// `Expect` other than `100-continue`.
    Expectation,
}

impl fmt::Display for HeaderReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "header_too_large",
            Self::MissingCredential => "missing_credential",
            Self::Credential => "invalid_credential",
            Self::Metadata => "invalid_metadata",
            Self::Connection => "invalid_connection",
            Self::Expectation => "unsupported_expectation",
        })
    }
}

impl std::error::Error for HeaderReject {}

/// The request-local result of inbound vetting: the provider credential and the reviewed
/// metadata. Constructed only by [`vet_inbound`]; consumed by [`wire_headers`].
///
/// It has no `Clone`, so there is exactly one copy per request, and it is not stored in
/// any shared state. Its `Debug` reveals only which optional values are present.
pub struct VettedHeaders {
    credential: ProviderCredential,
    organization: Option<HeaderValue>,
    project: Option<HeaderValue>,
}

impl fmt::Debug for VettedHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VettedHeaders")
            .field("credential", &self.credential)
            .field("has_organization", &self.organization.is_some())
            .field("has_project", &self.project.is_some())
            .finish()
    }
}

/// `true` for a name in the space reserved for the Beta 1 local caller token (#12).
#[must_use]
pub fn is_local_authority(name: &HeaderName) -> bool {
    name.as_str().starts_with(LOCAL_AUTHORITY_PREFIX)
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

/// Vet a caller's request headers. See the contract for the full table; in summary:
///
/// - consumed: `Authorization` (exactly one, `Bearer`), `OpenAI-Organization` and
///   `OpenAI-Project` (at most one each, restricted alphabet), `Connection` (to learn which
///   headers to treat as removed), `Expect` (only `100-continue`);
/// - everything else, including `Host`, `Content-Length`, `User-Agent`, `Accept*`, cookies,
///   `X-Forwarded-*`, SDK telemetry, and local-authority headers, is ignored.
///
/// Body framing (`Content-Length`, `Transfer-Encoding`) is validated by the route
/// admission, not here, because it depends on the body limits.
///
/// # Errors
/// A [`HeaderReject`] for every refusal.
pub fn vet_inbound(headers: &HeaderMap) -> Result<VettedHeaders, HeaderReject> {
    let mut total = 0_usize;
    for (name, value) in headers {
        if value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(HeaderReject::TooLarge);
        }
        total = total.saturating_add(name.as_str().len().saturating_add(value.len()));
    }
    if total > MAX_HEADER_BYTES {
        return Err(HeaderReject::TooLarge);
    }

    if headers.contains_key(header::UPGRADE) {
        return Err(HeaderReject::Connection);
    }
    let nominated = connection_nominated(headers)?;
    let removed = |name: &HeaderName| nominated.iter().any(|n| n == name);

    let mut expect = headers.get_all(header::EXPECT).iter();
    match (expect.next(), expect.next()) {
        (None, _) => {}
        (Some(v), None) if v.as_bytes().eq_ignore_ascii_case(b"100-continue") => {}
        _ => return Err(HeaderReject::Expectation),
    }

    // A nominated `Authorization` is, per RFC 9110 section 7.6.1, removed by the first
    // intermediary; it is therefore absent here rather than silently forwarded.
    if removed(&header::AUTHORIZATION) {
        return Err(HeaderReject::MissingCredential);
    }
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let credential = match (values.next(), values.next()) {
        (None, _) => return Err(HeaderReject::MissingCredential),
        (Some(v), None) => {
            ProviderCredential::parse(v.as_bytes()).map_err(|_| HeaderReject::Credential)?
        }
        (Some(_), Some(_)) => return Err(HeaderReject::Credential),
    };

    let organization = metadata(headers, &ORGANIZATION, removed(&ORGANIZATION))?;
    let project = metadata(headers, &PROJECT, removed(&PROJECT))?;

    Ok(VettedHeaders {
        credential,
        organization,
        project,
    })
}

/// Names listed in `Connection`. Any malformed token, or a token naming a header that
/// cannot be removed without changing framing, makes the request ambiguous.
fn connection_nominated(headers: &HeaderMap) -> Result<Vec<HeaderName>, HeaderReject> {
    let mut out = Vec::new();
    for value in headers.get_all(header::CONNECTION) {
        let text = value.to_str().map_err(|_| HeaderReject::Connection)?;
        for token in text.split(',') {
            let token = token.trim_matches([' ', '\t']);
            if token.is_empty() {
                continue;
            }
            if out.len() >= MAX_CONNECTION_TOKENS {
                return Err(HeaderReject::Connection);
            }
            let name =
                HeaderName::from_bytes(token.as_bytes()).map_err(|_| HeaderReject::Connection)?;
            if NOT_NOMINATABLE.contains(&name.as_str()) {
                return Err(HeaderReject::Connection);
            }
            out.push(name);
        }
    }
    Ok(out)
}

/// Whether a `Connection` header nominates `name` as hop-by-hop (so an intermediary would
/// have removed it). Used by local authentication (#63): a nominated token header is absent.
///
/// # Errors
/// [`HeaderReject::Connection`] when `Connection` itself is malformed or ambiguous.
pub(crate) fn connection_nominates(
    headers: &HeaderMap,
    name: &HeaderName,
) -> Result<bool, HeaderReject> {
    Ok(connection_nominated(headers)?.iter().any(|n| n == name))
}

/// Zero or one identifier of `[A-Za-z0-9_.-]{1,128}`; a nominated header is absent.
fn metadata(
    headers: &HeaderMap,
    name: &HeaderName,
    nominated: bool,
) -> Result<Option<HeaderValue>, HeaderReject> {
    if nominated {
        return Ok(None);
    }
    let mut values = headers.get_all(name).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return if headers.contains_key(name) {
            Err(HeaderReject::Metadata)
        } else {
            Ok(None)
        };
    };
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_METADATA_BYTES
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return Err(HeaderReject::Metadata);
    }
    let mut value = value.clone();
    // Organization and project ids identify the caller's billing context: treat them as
    // sensitive in HTTP-library debug output too.
    value.set_sensitive(true);
    Ok(Some(value))
}

/// The gateway's own `User-Agent`. Fixed per build; the caller's is never forwarded.
const USER_AGENT: &str = concat!("redact-secret-gateway/", env!("CARGO_PKG_VERSION"));

/// Complete outbound header set. `body_len` is the exact length of the sanitized body the
/// caller of this function will send; nothing the caller of the gateway declared can
/// influence it.
///
/// `Host` is deliberately absent: the HTTP client derives it from the reviewed destination
/// URL only. `Transfer-Encoding` is absent: the body is a single sealed buffer sent with an
/// explicit `Content-Length`.
#[must_use]
pub fn wire_headers(vetted: VettedHeaders, body_len: usize) -> HeaderMap {
    let mut map = HeaderMap::with_capacity(8);
    map.insert(header::AUTHORIZATION, vetted.credential.into_header_value());
    map.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // Provider responses are relayed byte for byte; asking for no content coding keeps
    // them decodable by the caller.
    map.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    map.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    map.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    map.insert(header::CONTENT_LENGTH, HeaderValue::from(body_len));
    if let Some(v) = vetted.organization {
        map.insert(ORGANIZATION, v);
    }
    if let Some(v) = vetted.project {
        map.insert(PROJECT, v);
    }
    map
}

/// Header names [`wire_headers`] can emit (lowercase). Single source for tests and docs.
pub const WIRE_HEADER_NAMES: &[&str] = &[
    "authorization",
    "content-type",
    "accept-encoding",
    "accept",
    "user-agent",
    "content-length",
    "openai-organization",
    "openai-project",
];

/// A provider response cannot be relayed faithfully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResponseHeaderError {
    /// A content coding other than `identity`: relaying the bytes without the coding
    /// header would corrupt them, and the gateway does not decode.
    ContentEncoding,
}

impl ResponseHeaderError {
    #[must_use]
    pub const fn code(self) -> crate::telemetry::SafeCode {
        crate::telemetry::SafeCode::TransportFailure
    }
}

impl fmt::Display for ResponseHeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for ResponseHeaderError {}

/// The provider response headers to relay: an allowlist (exact names plus `x-ratelimit-*`),
/// minus anything the response's own `Connection` nominates, minus over-long values.
/// `Set-Cookie`, hop-by-hop headers, framing (`Content-Length`, `Transfer-Encoding`),
/// `Content-Encoding`, `Location`, `WWW-Authenticate`, `Server`, CORS, and every other
/// header are dropped; the gateway's HTTP server regenerates framing for the caller.
///
/// # Errors
/// [`ResponseHeaderError::ContentEncoding`] if the provider applied a content coding.
pub fn relay_response_headers(headers: &HeaderMap) -> Result<HeaderMap, ResponseHeaderError> {
    for value in headers.get_all(header::CONTENT_ENCODING) {
        if !value.as_bytes().eq_ignore_ascii_case(b"identity") {
            return Err(ResponseHeaderError::ContentEncoding);
        }
    }
    let nominated: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim_matches([' ', '\t']).as_bytes()).ok())
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let allowed = RESPONSE_ALLOWLIST.contains(&name.as_str())
            || RESPONSE_PREFIX_ALLOWLIST
                .iter()
                .any(|p| name.as_str().starts_with(p));
        if !allowed
            || is_hop_by_hop(name)
            || nominated.contains(name)
            || value.len() > MAX_RELAYED_VALUE_BYTES
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-SYNTHETIC-REVOKED-1111-NOT-A-KEY";

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (n, v) in pairs {
            m.append(
                HeaderName::from_bytes(n.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    fn auth() -> (&'static str, String) {
        ("authorization", format!("Bearer {KEY}"))
    }

    fn vet(extra: &[(&str, &str)]) -> Result<VettedHeaders, HeaderReject> {
        let (n, v) = auth();
        let mut pairs = vec![(n, v.as_str())];
        pairs.extend_from_slice(extra);
        vet_inbound(&map(&pairs))
    }

    #[test]
    fn minimal_request_is_accepted() {
        let v = vet(&[]).unwrap();
        assert!(v.organization.is_none() && v.project.is_none());
    }

    #[test]
    fn credential_outcomes_are_explicit() {
        assert_eq!(
            vet_inbound(&map(&[])).unwrap_err(),
            HeaderReject::MissingCredential
        );
        assert_eq!(
            vet_inbound(&map(&[
                ("authorization", "Bearer a"),
                ("authorization", "Bearer b")
            ]))
            .unwrap_err(),
            HeaderReject::Credential
        );
        // Identical duplicates are still ambiguous.
        assert_eq!(
            vet_inbound(&map(&[
                ("authorization", "Bearer a"),
                ("authorization", "Bearer a")
            ]))
            .unwrap_err(),
            HeaderReject::Credential
        );
        for bad in [
            "Basic abc",
            "Bearer",
            "Bearer a b",
            "Bearer a,Bearer b",
            "a",
        ] {
            assert_eq!(
                vet_inbound(&map(&[("authorization", bad)])).unwrap_err(),
                HeaderReject::Credential
            );
        }
        // Other credential-looking headers are neither accepted nor forwarded.
        for other in ["x-api-key", "api-key", "proxy-authorization", "cookie"] {
            assert_eq!(
                vet_inbound(&map(&[(other, "Bearer a")])).unwrap_err(),
                HeaderReject::MissingCredential
            );
        }
    }

    #[test]
    fn organization_and_project_are_validated() {
        let v = vet(&[
            ("openai-organization", "org-AbC_1.2-3"),
            ("openai-project", "proj_x"),
        ])
        .unwrap();
        assert!(v.organization.is_some() && v.project.is_some());
        for (name, bad) in [
            ("openai-organization", ""),
            ("openai-organization", "org evil"),
            ("openai-organization", "org,other"),
            ("openai-project", "proj/../x"),
            ("openai-project", "p\u{e9}"),
        ] {
            let out = if bad.is_ascii() {
                vet(&[(name, bad)])
            } else {
                let (n, v) = auth();
                let mut m = map(&[(n, v.as_str())]);
                m.append(
                    HeaderName::from_static("openai-project"),
                    HeaderValue::from_bytes(bad.as_bytes()).unwrap(),
                );
                vet_inbound(&m)
            };
            assert_eq!(out.unwrap_err(), HeaderReject::Metadata);
        }
        let long = "a".repeat(MAX_METADATA_BYTES + 1);
        assert_eq!(
            vet(&[("openai-project", long.as_str())]).unwrap_err(),
            HeaderReject::Metadata
        );
        assert_eq!(
            vet(&[("openai-organization", "a"), ("openai-organization", "b")]).unwrap_err(),
            HeaderReject::Metadata
        );
    }

    #[test]
    fn connection_nomination_removes_headers() {
        // Nominating an allowlisted header removes it: the credential is gone, so the
        // request is refused rather than forwarded with it.
        assert_eq!(
            vet(&[("connection", "Authorization")]).unwrap_err(),
            HeaderReject::MissingCredential
        );
        // Optional metadata that is nominated is dropped, not forwarded.
        let v = vet(&[
            ("connection", "close, openai-project"),
            ("openai-project", "p1"),
        ])
        .unwrap();
        assert!(v.project.is_none());
        // Plain keep-alive/close and unknown names are fine.
        assert!(vet(&[("connection", "keep-alive, x-foo")]).is_ok());
        // Ambiguous nominations and syntax errors are refused.
        for bad in [
            "content-length",
            "Transfer-Encoding",
            "host",
            "upgrade",
            "a b",
            "a;b",
        ] {
            assert_eq!(
                vet(&[("connection", bad)]).unwrap_err(),
                HeaderReject::Connection,
                "{bad}"
            );
        }
        assert_eq!(
            vet(&[("upgrade", "websocket")]).unwrap_err(),
            HeaderReject::Connection
        );
    }

    #[test]
    fn expect_only_100_continue() {
        assert!(vet(&[("expect", "100-Continue")]).is_ok());
        assert_eq!(
            vet(&[("expect", "200-ok")]).unwrap_err(),
            HeaderReject::Expectation
        );
    }

    #[test]
    fn oversized_headers_are_refused() {
        let big = "a".repeat(MAX_HEADER_VALUE_BYTES + 1);
        assert_eq!(
            vet(&[("x-big", big.as_str())]).unwrap_err(),
            HeaderReject::TooLarge
        );
        let each = "a".repeat(2000);
        let many: Vec<(String, &str)> = (0..9)
            .map(|i| (format!("x-pad-{i}"), each.as_str()))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        assert_eq!(vet(&refs).unwrap_err(), HeaderReject::TooLarge);
    }

    #[test]
    fn wire_headers_are_regenerated_not_copied() {
        let (n, v) = auth();
        let inbound = map(&[
            (n, v.as_str()),
            ("host", "evil.example"),
            ("content-length", "999999"),
            ("content-type", "text/plain"),
            ("user-agent", "OpenAI/Python 9"),
            ("x-stainless-os", "x"),
            ("x-forwarded-for", "10.0.0.1"),
            ("x-forwarded-host", "evil.example"),
            ("forwarded", "for=1.1.1.1"),
            ("via", "1.1 proxy"),
            ("proxy-authorization", "Basic abc"),
            ("cookie", "s=1"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("x-gateway-local-caller-token", "LOCAL-ONLY"),
            ("openai-organization", "org-1"),
            ("accept-encoding", "gzip"),
            ("accept", "text/html"),
        ]);
        let wire = wire_headers(vet_inbound(&inbound).unwrap(), 42);
        let mut names: Vec<&str> = wire.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        let mut expected = vec![
            "accept",
            "accept-encoding",
            "authorization",
            "content-length",
            "content-type",
            "openai-organization",
            "user-agent",
        ];
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(wire[header::CONTENT_LENGTH], "42");
        assert_eq!(wire[header::CONTENT_TYPE], "application/json");
        assert_eq!(wire[header::ACCEPT_ENCODING], "identity");
        assert!(
            wire[header::USER_AGENT]
                .to_str()
                .unwrap()
                .starts_with("redact-secret-gateway/")
        );
        assert!(wire[header::AUTHORIZATION].is_sensitive());
        for name in wire.keys() {
            assert!(WIRE_HEADER_NAMES.contains(&name.as_str()));
            assert!(!is_hop_by_hop(name) && !is_local_authority(name));
        }
    }

    #[test]
    fn local_authority_prefix_is_reserved() {
        assert!(is_local_authority(&HeaderName::from_static(
            "x-gateway-local-caller-token"
        )));
        assert!(!is_local_authority(&header::AUTHORIZATION));
    }

    #[test]
    fn debug_never_prints_values() {
        let v = vet(&[("openai-organization", "org-SYNTH-ORG-MARK")]).unwrap();
        let text = format!("{v:?}");
        assert!(!text.contains("SYNTHETIC") && !text.contains("SYNTH-ORG"));
        let w = wire_headers(v, 1);
        let text = format!("{w:?}");
        assert!(!text.contains("SYNTHETIC") && !text.contains("SYNTH-ORG"));
    }

    #[test]
    fn response_headers_are_allowlisted() {
        let upstream = map(&[
            ("content-type", "application/json"),
            ("set-cookie", "a=b"),
            ("connection", "keep-alive, x-request-id"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("content-length", "5"),
            ("x-request-id", "req_1"),
            ("openai-processing-ms", "12"),
            ("x-ratelimit-remaining-requests", "9"),
            ("location", "https://evil.example"),
            ("www-authenticate", "Bearer"),
            ("server", "x"),
            ("access-control-allow-origin", "*"),
            ("retry-after", "3"),
        ]);
        let out = relay_response_headers(&upstream).unwrap();
        let mut names: Vec<&str> = out.keys().map(HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "content-type",
                "openai-processing-ms",
                "retry-after",
                "x-ratelimit-remaining-requests"
            ]
        );
        let long = "a".repeat(MAX_RELAYED_VALUE_BYTES + 1);
        assert!(
            relay_response_headers(&map(&[("x-request-id", long.as_str())]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            relay_response_headers(&map(&[("content-encoding", "gzip")])).unwrap_err(),
            ResponseHeaderError::ContentEncoding
        );
        assert!(relay_response_headers(&map(&[("content-encoding", "identity")])).is_ok());
    }
}
