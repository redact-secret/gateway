//! Duplicate-key-rejecting JSON path (ADR 0007, ADR 0011).
//!
//! Selected candidate: `serde_json`'s tokenizer driven by a custom `Visitor` that builds
//! an owned tree and rejects duplicate object keys after escape decoding (so `"a"` and
//! `"a"` collide). `serde_json` already rejects malformed JSON, trailing bytes,
//! invalid UTF-8, lone surrogate escapes, and nesting beyond its recursion limit.
//!
//! [`parse_budgeted`] adds depth, node, and string budgets enforced while the tree is
//! built, so a hostile structure is rejected before it is fully allocated (#18). Memory
//! accounting for the tree is the caller's reservation (`admission::RequestLimits`). Any
//! borrowed or SIMD parser stays gated on #5 measurements and must pass the same
//! conformance table.

use std::cell::Cell;
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

/// Parse budgets enforced while the tree is built (ADR 0007: budget parsed nodes and
/// strings, not only wire bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Maximum container nesting (the root container is depth 1).
    pub max_depth: u32,
    /// Maximum values plus object keys.
    pub max_nodes: u32,
    /// Maximum decoded bytes of one string or key.
    pub max_string_bytes: usize,
    /// Maximum decoded bytes of all strings and keys together.
    pub max_total_string_bytes: usize,
}

impl Budget {
    /// No budget beyond the tokenizer's own recursion limit. For the small, trusted
    /// configuration file only; never for request bodies.
    pub const UNBOUNDED: Self = Self {
        max_depth: u32::MAX,
        max_nodes: u32::MAX,
        max_string_bytes: usize::MAX,
        max_total_string_bytes: usize::MAX,
    };
}

/// Why a budgeted parse failed. Carries no input text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Syntax, encoding, trailing bytes, or duplicate keys.
    Malformed,
    /// A [`Budget`] limit was exceeded.
    LimitExceeded,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed_input",
            Self::LimitExceeded => "limit_exceeded",
        })
    }
}

impl std::error::Error for ParseError {}

/// Parse a complete JSON document, rejecting duplicate keys.
///
/// # Errors
/// [`MalformedJson`] for any syntax, encoding, depth, or duplicate-key violation.
pub fn parse_strict(bytes: &[u8]) -> Result<Json, MalformedJson> {
    parse_budgeted(bytes, &Budget::UNBOUNDED).map_err(|_| MalformedJson)
}

/// Parse a complete JSON document with duplicate-key rejection and the given budgets.
///
/// # Errors
/// [`ParseError::LimitExceeded`] when a budget is exceeded, otherwise
/// [`ParseError::Malformed`].
pub fn parse_budgeted(bytes: &[u8], budget: &Budget) -> Result<Json, ParseError> {
    let state = State {
        budget,
        nodes: Cell::new(0),
        strings: Cell::new(0),
        limit_hit: Cell::new(false),
    };
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let outcome = de
        .deserialize_any(JsonVisitor {
            state: &state,
            depth: 1,
        })
        .and_then(|value| de.end().map(|()| value));
    match outcome {
        Ok(value) => Ok(value),
        Err(_) if state.limit_hit.get() => Err(ParseError::LimitExceeded),
        Err(_) => Err(ParseError::Malformed),
    }
}

struct State<'a> {
    budget: &'a Budget,
    nodes: Cell<u32>,
    strings: Cell<usize>,
    limit_hit: Cell<bool>,
}

impl State<'_> {
    fn limit<E: de::Error>(&self) -> E {
        self.limit_hit.set(true);
        // Fixed message: never include input.
        E::custom("limit")
    }

    fn charge_node<E: de::Error>(&self) -> Result<(), E> {
        let next = self.nodes.get().saturating_add(1);
        if next > self.budget.max_nodes {
            return Err(self.limit());
        }
        self.nodes.set(next);
        Ok(())
    }

    fn charge_string<E: de::Error>(&self, len: usize) -> Result<(), E> {
        let total = self.strings.get().saturating_add(len);
        if len > self.budget.max_string_bytes || total > self.budget.max_total_string_bytes {
            return Err(self.limit());
        }
        self.strings.set(total);
        Ok(())
    }
}

struct Seed<'a> {
    state: &'a State<'a>,
    depth: u32,
}

impl<'de> de::DeserializeSeed<'de> for Seed<'_> {
    type Value = Json;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(JsonVisitor {
            state: self.state,
            depth: self.depth,
        })
    }
}

/// Object keys are decoded, charged as a node plus string bytes, and owned.
struct KeySeed<'a> {
    state: &'a State<'a>,
}

impl<'de> de::DeserializeSeed<'de> for KeySeed<'_> {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        self.state.charge_node()?;
        deserializer.deserialize_str(KeyVisitor { state: self.state })
    }
}

