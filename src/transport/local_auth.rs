//! Local caller authentication (#63; ADR 0030, docs/contracts/local-caller-auth.md).
//!
//! One shared secret answers one question: "may this local process use this gateway
//! instance?" It is not identity and not the provider credential. This module owns the
//! whole boundary so a route cannot invent its own variant of it:
//!
//! - [`LocalToken`]: the configured token, resolved once at startup from an `env` or `file`
//!   reference ([`resolve_reference`]), held without `Clone`, `Display`, equality or
//!   serialization, with a fixed redacted `Debug`.
//! - [`LocalAuth`]: the immutable plan field (`Disabled` or `Token`), built at startup and
//!   handed to each proxy route. [`LocalAuth::screen`] is the single decision a route makes
//!   first, before any body, reservation, or upstream contact.
//! - [`AuthReject`]: the two fixed outcomes (`local_auth_required`, `local_auth_invalid`).
//!
//! The candidate header value is compared in constant time over zero-padded 128-byte
//! buffers; it is never logged, stored, or placed in an error. Secure erasure is not
//! promised (SECURITY.md, ADR 0007): the token lives for the process lifetime in the plan,
//! was read through ordinary buffers at startup, and may persist in freed memory, swap, or a
//! core dump.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName};
use subtle::ConstantTimeEq;

use super::headers;
use crate::config::{ConfigError, ConfigErrorKind};
use crate::telemetry::SafeCode;

/// The request header carrying the local caller token. Inside the reserved
/// [`headers::LOCAL_AUTHORITY_PREFIX`] namespace, so it can never be forwarded.
pub const LOCAL_TOKEN_HEADER: &str = "x-gateway-local-token";

/// Shortest accepted token, in bytes. A floor, not a recommendation.
pub const MIN_TOKEN_BYTES: usize = 32;
/// Longest accepted token, in bytes.
pub const MAX_TOKEN_BYTES: usize = 128;
/// Longest accepted token file, in bytes (token plus an optional line ending, with room).
pub const MAX_TOKEN_FILE_BYTES: usize = 256;

/// Static schema location of every token-source failure. Never the name or path.
const TOKEN_LOCATION: &str = "deployment.local_auth.token";

const NAME: HeaderName = HeaderName::from_static(LOCAL_TOKEN_HEADER);

/// Why a request failed local authentication. Fixed set; carries no header value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthReject {
    /// The header is absent, or `Connection` nominated it away.
    Required,
    /// Duplicate, malformed, out of bounds, or not the configured token.
    Invalid,
}

impl AuthReject {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::Required => SafeCode::LocalAuthRequired,
            Self::Invalid => SafeCode::LocalAuthInvalid,
        }
    }
}

/// Where the token is read from at startup. A reference, never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenReference {
    /// An environment variable name (`[A-Z_][A-Z0-9_]{0,63}`).
    Env(String),
    /// An absolute file path.
    File(PathBuf),
}

/// The configured local caller token. No `Clone`, `Default`, `Display`, `Serialize`, or
/// equality operator; `Debug` prints a fixed marker.
pub struct LocalToken {
    buf: [u8; MAX_TOKEN_BYTES],
    len: usize,
}

impl fmt::Debug for LocalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalToken(<redacted>)")
    }
}

