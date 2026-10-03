//! Reviewed upstream destinations (ADR 0009, ADR 0013, docs/contracts/upstream-destinations.md).
//!
//! A destination is derived only from a reviewed provider profile selected by deployment
//! authority. The origin allowlist is compiled in, so no configuration value, request
//! path, query, header, body value, or absolute-form target can name a host. The API that
//! turns a route into a URL accepts only a [`RouteId`]; there is no function from a URL
//! string supplied by a caller to a destination.
//!
//! [`Origin::parse`] is the strict canonical-form gate used for the reviewed constants and
//! exercised with hostile inputs in tests. It refuses to canonicalize: any input that is not
//! already in the one accepted spelling is rejected, so two spellings of a host can never
//! be compared unequal by one component and equal by another. As a final differential
//! check the HTTP client library's own URL parser must agree with this parser about the
//! host and port.

use std::fmt;

use crate::config::{Provider, RouteId};

/// The only port policy: the HTTPS default. `:443` may be spelled or omitted; nothing else
/// is accepted, and reviewed origins never carry a non-default port.
pub const HTTPS_PORT: u16 = 443;

/// Compiled-in hostname allowlist: exactly the reviewed first-provider origin (ADR 0013).
const REVIEWED_HOSTS: &[&str] = &["api.openai.com"];

/// HTTP method a reviewed route may use. Only `POST` exists; `CONNECT` and every other
/// method are unrepresentable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteMethod {
    Post,
}

impl RouteMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Post => "POST",
        }
    }
}

/// Why an origin string was rejected. Fixed set; never carries the offending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OriginError {
    /// Not exactly the lowercase `https://` scheme.
    Scheme,
    /// Userinfo (`user:pass@`) present.
    Userinfo,
    /// Path, query, fragment, backslash, or percent-escape present; an origin is only
    /// scheme + host + optional default port.
    NotOriginForm,
    /// Host is empty, uppercase, non-ASCII, internationalized (`xn--`), has a trailing or
    /// empty label, is an IP literal in any notation, or is otherwise not a canonical
    /// registered-domain-shaped name.
    Host,
    /// A port other than the explicit-or-implied 443.
    Port,
    /// Well-formed, but not in the reviewed origin allowlist.
    NotReviewed,
    /// The URL library disagrees with the strict parser about host or port.
    ParserDifferential,
}

impl fmt::Display for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Scheme => "origin_scheme",
            Self::Userinfo => "origin_userinfo",
            Self::NotOriginForm => "origin_not_origin_form",
            Self::Host => "origin_host",
            Self::Port => "origin_port",
            Self::NotReviewed => "origin_not_reviewed",
            Self::ParserDifferential => "origin_parser_differential",
        })
    }
}

impl std::error::Error for OriginError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheme {
    Https,
    /// Plain HTTP exists only in unit-test builds, for the loopback fake upstream.
    #[cfg(test)]
    Http,
}

impl Scheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            #[cfg(test)]
            Self::Http => "http",
        }
    }
}

/// A validated upstream origin. Production values are always `https`, on port 443, with a
/// canonical lowercase ASCII hostname from the reviewed allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    scheme: Scheme,
    host: Box<str>,
    port: u16,
}

impl Origin {
    /// Strictly parse `https://host[:443]` and require it to be in the reviewed allowlist.
    ///
    /// # Errors
    /// An [`OriginError`] for every non-canonical, non-HTTPS, userinfo-bearing,
    /// non-default-port, IP-literal, or unreviewed input.
    pub fn parse(input: &str) -> Result<Self, OriginError> {
        let origin = Self::parse_syntax(input)?;
        if !REVIEWED_HOSTS.contains(&&*origin.host) {
            return Err(OriginError::NotReviewed);
        }
        Ok(origin)
    }

