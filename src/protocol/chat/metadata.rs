//! `metadata`: a closed string-to-string map. Owner: #55. Contract:
//! `docs/contracts/chat-completions-request.md`, ADR 0025 (D1, D8, D9).
//!
//! Keys are LINK labels (detect-only: a core finding blocks the request and a key is never
//! rewritten); values are strings inspected as redactable text. There is no other value
//! type, no nesting, and no passthrough: a number, boolean, `null`, array, object, an
//! empty or over-long key, a key outside the charset, or a seventeenth entry rejects the
//! request before inspection. `classify` routes the key here; nothing else edits this file.

use std::io::{self, Write};

use super::serialize::{Bounded, json_str};
use super::tool_calls::Derived;
use super::{Checked, SerializeError, TextSlot, unsupported};
use crate::protocol::json::Json;

/// Most entries (the provider's published limit).
pub const MAX_METADATA_ENTRIES: usize = 16;
/// Longest key, in bytes.
pub const MAX_METADATA_KEY_BYTES: usize = 64;
/// Longest value, in bytes, at parse time and again after redaction.
pub const MAX_METADATA_VALUE_BYTES: usize = 512;

/// Parsed metadata. `present` keeps `"metadata":{}` distinct from an absent field so the
/// outbound document has the same shape as the accepted one.
#[derive(Default)]
pub struct Metadata {
    present: bool,
    entries: Vec<(String, String)>,
}

impl std::fmt::Debug for Metadata {
    /// Prints only the entry count: keys and values are caller payload.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Metadata({})", self.entries.len())
    }
}