/// `true` when `bytes` is 32 to 128 bytes of RFC 3986 unreserved characters.
fn token_shaped(bytes: &[u8]) -> bool {
    (MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Zero-filled 128-byte copy of `bytes` (truncated past 128, which the length compare
/// rejects).
fn pad(bytes: &[u8]) -> [u8; MAX_TOKEN_BYTES] {
    let mut out = [0_u8; MAX_TOKEN_BYTES];
    let n = bytes.len().min(MAX_TOKEN_BYTES);
    if let (Some(dst), Some(src)) = (out.get_mut(..n), bytes.get(..n)) {
        dst.copy_from_slice(src);
    }
    out
}

impl LocalToken {
    /// Build from token bytes.
    ///
    /// # Errors
    /// [`ConfigErrorKind::InvalidValue`] (at the static token location) when the bytes are
    /// shorter than 32, longer than 128, or outside `A-Za-z0-9-._~`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ConfigError> {
        if !token_shaped(bytes) {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidValue,
                TOKEN_LOCATION,
            ));
        }
        Ok(Self {
            buf: pad(bytes),
            len: bytes.len(),
        })
    }

    /// Constant-time equality with `candidate`, for any candidate length (including zero and
    /// beyond 128). Both sides are compared as zero-padded 128-byte buffers and the lengths
    /// are compared as well; every step runs unconditionally, so run time does not depend on
    /// the matching prefix or on the configured length.
    #[must_use]
    pub fn matches(&self, candidate: &[u8]) -> bool {
        let padded = pad(candidate);
        let same_bytes = self.buf.as_slice().ct_eq(padded.as_slice());
        let ours = u64::try_from(self.len).unwrap_or(u64::MAX);
        let theirs = u64::try_from(candidate.len()).unwrap_or(u64::MAX);
        bool::from(same_bytes & ours.ct_eq(&theirs))
    }
}

/// The immutable local-authentication authority in the runtime plan. Cheap to clone (one
/// `Arc`); the token itself is never copied.
#[derive(Clone, Debug)]
pub enum LocalAuth {
    /// No caller check (the Alpha behavior); supported on a loopback listener only.
    Disabled,
    /// Every proxy `POST` must carry the configured token.
    Token(Arc<LocalToken>),
}

impl LocalAuth {
    /// `true` when callers must present a token.
    #[must_use]
    pub const fn is_enforced(&self) -> bool {
        matches!(self, Self::Token(_))
    }

    /// The one local-authentication decision for a proxy route. Call it first, before the
    /// body is touched, anything is reserved, or `100 Continue` is possible. Only `POST`
    /// is authenticated: other methods on a served route get the route's own `405`, which
    /// does not depend on a secret.
    ///
    /// # Errors
    /// [`AuthReject::Required`] for an absent header or one nominated away by `Connection`;
    /// [`AuthReject::Invalid`] for a duplicate, malformed, out-of-bounds, wrong, or
    /// unparseable-`Connection` request. Failures share status, body, and headers.
    pub fn screen(&self, method: &Method, headers: &HeaderMap) -> Result<(), AuthReject> {
        let Self::Token(token) = self else {
            return Ok(());
        };
        if method != Method::POST {
            return Ok(());
        }
        match headers::connection_nominates(headers, &NAME) {
            Ok(false) => {}
            Ok(true) => return Err(AuthReject::Required),
            Err(_) => return Err(AuthReject::Invalid),
        }
        let mut values = headers.get_all(&NAME).iter();
        let Some(first) = values.next() else {
            return Err(AuthReject::Required);
        };
        if values.next().is_some() {
            return Err(AuthReject::Invalid);
        }
        let bytes = first.as_bytes();
        if !token_shaped(bytes) || !token.matches(bytes) {
            return Err(AuthReject::Invalid);
        }
        Ok(())
    }
}

fn err(kind: ConfigErrorKind) -> ConfigError {
    ConfigError::new(kind, TOKEN_LOCATION)
}

/// Strip one trailing `\n` or `\r\n`; any other whitespace stays and fails validation.
fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes)
}

/// Resolve a reference once at startup.
///
/// # Errors
/// [`ConfigError`] at `deployment.local_auth.token` with a fixed kind; the variable name,
/// the path, and the content are never echoed.
pub fn resolve_reference(reference: &TokenReference) -> Result<LocalToken, ConfigError> {
    match reference {
        TokenReference::Env(name) => resolve_env_with(name, |n| std::env::var_os(n)),
        TokenReference::File(path) => resolve_file(path),
    }
}