    fn parse_syntax(input: &str) -> Result<Self, OriginError> {
        let authority = input.strip_prefix("https://").ok_or(OriginError::Scheme)?;
        if authority.contains('@') {
            return Err(OriginError::Userinfo);
        }
        if authority.bytes().any(|b| {
            matches!(
                b,
                b'/' | b'?' | b'#' | b'\\' | b'%' | b' ' | b'\t' | b'\r' | b'\n'
            )
        }) {
            return Err(OriginError::NotOriginForm);
        }
        if authority.contains(['[', ']']) {
            return Err(OriginError::Host);
        }
        let (host, port) = match authority.split_once(':') {
            None => (authority, HTTPS_PORT),
            Some((host, digits)) => (host, parse_port(digits)?),
        };
        if port != HTTPS_PORT {
            return Err(OriginError::Port);
        }
        validate_host(host)?;
        let origin = Self {
            scheme: Scheme::Https,
            host: host.into(),
            port,
        };
        origin.cross_check()?;
        Ok(origin)
    }

    /// The HTTP client library's URL parser must agree with the strict parser. Any
    /// disagreement is a parser differential and is rejected.
    fn cross_check(&self) -> Result<(), OriginError> {
        let url = reqwest::Url::parse(&self.authority_url())
            .map_err(|_| OriginError::ParserDifferential)?;
        let agrees = url.scheme() == self.scheme.as_str()
            && url.host_str() == Some(&*self.host)
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(self.port)
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none();
        if agrees {
            Ok(())
        } else {
            Err(OriginError::ParserDifferential)
        }
    }

    fn authority_url(&self) -> String {
        if self.port == HTTPS_PORT && self.scheme == Scheme::Https {
            format!("https://{}", self.host)
        } else {
            format!("{}://{}:{}", self.scheme.as_str(), self.host, self.port)
        }
    }

    #[must_use]
    pub const fn is_https(&self) -> bool {
        matches!(self.scheme, Scheme::Https)
    }

    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Unit-test-only origin for a loopback fake upstream, plain HTTP, any port.
    /// Compiled out of every non-test build: no configuration path reaches it.
    #[cfg(test)]
    pub(crate) fn for_test_http(addr: std::net::SocketAddr) -> Self {
        Self {
            scheme: Scheme::Http,
            host: addr.ip().to_string().into(),
            port: addr.port(),
        }
    }

    /// Unit-test-only HTTPS origin on an arbitrary host and port (a local TLS fake).
    /// Skips the reviewed-host allowlist; still HTTPS with full verification.
    #[cfg(test)]
    pub(crate) fn for_test_https(host: &str, port: u16) -> Self {
        Self {
            scheme: Scheme::Https,
            host: host.into(),
            port,
        }
    }
}

fn parse_port(digits: &str) -> Result<u16, OriginError> {
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return Err(OriginError::Port);
    }
    digits.parse::<u16>().map_err(|_| OriginError::Port)
}

/// Canonical, lowercase, ASCII, dotted registered-domain shape. Rejects IP literals in
/// every notation by requiring the final label to start with a letter: `127.0.0.1`,
/// decimal `2130706433`, octal `0177.0.0.1`, hex `0x7f.1`, and bracketed IPv6 (the bracket
/// and colon characters are not host characters) all fail.
fn validate_host(host: &str) -> Result<(), OriginError> {
    const MAX_HOST: usize = 253;
    const MAX_LABEL: usize = 63;
    if host.is_empty() || host.len() > MAX_HOST || !host.is_ascii() {
        return Err(OriginError::Host);
    }
    let mut labels = 0_u32;
    let mut last = "";
    for label in host.split('.') {
        let valid = !label.is_empty()
            && label.len() <= MAX_LABEL
            && !label.starts_with('-')
            && !label.ends_with('-')
            && !label.starts_with("xn--")
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !valid {
            return Err(OriginError::Host);
        }
        labels = labels.saturating_add(1);
        last = label;
    }
    let final_starts_with_letter = last.bytes().next().is_some_and(|b| b.is_ascii_lowercase());
    if labels < 2 || !final_starts_with_letter {
        return Err(OriginError::Host);
    }
    Ok(())
}

/// A reviewed route: exact path, fixed method, fixed origin. Built from a provider profile
/// and never from request data.
#[derive(Clone, Debug)]
pub struct Destination {
    origin: Origin,
    path: &'static str,
    method: RouteMethod,
    url: reqwest::Url,
}