struct KeyVisitor<'a> {
    state: &'a State<'a>,
}

impl Visitor<'_> for KeyVisitor<'_> {
    type Value = String;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string key")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        self.state.charge_string(v.len())?;
        Ok(v.to_owned())
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
        self.state.charge_string(v.len())?;
        Ok(v)
    }
}

struct JsonVisitor<'a> {
    state: &'a State<'a>,
    depth: u32,
}

impl JsonVisitor<'_> {
    fn leaf<E: de::Error>(&self, value: Json) -> Result<Json, E> {
        self.state.charge_node()?;
        Ok(value)
    }

    /// Charge a container and return the depth of its children.
    fn enter<E: de::Error>(&self) -> Result<u32, E> {
        if self.depth > self.state.budget.max_depth {
            return Err(self.state.limit());
        }
        self.state.charge_node()?;
        Ok(self.depth.saturating_add(1))
    }
}

impl<'de> Visitor<'de> for JsonVisitor<'_> {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        self.leaf(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        self.leaf(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        self.leaf(Json::Number(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        self.leaf(Json::Number(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        let number = serde_json::Number::from_f64(v).ok_or_else(|| E::custom("number"))?;
        self.leaf(Json::Number(number))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        self.state.charge_string(v.len())?;
        self.leaf(Json::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        self.state.charge_string(v.len())?;
        self.leaf(Json::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let child_depth = self.enter()?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(Seed {
            state: self.state,
            depth: child_depth,
        })? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let child_depth = self.enter()?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut entries = Vec::new();
        while let Some(key) = map.next_key_seed(KeySeed { state: self.state })? {
            if !seen.insert(key.clone()) {
                // Fixed message: never include the key.
                return Err(de::Error::custom("duplicate"));
            }
            let value = map.next_value_seed(Seed {
                state: self.state,
                depth: child_depth,
            })?;
            entries.push((key, value));
        }
        Ok(Json::Object(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: Budget = Budget {
        max_depth: 3,
        max_nodes: 8,
        max_string_bytes: 4,
        max_total_string_bytes: 8,
    };

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

    #[test]
    fn depth_budget_counts_containers() {
        assert!(parse_budgeted(b"[[[1]]]", &TINY).is_ok());
        assert_eq!(
            parse_budgeted(b"[[[[1]]]]", &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
        // Deep input is a limit failure, not a stack overflow.
        let deep = "[".repeat(100_000);
        assert_eq!(
            parse_budgeted(deep.as_bytes(), &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
    }

    #[test]
    fn node_budget_counts_values_and_keys() {
        // array + 7 numbers = 8 nodes.
        assert!(parse_budgeted(b"[1,2,3,4,5,6,7]", &TINY).is_ok());
        assert_eq!(
            parse_budgeted(b"[1,2,3,4,5,6,7,8]", &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
        // object + 3 keys + 3 values = 7 nodes; a fourth pair exceeds 8.
        assert!(parse_budgeted(br#"{"a":1,"b":2,"c":3}"#, &TINY).is_ok());
        assert_eq!(
            parse_budgeted(br#"{"a":1,"b":2,"c":3,"d":4}"#, &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
    }

    #[test]
    fn string_budgets_apply_to_decoded_bytes_of_values_and_keys() {
        assert!(parse_budgeted(br#"["abcd"]"#, &TINY).is_ok());
        assert_eq!(
            parse_budgeted(br#"["abcde"]"#, &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
        // Total across strings: 3 x 3 bytes exceeds 8.
        assert_eq!(
            parse_budgeted(br#"["abc","abc","abc"]"#, &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
        // A long key is charged too.
        assert_eq!(
            parse_budgeted(br#"{"abcde":1}"#, &TINY).unwrap_err(),
            ParseError::LimitExceeded
        );
        // Budgets apply after escape decoding: six escaped bytes decode to one.
        assert!(parse_budgeted(br#"["AAAA"]"#, &TINY).is_ok());
    }

    #[test]
    fn malformed_stays_malformed_under_a_budget() {
        assert_eq!(
            parse_budgeted(br#"{"a":1,"a":2}"#, &TINY).unwrap_err(),
            ParseError::Malformed
        );
        assert_eq!(
            parse_budgeted(b"[1,", &TINY).unwrap_err(),
            ParseError::Malformed
        );
    }

    #[test]
    fn budget_errors_do_not_echo_input() {
        let doc = br#"["SYNTHETIC_TOO_LONG_STRING"]"#;
        let err = parse_budgeted(doc, &TINY).unwrap_err();
        assert!(!format!("{err} {err:?}").contains("SYNTHETIC"));
    }
}