/// Environment resolution over an injectable lookup (tests cannot mutate the process
/// environment: `unsafe` is forbidden). Unset and empty share the `unreadable` kind.
fn resolve_env_with(
    name: &str,
    lookup: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Result<LocalToken, ConfigError> {
    let value = lookup(name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| err(ConfigErrorKind::Unreadable))?;
    let text = value
        .to_str()
        .ok_or_else(|| err(ConfigErrorKind::InvalidValue))?;
    LocalToken::from_bytes(text.as_bytes())
}

/// File resolution: the path is checked once, then the opened handle is checked again
/// (`fstat`), so the size and mode rules apply to the file actually read. The first check
/// keeps a FIFO or device from blocking the open.
fn resolve_file(path: &Path) -> Result<LocalToken, ConfigError> {
    let before = std::fs::metadata(path).map_err(|_| err(ConfigErrorKind::Unreadable))?;
    if !before.is_file() {
        return Err(err(ConfigErrorKind::InvalidValue));
    }
    let file = std::fs::File::open(path).map_err(|_| err(ConfigErrorKind::Unreadable))?;
    let meta = file
        .metadata()
        .map_err(|_| err(ConfigErrorKind::Unreadable))?;
    let max = u64::try_from(MAX_TOKEN_FILE_BYTES).unwrap_or(u64::MAX);
    if !meta.is_file() || meta.len() > max || !mode_ok(&meta) {
        return Err(err(ConfigErrorKind::InvalidValue));
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| err(ConfigErrorKind::Unreadable))?;
    if bytes.len() > MAX_TOKEN_FILE_BYTES {
        return Err(err(ConfigErrorKind::InvalidValue));
    }
    LocalToken::from_bytes(strip_line_ending(&bytes))
}

/// On Unix the file must not be readable, writable, or executable by "other"; group access
/// is allowed so a Kubernetes `fsGroup` mount works. Other platforms have no such bits.
#[cfg(unix)]
fn mode_ok(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o007 == 0
}

#[cfg(not(unix))]
fn mode_ok(_meta: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use reqwest::header::HeaderValue;

    use super::*;

    const GOOD: &str = "SYNTH-local-token-0123456789-abcdefghij";

    fn auth() -> LocalAuth {
        LocalAuth::Token(Arc::new(LocalToken::from_bytes(GOOD.as_bytes()).unwrap()))
    }

    fn headers(values: &[&[u8]]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append(NAME, HeaderValue::from_bytes(v).unwrap());
        }
        map
    }

    #[test]
    fn token_syntax_bounds_and_alphabet() {
        let ok = |s: &str| LocalToken::from_bytes(s.as_bytes()).is_ok();
        assert!(ok(&"a".repeat(32)));
        assert!(ok(&"a".repeat(128)));
        assert!(ok(&format!("A-z.0_9~{}", "x".repeat(25))));
        assert!(!ok(&"a".repeat(31)));
        assert!(!ok(&"a".repeat(129)));
        assert!(!ok(""));
        for bad in ["+", "/", "=", " ", "\"", ",", "\t", "é", "Bearer x"] {
            assert!(!ok(&format!("{}{bad}", "a".repeat(40))), "{bad:?}");
        }
    }

    #[test]
    fn debug_never_prints_the_token() {
        let shown = format!("{:?} {:?}", auth(), LocalToken::from_bytes(GOOD.as_bytes()));
        assert!(!shown.contains("SYNTH"));
        assert!(shown.contains("LocalToken(<redacted>)"));
    }

    #[test]
    fn comparison_handles_every_length_without_panic() {
        let token = LocalToken::from_bytes(GOOD.as_bytes()).unwrap();
        assert!(token.matches(GOOD.as_bytes()));
        for n in 0..=300 {
            let candidate = vec![b'a'; n];
            assert!(!token.matches(&candidate), "len {n}");
        }
        // The token plus the padding value is not equal, nor is a prefix of it.
        let mut padded = GOOD.as_bytes().to_vec();
        padded.push(0);
        assert!(!token.matches(&padded));
        assert!(!token.matches(&GOOD.as_bytes()[..GOOD.len() - 1]));
    }

    #[test]
    fn disabled_ignores_the_header_entirely() {
        for h in [headers(&[]), headers(&[b"x", b"y"]), headers(&[b"\xff"])] {
            assert_eq!(LocalAuth::Disabled.screen(&Method::POST, &h), Ok(()));
        }
    }

    #[test]
    fn every_header_row_of_the_contract() {
        let a = auth();
        let long = "a".repeat(129);
        let short = "a".repeat(31);
        let g = GOOD.as_bytes();
        type Case<'a> = (Vec<&'a [u8]>, Result<(), AuthReject>);
        let cases: Vec<Case<'_>> = vec![
            (vec![], Err(AuthReject::Required)),
            (vec![g], Ok(())),
            (vec![g, g], Err(AuthReject::Invalid)),
            (vec![g, b"other"], Err(AuthReject::Invalid)),
            (vec![b""], Err(AuthReject::Invalid)),
            (
                vec![b"Bearer SYNTH-local-token-0123456789-abcdefghij"],
                Err(AuthReject::Invalid),
            ),
            (
                vec![b"SYNTH-local-token-0123456789-abcdefghij,x"],
                Err(AuthReject::Invalid),
            ),
            (
                vec![b" SYNTH-local-token-0123456789-abcdefghij"],
                Err(AuthReject::Invalid),
            ),
            (
                vec![b"\"SYNTH-local-token-0123456789-abcdefghij\""],
                Err(AuthReject::Invalid),
            ),
            (
                vec![b"SYNTH-local-token-0123456789-abcdefgh\xff"],
                Err(AuthReject::Invalid),
            ),
            (vec![long.as_bytes()], Err(AuthReject::Invalid)),
            (vec![short.as_bytes()], Err(AuthReject::Invalid)),
            (
                vec![b"SYNTH-local-token-0123456789-abcdefghiX"],
                Err(AuthReject::Invalid),
            ),
        ];
        for (values, want) in cases {
            assert_eq!(
                a.screen(&Method::POST, &headers(&values)),
                want,
                "{values:?}"
            );
        }
    }

    #[test]
    fn connection_nomination_removes_the_header_and_other_methods_pass_through() {
        let a = auth();
        let mut h = headers(&[GOOD.as_bytes()]);
        h.insert(
            "connection",
            HeaderValue::from_static("X-Gateway-Local-Token"),
        );
        assert_eq!(a.screen(&Method::POST, &h), Err(AuthReject::Required));
        h.insert("connection", HeaderValue::from_static("bad token"));
        assert_eq!(a.screen(&Method::POST, &h), Err(AuthReject::Invalid));
        // Wrong methods are the route's 405, which does not depend on a secret.
        assert_eq!(a.screen(&Method::GET, &headers(&[])), Ok(()));
    }

    #[test]
    fn reject_codes_are_the_two_new_spellings() {
        assert_eq!(AuthReject::Required.code().as_str(), "local_auth_required");
        assert_eq!(AuthReject::Invalid.code().as_str(), "local_auth_invalid");
    }

    #[test]
    fn env_resolution_kinds() {
        assert!(resolve_env_with("X", |_| Some(OsString::from(GOOD))).is_ok());
        let kind = |v: Option<OsString>| resolve_env_with("X", |_| v).unwrap_err().kind();
        assert_eq!(kind(None), ConfigErrorKind::Unreadable);
        assert_eq!(kind(Some(OsString::new())), ConfigErrorKind::Unreadable);
        assert_eq!(
            kind(Some(OsString::from("short"))),
            ConfigErrorKind::InvalidValue
        );
        assert_eq!(
            kind(Some(OsString::from(format!("{GOOD}\n")))),
            ConfigErrorKind::InvalidValue
        );
    }
}
