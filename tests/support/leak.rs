//! Synthetic canary markers and a leakage scanner.
//!
//! Markers are unmistakably synthetic strings (never credential-shaped for any real
//! provider). A test puts them into request bodies, headers, or arguments, runs gateway
//! code, then scans everything the gateway generated (errors, `Debug`/`Display` output,
//! logs, stdout/stderr) and fails if any marker appears.
//!
//! The scanner never prints the marker value or the haystack. A [`Leak`] reports only
//! the marker label, the source name, and a byte offset, so a failing CI run does not
//! itself leak the synthetic value.

use std::fmt;

/// Marker planted in request bodies.
pub const BODY_MARKER: &str = "SYNTH-CANARY-BODY-7Q2XK9-NOT-A-CREDENTIAL";
/// Marker planted in request header values.
pub const HEADER_MARKER: &str = "SYNTH-CANARY-HEADER-4M8TZ1-NOT-A-CREDENTIAL";
/// Marker planted in JSON object keys (keys can leak through duplicate-key errors).
pub const KEY_MARKER: &str = "SYNTH-CANARY-KEY-9D3WR5-NOT-A-CREDENTIAL";
/// Marker planted in command-line arguments and URLs.
pub const ARG_MARKER: &str = "SYNTH-CANARY-ARG-2H6VB8-NOT-A-CREDENTIAL";

/// A set of labelled markers to search for.
pub struct Markers {
    items: Vec<(&'static str, String)>,
}

impl fmt::Debug for Markers {
    /// Labels only; values are never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let labels: Vec<&str> = self.items.iter().map(|(l, _)| *l).collect();
        f.debug_struct("Markers").field("labels", &labels).finish()
    }
}

/// A marker was found in gateway-generated output. Carries no marker value.
#[derive(Debug, PartialEq, Eq)]
pub struct Leak {
    pub marker_label: &'static str,
    pub source: String,
    pub offset: usize,
}

impl fmt::Display for Leak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "synthetic marker `{}` leaked into `{}` at byte {}",
            self.marker_label, self.source, self.offset
        )
    }
}

impl std::error::Error for Leak {}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

impl Markers {
    /// The four standard markers.
    #[must_use]
    pub fn standard() -> Self {
        Self::empty()
            .with("body", BODY_MARKER)
            .with("header", HEADER_MARKER)
            .with("key", KEY_MARKER)
            .with("arg", ARG_MARKER)
    }

    #[must_use]
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }

    #[must_use]
    pub fn with(mut self, label: &'static str, value: &str) -> Self {
        self.items.push((label, value.to_owned()));
        self
    }

    /// Scan `haystack` for every marker, its case variants, and a 16-byte prefix (to
    /// catch truncated echoes). `source` names where the bytes came from.
    pub fn scan(&self, source: &str, haystack: &[u8]) -> Result<(), Leak> {
        for (label, value) in &self.items {
            let prefix_len = value.len().min(16);
            let variants = [
                value.clone(),
                value.to_ascii_lowercase(),
                value.to_ascii_uppercase(),
                value[..prefix_len].to_owned(),
            ];
            for variant in &variants {
                if let Some(offset) = find(haystack, variant.as_bytes()) {
                    return Err(Leak {
                        marker_label: label,
                        source: source.to_owned(),
                        offset,
                    });
                }
            }
        }
        Ok(())
    }

    /// Scan text. See [`Markers::scan`].
    pub fn scan_str(&self, source: &str, text: &str) -> Result<(), Leak> {
        self.scan(source, text.as_bytes())
    }

    /// Panic with a value-free message if any marker is present.
    pub fn assert_clean(&self, source: &str, haystack: &[u8]) {
        if let Err(leak) = self.scan(source, haystack) {
            panic!("{leak}");
        }
    }

    /// Scan the `Display` and `Debug` rendering of a value.
    pub fn assert_clean_fmt<T: fmt::Display + fmt::Debug>(&self, source: &str, value: &T) {
        self.assert_clean(&format!("{source} (Display)"), value.to_string().as_bytes());
        self.assert_clean(
            &format!("{source} (Debug)"),
            format!("{value:?}").as_bytes(),
        );
    }

    /// Scan the `Debug` rendering of a value that has no `Display`.
    pub fn assert_clean_debug<T: fmt::Debug>(&self, source: &str, value: &T) {
        self.assert_clean(
            &format!("{source} (Debug)"),
            format!("{value:?}").as_bytes(),
        );
    }
}