impl Destination {
    fn new(origin: Origin, path: &'static str, method: RouteMethod) -> Result<Self, OriginError> {
        let text = format!("{}{path}", origin.authority_url());
        let url = reqwest::Url::parse(&text).map_err(|_| OriginError::ParserDifferential)?;
        let exact = url.scheme() == origin.scheme.as_str()
            && url.host_str() == Some(origin.host())
            && url.port_or_known_default() == Some(origin.port())
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == path
            && url.query().is_none()
            && url.fragment().is_none();
        if !exact {
            return Err(OriginError::ParserDifferential);
        }
        Ok(Self {
            origin,
            path,
            method,
            url,
        })
    }

    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    #[must_use]
    pub const fn path(&self) -> &'static str {
        self.path
    }

    #[must_use]
    pub const fn method(&self) -> RouteMethod {
        self.method
    }

    /// The fixed, verified URL. Crate-private: the client uses it, callers cannot replace it.
    pub(super) const fn url(&self) -> &reqwest::Url {
        &self.url
    }

    #[cfg(test)]
    pub(crate) fn for_test(origin: Origin, path: &'static str) -> Result<Self, OriginError> {
        Self::new(origin, path, RouteMethod::Post)
    }
}

/// Operator-visible route identifier for Chat Completions on the first provider.
pub const OPENAI_CHAT_COMPLETIONS_ROUTE: &str = "openai.chat_completions";

/// Operator-visible route identifier for Responses on the first provider (#86). Equal to
/// `Protocol::ResponsesText.route_name()`.
pub const OPENAI_RESPONSES_ROUTE: &str = "openai.responses";

/// One reviewed route in the static table.
#[derive(Clone, Debug)]
pub struct RouteBinding {
    id: RouteId,
    destination: Destination,
}

impl RouteBinding {
    #[must_use]
    pub const fn id(&self) -> &RouteId {
        &self.id
    }

    #[must_use]
    pub const fn destination(&self) -> &Destination {
        &self.destination
    }

    #[cfg(test)]
    pub(crate) const fn for_test(id: RouteId, destination: Destination) -> Self {
        Self { id, destination }
    }
}

/// The reviewed static route table for a provider profile. Exact matching only.
///
/// # Errors
/// [`OriginError`] if a compiled-in constant fails its own validation (a build bug,
/// surfaced as a startup failure rather than a panic).
pub fn reviewed_routes(provider: Provider) -> Result<Vec<RouteBinding>, OriginError> {
    match provider {
        Provider::OpenAi => {
            let origin = Origin::parse("https://api.openai.com")?;
            let destination =
                Destination::new(origin.clone(), "/v1/chat/completions", RouteMethod::Post)?;
            let responses = Destination::new(origin, "/v1/responses", RouteMethod::Post)?;
            Ok(vec![
                RouteBinding {
                    id: RouteId::new(OPENAI_CHAT_COMPLETIONS_ROUTE),
                    destination,
                },
                RouteBinding {
                    id: RouteId::new(OPENAI_RESPONSES_ROUTE),
                    destination: responses,
                },
            ])
        }
    }
}

