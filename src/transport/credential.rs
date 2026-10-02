//! Request-local provider credential (#24; ADR 0009, ADR 0016,
//! docs/contracts/headers-and-credentials.md).
//!
//! A [`ProviderCredential`] is the caller-supplied provider `Authorization` value. It is
//! transport authority, not model text: it is never part of a parsed body, never scanned
//! or transformed, and never stored in shared state. It lives exactly as long as one
//! request.
//!
//! Leak resistance by construction:
//! - no `Clone`, `Default`, `Display`, `Serialize`, `Deserialize`, `PartialEq`, or `Hash`;
//! - `Debug` prints a fixed redacted token and nothing derived from the value;
//! - the stored `HeaderValue` is marked sensitive, so HTTP-library `Debug` output and
//!   header compression also treat it as secret;
//! - the only reader is crate-private and is called only by the wire builder.
//!
//! Secure erasure is **not** promised (SECURITY.md, ADR 0007): the bytes may be copied by
//! the allocator, the HTTP stack, and the TLS layer, and are not zeroized on drop.
//!
//! This is the *provider* credential. The Beta 1 local caller token (#12) is a different
//! concept with a different header and a different type; it must never be accepted here
//! and never be forwarded (see `headers::LOCAL_AUTHORITY_PREFIX`).

use std::fmt;

use reqwest::header::HeaderValue;

/// Longest accepted bearer token, in bytes. Real provider keys are far shorter; the cap
/// bounds what a hostile caller can make the gateway hold and forward.
pub const MAX_TOKEN_BYTES: usize = 512;

/// A syntactically valid `Authorization: Bearer <token>` provider credential.
pub struct ProviderCredential {
    /// Normalized `Bearer <token>`, marked sensitive.
    value: HeaderValue,
}

/// Why a credential was refused. Fixed set; carries no input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialError {
    /// Not exactly `Bearer` + one space + a non-empty token of the accepted alphabet.
    Syntax,
}

impl ProviderCredential {
    /// Parse a raw `Authorization` header value. The scheme match is case-insensitive
    /// (RFC 9110); the forwarded form is normalized to `Bearer <token>`. The token
    /// alphabet is `A-Za-z0-9 - . _ ~ + / =` (RFC 6750 `b64token`), which excludes
    /// whitespace, commas (no list-folded credentials), quotes, and control bytes.
    ///
    /// # Errors
    /// [`CredentialError::Syntax`] for any other form, including an empty or oversized
    /// token, other schemes, extra whitespace, and non-ASCII bytes.
    pub fn parse(raw: &[u8]) -> Result<Self, CredentialError> {
        const SCHEME: &[u8] = b"bearer";
        let (scheme, token) = split_once_space(raw).ok_or(CredentialError::Syntax)?;
        if !scheme.eq_ignore_ascii_case(SCHEME)
            || token.is_empty()
            || token.len() > MAX_TOKEN_BYTES
            || !token.iter().all(|b| is_token68(*b))
        {
            return Err(CredentialError::Syntax);
        }
        let mut normalized = Vec::with_capacity(token.len().saturating_add(7));
        normalized.extend_from_slice(b"Bearer ");
        normalized.extend_from_slice(token);
        let mut value =
            HeaderValue::from_bytes(&normalized).map_err(|_| CredentialError::Syntax)?;
        value.set_sensitive(true);
        Ok(Self { value })
    }

    /// Hand the sensitive header value to the wire builder. Consumes the credential so a
    /// request cannot keep a second copy of it in gateway state.
    pub(super) fn into_header_value(self) -> HeaderValue {
        self.value
    }
}

fn split_once_space(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = raw.iter().position(|b| *b == b' ')?;
    let (scheme, rest) = raw.split_at(at);
    Some((scheme, rest.get(1..)?))
}

const fn is_token68(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

impl fmt::Debug for ProviderCredential {
    /// Fixed text. Not even the length is printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderCredential(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTH: &str = "sk-SYNTHETIC-REVOKED-0000-NOT-A-KEY";

    #[test]
    fn accepts_bearer_and_normalizes_scheme_case() {
        for scheme in ["Bearer", "bearer", "BEARER"] {
            let c = ProviderCredential::parse(format!("{scheme} {SYNTH}").as_bytes()).unwrap();
            let v = c.into_header_value();
            assert_eq!(v.as_bytes(), format!("Bearer {SYNTH}").as_bytes());
            assert!(v.is_sensitive());
        }
    }

    #[test]
    fn rejects_every_other_form() {
        let long = format!("Bearer {}", "a".repeat(MAX_TOKEN_BYTES + 1));
        let cases: Vec<&[u8]> = vec![
            b"",
            b"Bearer",
            b"Bearer ",
            b"Bearer  tok",
            b" Bearer tok",
            b"Bearer tok ",
            b"Bearer tok, Bearer tok2",
            b"Bearer to\"k",
            b"Bearer to\tk",
            b"Bearer t\x00k",
            b"Bearer t\xffk",
            b"Basic dXNlcjpwYXNz",
            b"Token tok",
            b"tok",
            long.as_bytes(),
        ];
        for case in cases {
            assert_eq!(
                ProviderCredential::parse(case).unwrap_err(),
                CredentialError::Syntax
            );
        }
        let max = format!("Bearer {}", "a".repeat(MAX_TOKEN_BYTES));
        assert!(ProviderCredential::parse(max.as_bytes()).is_ok());
    }

    #[test]
    fn debug_is_fixed_and_value_free() {
        let c = ProviderCredential::parse(format!("Bearer {SYNTH}").as_bytes()).unwrap();
        assert_eq!(format!("{c:?}"), "ProviderCredential(<redacted>)");
        assert!(!format!("{c:#?}").contains("SYNTHETIC"));
    }
}