impl Metadata {
    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A LINK label: 1 to 64 bytes of `[A-Za-z0-9_.:-]`.
fn is_link(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_METADATA_KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// The `metadata` value. Anything but an object of at most 16 LINK-keyed string entries
/// (values of at most 512 bytes, no duplicate keys) is rejected. The entries are also
/// charged (two nodes and the key plus value bytes per entry) against the request-wide
/// derived counters shared with tool arguments, so metadata cannot add to them unaccounted.
/// The charge is conservative: the parse budgets already counted the same strings once.
pub(in crate::protocol) fn parse(value: Json, derived: &mut Derived) -> Checked<Metadata> {
    let Json::Object(items) = value else {
        return Err(unsupported());
    };
    if items.len() > MAX_METADATA_ENTRIES {
        return Err(unsupported());
    }
    let mut entries: Vec<(String, String)> = Vec::with_capacity(items.len());
    for (key, value) in items {
        let Json::String(text) = value else {
            return Err(unsupported());
        };
        // The parser already rejects duplicate keys; this is the second, local guard so the
        // invariant does not depend on it.
        if !is_link(&key)
            || text.len() > MAX_METADATA_VALUE_BYTES
            || entries.iter().any(|(k, _)| *k == key)
        {
            return Err(unsupported());
        }
        derived.charge(2, key.len().saturating_add(text.len()))?;
        entries.push((key, text));
    }
    Ok(Metadata {
        present: true,
        entries,
    })
}

/// Which part of an entry the traversal reached, with the entry ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::protocol) enum MetaLeaf {
    Key(usize),
    Value(usize),
}

/// Visit metadata texts: key, then value, per entry, in input order. Shared by Chat
/// (traversal position 5 in `slots.rs`) and Responses.
pub(in crate::protocol) fn visit_leaves(metadata: &Metadata, f: &mut impl FnMut(MetaLeaf, &str)) {
    for (entry, (key, value)) in metadata.entries.iter().enumerate() {
        f(MetaLeaf::Key(entry), key);
        f(MetaLeaf::Value(entry), value);
    }
}

/// Mutable twin of [`visit_leaves`].
pub(in crate::protocol) fn visit_leaves_mut(
    metadata: &mut Metadata,
    f: &mut impl FnMut(MetaLeaf, &mut String),
) {
    for (entry, (key, value)) in metadata.entries.iter_mut().enumerate() {
        f(MetaLeaf::Key(entry), key);
        f(MetaLeaf::Value(entry), value);
    }
}

const fn slot_of(leaf: MetaLeaf) -> TextSlot {
    match leaf {
        MetaLeaf::Key(entry) => TextSlot::MetadataKey { entry },
        MetaLeaf::Value(entry) => TextSlot::MetadataValue { entry },
    }
}

pub(super) fn visit(metadata: &Metadata, f: &mut impl FnMut(TextSlot, &str)) {
    visit_leaves(metadata, &mut |leaf, text| f(slot_of(leaf), text));
}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(metadata: &mut Metadata, f: &mut impl FnMut(TextSlot, &mut String)) {
    visit_leaves_mut(metadata, &mut |leaf, text| f(slot_of(leaf), text));
}

/// Write `,"metadata":{...}` when present.
pub(in crate::protocol) fn write(metadata: &Metadata, w: &mut Bounded) -> io::Result<()> {
    if !metadata.present {
        return Ok(());
    }
    w.write_all(b",\"metadata\":{")?;
    for (index, (key, value)) in metadata.entries.iter().enumerate() {
        if index > 0 {
            w.write_all(b",")?;
        }
        json_str(w, key)?;
        w.write_all(b":")?;
        json_str(w, value)?;
    }
    w.write_all(b"}")
}

/// Revalidation after mutation. A replaced value that outgrew its bound is never truncated
/// ([`SerializeError::Limit`]); a key that changed or left the LINK charset (the traversal
/// hands detect-only text out mutably, a caller must not write it) is
/// [`SerializeError::Invalid`]. Entry count and key uniqueness are rechecked.
pub(in crate::protocol) fn revalidate(metadata: &Metadata) -> Result<(), SerializeError> {
    if metadata.entries.len() > MAX_METADATA_ENTRIES {
        return Err(SerializeError::Invalid);
    }
    for (index, (key, value)) in metadata.entries.iter().enumerate() {
        if !is_link(key) {
            return Err(SerializeError::Invalid);
        }
        if metadata
            .entries
            .iter()
            .take(index)
            .any(|(earlier, _)| earlier == key)
        {
            return Err(SerializeError::Invalid);
        }
        if value.len() > MAX_METADATA_VALUE_BYTES {
            return Err(SerializeError::Limit);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{ChatRequest, classify};
    use super::*;
    use crate::admission::RequestLimits;
    use crate::protocol::ProtocolError;
    use crate::protocol::json::{Budget, parse_budgeted};

    fn run(metadata: &str) -> Result<ChatRequest, ProtocolError> {
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"q"}}],"metadata":{metadata}}}"#
        );
        let limits = RequestLimits::provisional();
        let budget = Budget {
            max_depth: limits.max_depth,
            max_nodes: limits.max_nodes,
            max_string_bytes: 1 << 20,
            max_total_string_bytes: 1 << 20,
        };
        let doc = parse_budgeted(body.as_bytes(), &budget).map_err(|_| ProtocolError::Malformed)?;
        classify(doc, &limits)
    }

    fn slots(r: &ChatRequest) -> Vec<(TextSlot, String)> {
        let mut out = Vec::new();
        r.for_each_text(|s, t| out.push((s, t.to_owned())));
        out
    }

    fn many(count: usize) -> String {
        let items: Vec<String> = (0..count).map(|i| format!(r#""k{i}":"v""#)).collect();
        format!("{{{}}}", items.join(","))
    }

    #[test]
    fn entries_are_slots_in_input_order_key_then_value() {
        let r = run(r#"{"b":"2","a.b:c-d_e":"한국어 é \"q\""}"#).expect("ok");
        let got = slots(&r);
        assert_eq!(got.len(), 5);
        assert_eq!(got[1], (TextSlot::MetadataKey { entry: 0 }, "b".into()));
        assert_eq!(got[2], (TextSlot::MetadataValue { entry: 0 }, "2".into()));
        assert_eq!(got[4].1, "한국어 é \"q\"");
        assert_eq!(r.redactable_count(), 3);
    }

    #[test]
    fn empty_object_round_trips_and_absent_stays_absent() {
        let r = run("{}").expect("ok");
        let out = r.serialize_bounded(1024).expect("fits");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.ends_with(r#","metadata":{}}"#), "{text}");
        let absent = ChatRequest::for_test();
        let out = absent.serialize_bounded(1024).expect("fits");
        assert!(!String::from_utf8(out).expect("utf8").contains("metadata"));
    }

    #[test]
    fn shapes_outside_the_closed_subset_are_unsupported() {
        let long_key = format!(r#"{{"{}":"v"}}"#, "k".repeat(65));
        let long_value = format!(r#"{{"k":"{}"}}"#, "v".repeat(513));
        let seventeen = many(17);
        for bad in [
            "null",
            "[]",
            "\"s\"",
            "1",
            "true",
            r#"{"k":1}"#,
            r#"{"k":true}"#,
            r#"{"k":null}"#,
            r#"{"k":["a"]}"#,
            r#"{"k":{"a":"b"}}"#,
            r#"{"":"v"}"#,
            r#"{"bad key":"v"}"#,
            r#"{"k/x":"v"}"#,
            r#"{"ké":"v"}"#,
            r#"{"k\u0000":"v"}"#,
            r#"{"k\n":"v"}"#,
            &long_key,
            &long_value,
            &seventeen,
        ] {
            assert_eq!(run(bad).unwrap_err(), ProtocolError::Unsupported, "{bad}");
        }
        // Duplicate keys (also after escape decoding) fail in the parser.
        for dup in [r#"{"k":"a","k":"b"}"#, r#"{"k":"a","k":"b"}"#] {
            assert_eq!(run(dup).unwrap_err(), ProtocolError::Malformed, "{dup}");
        }
        // Exactly at the limits is accepted.
        let max_key = format!(r#"{{"{}":"{}"}}"#, "k".repeat(64), "v".repeat(512));
        assert!(run(&max_key).is_ok());
        assert!(run(&many(16)).is_ok());
    }

    #[test]
    fn revalidate_refuses_a_grown_value_a_changed_key_and_never_truncates() {
        let mut r = run(r#"{"k":"v"}"#).expect("ok");
        assert!(r.revalidate().is_ok());
        r.for_each_text_mut(|slot, t| {
            if matches!(slot, TextSlot::MetadataValue { .. }) {
                *t = "x".repeat(MAX_METADATA_VALUE_BYTES);
            }
        });
        assert!(r.revalidate().is_ok());
        r.for_each_text_mut(|slot, t| {
            if matches!(slot, TextSlot::MetadataValue { .. }) {
                t.push('x');
            }
        });
        assert_eq!(r.revalidate().unwrap_err(), SerializeError::Limit);
        let mut r = run(r#"{"k":"v"}"#).expect("ok");
        r.for_each_text_mut(|slot, t| {
            if matches!(slot, TextSlot::MetadataKey { .. }) {
                *t = "<SECRET_1>".into();
            }
        });
        assert_eq!(r.revalidate().unwrap_err(), SerializeError::Invalid);
    }

    #[test]
    fn debug_never_prints_keys_or_values() {
        let r = run(r#"{"SYNTHETIC_KEY":"SYNTHETIC_VALUE"}"#).expect("ok");
        let shown = format!("{:?} {r:?}", r.metadata);
        assert!(!shown.contains("SYNTHETIC"), "{shown}");
    }
}
