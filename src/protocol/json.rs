//! Duplicate-key-rejecting JSON path (ADR 0007, ADR 0011).
//!
//! Selected candidate: `serde_json`'s tokenizer driven by a custom `Visitor` that builds
//! an owned tree and rejects duplicate object keys after escape decoding (so `"a"` and
//! `"a"` collide). `serde_json` already rejects malformed JSON, trailing bytes,
//! invalid UTF-8, lone surrogate escapes, and nesting beyond its recursion limit.
//!
//! This is the correctness baseline only. Node/string/size budgets, memory accounting,
//! and any borrowed or SIMD parser are #18/#19 work gated on #5 measurements.

use std::collections::HashSet;
use std::fmt;

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// Owned decoded JSON value. `Debug` is manual and prints only the kind.
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    /// Insertion-ordered entries. Keys are unique by construction.
    Object(Vec<(String, Json)>),
}

impl fmt::Debug for Json {
    /// Prints only the kind, never content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Null => "Json::Null",
            Self::Bool(_) => "Json::Bool",
            Self::Number(_) => "Json::Number",
            Self::String(_) => "Json::String",
            Self::Array(_) => "Json::Array",
            Self::Object(_) => "Json::Object",
        })
    }
}

/// Safe parse failure. Never carries input text or the parser's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedJson;

impl fmt::Display for MalformedJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("malformed_input")
    }
}

impl std::error::Error for MalformedJson {}

/// Parse a complete JSON document, rejecting duplicate keys.
///
/// # Errors
/// [`MalformedJson`] for any syntax, encoding, depth, or duplicate-key violation.
pub fn parse_strict(bytes: &[u8]) -> Result<Json, MalformedJson> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = de.deserialize_any(JsonVisitor).map_err(|_| MalformedJson)?;
    de.end().map_err(|_| MalformedJson)?;
    Ok(value)
}

struct Seed;

impl<'de> de::DeserializeSeed<'de> for Seed {
    type Value = Json;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Number)
            .ok_or_else(|| E::custom("number"))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Seed)? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut entries = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                // Fixed message: never include the key.
                return Err(de::Error::custom("duplicate"));
            }
            let value = map.next_value_seed(Seed)?;
            entries.push((key, value));
        }
        Ok(Json::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_document() {
        let doc = parse_strict(br#"{"a":[1,2.5,null,true,"x"],"b":{"c":"d"}}"#).expect("valid");
        assert!(matches!(doc, Json::Object(ref e) if e.len() == 2));
    }

    #[test]
    fn rejects_duplicate_keys_including_nested_and_escaped() {
        assert!(parse_strict(br#"{"a":1,"a":2}"#).is_err());
        assert!(parse_strict(br#"{"o":{"k":1,"k":2}}"#).is_err());
        assert!(parse_strict(br#"[{"k":1,"k":2}]"#).is_err());
        // Escape-decoded collision.
        assert!(parse_strict(br#"{"a":1,"a":2}"#).is_err());
    }

    #[test]
    fn rejects_malformed_and_trailing_input() {
        assert!(parse_strict(b"{").is_err());
        assert!(parse_strict(br#"{"a":1} x"#).is_err());
        assert!(parse_strict(b"").is_err());
        assert!(parse_strict(&[b'"', 0xff, b'"']).is_err());
        assert!(parse_strict(br#""\ud800""#).is_err());
    }

    #[test]
    fn rejects_excessive_nesting_without_panicking() {
        let depth = 10_000_usize;
        let mut doc = "[".repeat(depth);
        doc.push_str(&"]".repeat(depth));
        assert!(parse_strict(doc.as_bytes()).is_err());
    }

    #[test]
    fn error_does_not_echo_input() {
        let err = parse_strict(br#"{"SYNTHETIC_KEY":1,"SYNTHETIC_KEY":2}"#).unwrap_err();
        assert!(!format!("{err} {err:?}").contains("SYNTHETIC"));
    }
}