/// Hostnames the resolver may be asked about for these routes.
#[must_use]
pub fn allowed_hosts(routes: &[RouteBinding]) -> Vec<Box<str>> {
    let mut hosts: Vec<Box<str>> = Vec::new();
    for route in routes {
        let host = route.destination.origin.host();
        if !hosts.iter().any(|h| &**h == host) {
            hosts.push(host.into());
        }
    }
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_origin_parses_and_is_canonical() {
        let o = Origin::parse("https://api.openai.com").expect("reviewed");
        assert!(o.is_https());
        assert_eq!(o.host(), "api.openai.com");
        assert_eq!(o.port(), 443);
        assert_eq!(Origin::parse("https://api.openai.com:443"), Ok(o));
    }

    #[test]
    fn hostile_spellings_cannot_alter_origin_authority() {
        let cases: &[(&str, OriginError)] = &[
            ("http://api.openai.com", OriginError::Scheme),
            ("HTTPS://api.openai.com", OriginError::Scheme),
            ("//api.openai.com", OriginError::Scheme),
            ("api.openai.com", OriginError::Scheme),
            ("https:/api.openai.com", OriginError::Scheme),
            ("https:\\\\api.openai.com", OriginError::Scheme),
            ("https://user@api.openai.com", OriginError::Userinfo),
            ("https://user:pw@api.openai.com", OriginError::Userinfo),
            ("https://api.openai.com@evil.example", OriginError::Userinfo),
            ("https://evil.example@api.openai.com", OriginError::Userinfo),
            ("https://api.openai.com/", OriginError::NotOriginForm),
            ("https://api.openai.com/v1", OriginError::NotOriginForm),
            ("https://api.openai.com?x=1", OriginError::NotOriginForm),
            ("https://api.openai.com#frag", OriginError::NotOriginForm),
            ("https://api.openai.com\\evil", OriginError::NotOriginForm),
            ("https://api.openai.com%2eevil", OriginError::NotOriginForm),
            ("https://api.openai.com ", OriginError::NotOriginForm),
            ("https://api.openai.com\n", OriginError::NotOriginForm),
            ("https://api.openai.com:8443", OriginError::Port),
            ("https://api.openai.com:80", OriginError::Port),
            ("https://api.openai.com:0443", OriginError::Port),
            ("https://api.openai.com:", OriginError::Port),
            ("https://api.openai.com:443:443", OriginError::Port),
            ("https://api.openai.com:65536", OriginError::Port),
            ("https://API.OPENAI.COM", OriginError::Host),
            ("https://Api.openai.com", OriginError::Host),
            ("https://api.openai.com.", OriginError::Host),
            ("https://.api.openai.com", OriginError::Host),
            ("https://api..openai.com", OriginError::Host),
            ("https://", OriginError::Host),
            ("https://api.openai.com\u{200b}", OriginError::Host),
            ("https://api.openai.cоm", OriginError::Host),
            ("https://xn--pi-openai.com", OriginError::Host),
            ("https://xn--80ak6aa92e.com", OriginError::Host),
            ("https://localhost", OriginError::Host),
            ("https://127.0.0.1", OriginError::Host),
            ("https://2130706433", OriginError::Host),
            ("https://0177.0.0.1", OriginError::Host),
            ("https://0x7f.0.0.1", OriginError::Host),
            ("https://0x7f000001", OriginError::Host),
            ("https://127.1", OriginError::Host),
            ("https://[::1]", OriginError::Host),
            ("https://[::ffff:127.0.0.1]", OriginError::Host),
            ("https://169.254.169.254", OriginError::Host),
            (
                "https://api.openai.com.evil.example",
                OriginError::NotReviewed,
            ),
            ("https://evilapi.openai.com", OriginError::NotReviewed),
            ("https://api.openai.com.cn", OriginError::NotReviewed),
            ("https://openai.com", OriginError::NotReviewed),
            ("https://example.com", OriginError::NotReviewed),
        ];
        for (input, expected) in cases {
            assert_eq!(Origin::parse(input), Err(*expected), "input {input:?}");
        }
    }

    #[test]
    fn destination_is_fixed_exact_https() {
        let routes = reviewed_routes(Provider::OpenAi).expect("routes");
        let [route, responses] = routes.as_slice() else {
            panic!("two reviewed routes");
        };
        assert_eq!(route.id().as_str(), OPENAI_CHAT_COMPLETIONS_ROUTE);
        assert_eq!(responses.id().as_str(), OPENAI_RESPONSES_ROUTE);
        assert_eq!(
            responses.destination().url().as_str(),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            responses.destination().origin(),
            route.destination().origin()
        );
        let d = route.destination();
        assert_eq!(
            d.url().as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(d.method(), RouteMethod::Post);
        assert_eq!(d.path(), "/v1/chat/completions");
        assert_eq!(allowed_hosts(&routes), vec!["api.openai.com".into()]);
    }
}
