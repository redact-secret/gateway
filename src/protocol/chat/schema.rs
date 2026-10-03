//! Bounded JSON-schema subset shared by `tools[].function.parameters` and
//! `response_format.json_schema.schema`. Owner: #54. Contract:
//! `docs/contracts/chat-completions-request.md` (schema subset), ADR 0025 D5, D6, D11.
//!
//! This is not a JSON Schema validator. It accepts a closed list of keywords, builds a typed
//! tree (so the serializer writes a fresh document and never splices the caller's bytes),
//! and rejects everything else: references (`$ref`, `$defs`, `$id`, `$schema`, ...) are
//! unknown keywords like any other and are never resolved or fetched, so there are no
//! unresolved, external, or recursive constructs to handle.
//!
//! Text classes (ADR 0025 D1, D2): property keys, `required` entries, and `enum`/`const`
//! strings are **labels** (constrained charset and length, scanned detect-only, never
//! rewritten); `description` is **text** (redacted in place). Numbers and booleans are
//! structural and are preserved. The traversal visits leaves in the canonical keyword order
//! below, counting labels and text together as `leaf`.
//!
//! Canonical keyword order (serializer and traversal): `type`, `description`, `properties`
//! (the caller's entry order), `items`, `required`, `enum`, `const`, `additionalProperties`,
//! `anyOf`, `minimum`, `maximum`, `minLength`, `maxLength`, `minItems`, `maxItems`.
//!
//! Bounds: depth 8 nested schema objects (the root is 1), 256 schema objects per schema, 64
//! properties, 64 `required`, 64 `enum`, 8 `anyOf`, 4 `type` entries, 4096-byte
//! descriptions, 64-byte labels, plus the request-wide [`Derived`] counters shared with the
//! other derived structures (tool-call arguments, #53).
//!
//! Numbers: integers must fit `i64`, floats must be finite, and a number is written in the
//! JSON writer's canonical form (identical value, so `1e3` is written `1000.0`). The pinned
//! `serde_json` has no `arbitrary_precision`, so an integer literal beyond `u64` is already a
//! float when it reaches this module; exact decimal text beyond `f64` is not promised (ADR
//! 0025 D10).

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};

use serde_json::Number;

use super::serialize::{Bounded, json_bool, json_str};
use super::tool_calls::Derived;
use super::{Checked, SerializeError, boolean, string, unsupported};
use crate::protocol::ProtocolError;
use crate::protocol::json::Json;

/// Most nested schema objects (the root is depth 1).
pub const MAX_SCHEMA_DEPTH: usize = 8;
/// Most schema objects in one schema.
pub const MAX_SCHEMA_OBJECTS: usize = 256;
/// Most `properties` entries in one object schema.
pub const MAX_PROPERTIES: usize = 64;
/// Most distinct `required` entries.
pub const MAX_REQUIRED: usize = 64;
/// Most `enum` entries.
pub const MAX_ENUM: usize = 64;
/// Most `anyOf` entries.
pub const MAX_ANY_OF: usize = 8;
/// Most entries in a `type` array.
pub const MAX_TYPES: usize = 4;
/// Longest `description`, in bytes (checked at parse time and again after redaction).
pub const MAX_DESCRIPTION_BYTES: usize = 4096;
/// Longest label, in bytes.
pub const MAX_LABEL_BYTES: usize = 64;

const fn limit() -> ProtocolError {
    ProtocolError::LimitExceeded
}

/// NAME: 1 to 64 bytes of `[A-Za-z0-9_-]`.
pub(super) fn is_name(text: &str) -> bool {
    (1..=MAX_LABEL_BYTES).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// ENUMTEXT: 1 to 64 bytes; Unicode alphanumerics plus ASCII space and `_ . : / + -`.
/// Control characters, quotes, backslashes, and angle brackets are not in the set.
pub(super) fn is_enum_text(text: &str) -> bool {
    (1..=MAX_LABEL_BYTES).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '.' | ':' | '/' | '+' | '-'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchemaType {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

impl SchemaType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Null => "null",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "object" => Self::Object,
            "array" => Self::Array,
            "string" => Self::String,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "boolean" => Self::Boolean,
            "null" => Self::Null,
            _ => return None,
        })
    }
}

/// `type` in the form the caller used (string or array), so the shape is preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TypeSpec {
    One(SchemaType),
    Many(Vec<SchemaType>),
}

/// One `enum`/`const` value: a label string or a structural scalar.
#[derive(Clone, PartialEq)]
enum Scalar {
    Label(String),
    Number(Number),
    Bool(bool),
    Null,
}

/// One schema object of the subset. Absent keywords are `None`.
#[derive(Clone, Default, PartialEq)]
pub struct Schema {
    types: Option<TypeSpec>,
    description: Option<String>,
    properties: Option<Vec<(String, Self)>>,
    items: Option<Box<Self>>,
    required: Option<Vec<String>>,
    enum_values: Option<Vec<Scalar>>,
    const_value: Option<Scalar>,
    additional_properties: Option<bool>,
    any_of: Option<Vec<Self>>,
    minimum: Option<Number>,
    maximum: Option<Number>,
    min_length: Option<Number>,
    max_length: Option<Number>,
    min_items: Option<Number>,
    max_items: Option<Number>,
}

impl fmt::Debug for Schema {
    /// Never prints keys, labels, or text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Schema")
    }
}

/// Which kind of leaf the traversal reached, with its ordinal (labels and text counted
/// together, in canonical order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Leaf {
    Label(usize),
    Text(usize),
}

/// A finite number whose integer form (when it is one) fits `i64`.
fn number(value: Json) -> Checked<Number> {
    match value {
        Json::Number(n) if n.is_i64() || (n.is_f64() && n.as_f64().is_some_and(f64::is_finite)) => {
            Ok(n)
        }
        _ => Err(unsupported()),
    }
}

/// A non-negative integer literal within `i64`.
fn bound(value: Json) -> Checked<Number> {
    match value {
        Json::Number(n) if n.as_i64().is_some_and(|i| i >= 0) => Ok(n),
        _ => Err(unsupported()),
    }
}

fn scalar(value: Json, derived: &mut Derived) -> Checked<Scalar> {
    derived.charge(1, 0)?;
    Ok(match value {
        Json::String(text) => {
            if !is_enum_text(&text) {
                return Err(unsupported());
            }
            derived.charge(0, text.len())?;
            Scalar::Label(text)
        }
        Json::Number(_) => Scalar::Number(number(value)?),
        Json::Bool(b) => Scalar::Bool(b),
        Json::Null => Scalar::Null,
        Json::Array(_) | Json::Object(_) => return Err(unsupported()),
    })
}

fn parse_types(value: Json) -> Checked<TypeSpec> {
    match value {
        Json::String(text) => SchemaType::parse(&text)
            .map(TypeSpec::One)
            .ok_or_else(unsupported),
        Json::Array(items) => {
            if items.is_empty() {
                return Err(unsupported());
            }
            if items.len() > MAX_TYPES {
                return Err(limit());
            }
            let mut out: Vec<SchemaType> = Vec::with_capacity(items.len());
            for item in items {
                let Json::String(text) = item else {
                    return Err(unsupported());
                };
                let parsed = SchemaType::parse(&text).ok_or_else(unsupported)?;
                if out.contains(&parsed) {
                    return Err(unsupported());
                }
                out.push(parsed);
            }
            Ok(TypeSpec::Many(out))
        }
        _ => Err(unsupported()),
    }
}

fn parse_description(value: Json, derived: &mut Derived) -> Checked<String> {
    let text = string(value)?;
    if text.len() > MAX_DESCRIPTION_BYTES {
        return Err(limit());
    }
    derived.charge(0, text.len())?;
    Ok(text)
}

/// Parse a root schema: an object schema (`"type":"object"`).
///
/// # Errors
/// [`ProtocolError::Unsupported`] for any keyword or value outside the subset;
/// [`ProtocolError::LimitExceeded`] for a depth, count, or derived-budget violation.
pub(super) fn parse_root(value: Json, derived: &mut Derived) -> Checked<Schema> {
    let mut objects = 0_usize;
    let schema = parse_schema(value, 1, &mut objects, derived)?;
    if schema.types == Some(TypeSpec::One(SchemaType::Object)) {
        Ok(schema)
    } else {
        Err(unsupported())
    }
}

fn parse_schema(
    value: Json,
    depth: usize,
    objects: &mut usize,
    derived: &mut Derived,
) -> Checked<Schema> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(limit());
    }
    *objects = objects.saturating_add(1);
    if *objects > MAX_SCHEMA_OBJECTS {
        return Err(limit());
    }
    derived.charge(1, 0)?;
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let child_depth = depth.saturating_add(1);
    let mut schema = Schema::default();
    for (key, value) in entries {
        match key.as_str() {
            "type" => schema.types = Some(parse_types(value)?),
            "description" => schema.description = Some(parse_description(value, derived)?),
            "properties" => {
                let Json::Object(props) = value else {
                    return Err(unsupported());
                };
                if props.len() > MAX_PROPERTIES {
                    return Err(limit());
                }
                let mut out = Vec::with_capacity(props.len());
                for (name, child) in props {
                    if !is_name(&name) {
                        return Err(unsupported());
                    }
                    derived.charge(1, name.len())?;
                    out.push((name, parse_schema(child, child_depth, objects, derived)?));
                }
                schema.properties = Some(out);
            }
            "items" => {
                schema.items = Some(Box::new(parse_schema(
                    value,
                    child_depth,
                    objects,
                    derived,
                )?));
            }
            "required" => {
                let Json::Array(items) = value else {
                    return Err(unsupported());
                };
                if items.len() > MAX_REQUIRED {
                    return Err(limit());
                }
                let mut seen = HashSet::with_capacity(items.len());
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let name = string(item)?;
                    if !is_name(&name) || !seen.insert(name.clone()) {
                        return Err(unsupported());
                    }
                    derived.charge(1, name.len())?;
                    out.push(name);
                }
                schema.required = Some(out);
            }
            "enum" => {
                let Json::Array(items) = value else {
                    return Err(unsupported());
                };
                if items.is_empty() {
                    return Err(unsupported());
                }
                if items.len() > MAX_ENUM {
                    return Err(limit());
                }
                schema.enum_values = Some(
                    items
                        .into_iter()
                        .map(|item| scalar(item, derived))
                        .collect::<Checked<Vec<_>>>()?,
                );
            }
            "const" => schema.const_value = Some(scalar(value, derived)?),
            "additionalProperties" => schema.additional_properties = Some(boolean(value)?),
            "anyOf" => {
                let Json::Array(items) = value else {
                    return Err(unsupported());
                };
                if items.is_empty() {
                    return Err(unsupported());
                }
                if items.len() > MAX_ANY_OF {
                    return Err(limit());
                }
                schema.any_of = Some(
                    items
                        .into_iter()
                        .map(|item| parse_schema(item, child_depth, objects, derived))
                        .collect::<Checked<Vec<_>>>()?,
                );
            }
            "minimum" => schema.minimum = Some(number(value)?),
            "maximum" => schema.maximum = Some(number(value)?),
            "minLength" => schema.min_length = Some(bound(value)?),
            "maxLength" => schema.max_length = Some(bound(value)?),
            "minItems" => schema.min_items = Some(bound(value)?),
            "maxItems" => schema.max_items = Some(bound(value)?),
            // `$ref`, `$defs`, `$id`, `$schema`, `allOf`, `oneOf`, `not`, `if`, `pattern`,
            // `format`, `default`, `examples`, `title`, and anything unknown.
            _ => return Err(unsupported()),
        }
    }
    Ok(schema)
}

fn emit(leaf: &mut usize) -> usize {
    let n = *leaf;
    *leaf = n.saturating_add(1);
    n
}

/// Visit every label and text leaf in canonical order.
pub(super) fn visit(schema: &Schema, leaf: &mut usize, f: &mut impl FnMut(Leaf, &str)) {
    if let Some(text) = &schema.description {
        f(Leaf::Text(emit(leaf)), text);
    }
    if let Some(props) = &schema.properties {
        for (key, child) in props {
            f(Leaf::Label(emit(leaf)), key);
            visit(child, leaf, f);
        }
    }
    if let Some(child) = &schema.items {
        visit(child, leaf, f);
    }
    if let Some(items) = &schema.required {
        for name in items {
            f(Leaf::Label(emit(leaf)), name);
        }
    }
    if let Some(values) = &schema.enum_values {
        for value in values {
            if let Scalar::Label(text) = value {
                f(Leaf::Label(emit(leaf)), text);
            }
        }
    }
    if let Some(Scalar::Label(text)) = &schema.const_value {
        f(Leaf::Label(emit(leaf)), text);
    }
    if let Some(children) = &schema.any_of {
        for child in children {
            visit(child, leaf, f);
        }
    }
}

/// Mutable twin of [`visit`]. Labels are handed over so the caller can scan them; a caller
/// must not write to them.
pub(super) fn visit_mut(
    schema: &mut Schema,
    leaf: &mut usize,
    f: &mut impl FnMut(Leaf, &mut String),
) {
    if let Some(text) = &mut schema.description {
        f(Leaf::Text(emit(leaf)), text);
    }
    if let Some(props) = &mut schema.properties {
        for (key, child) in props {
            f(Leaf::Label(emit(leaf)), key);
            visit_mut(child, leaf, f);
        }
    }
    if let Some(child) = &mut schema.items {
        visit_mut(child, leaf, f);
    }
    if let Some(items) = &mut schema.required {
        for name in items {
            f(Leaf::Label(emit(leaf)), name);
        }
    }
    if let Some(values) = &mut schema.enum_values {
        for value in values {
            if let Scalar::Label(text) = value {
                f(Leaf::Label(emit(leaf)), text);
            }
        }
    }
    if let Some(Scalar::Label(text)) = &mut schema.const_value {
        f(Leaf::Label(emit(leaf)), text);
    }
    if let Some(children) = &mut schema.any_of {
        for child in children {
            visit_mut(child, leaf, f);
        }
    }
}

/// Recheck after text mutation: descriptions within their bound, every label still within
/// its charset and length. A violation is a bound failure or a structural failure; nothing
/// is repaired.
pub(super) fn revalidate(schema: &Schema) -> Result<(), SerializeError> {
    if schema
        .description
        .as_ref()
        .is_some_and(|d| d.len() > MAX_DESCRIPTION_BYTES)
    {
        return Err(SerializeError::Limit);
    }
    let label_ok = |ok: bool| {
        if ok {
            Ok(())
        } else {
            Err(SerializeError::Invalid)
        }
    };
    if let Some(props) = &schema.properties {
        for (key, child) in props {
            label_ok(is_name(key))?;
            revalidate(child)?;
        }
    }
    if let Some(child) = &schema.items {
        revalidate(child)?;
    }
    if let Some(items) = &schema.required {
        for name in items {
            label_ok(is_name(name))?;
        }
    }
    for value in schema
        .enum_values
        .iter()
        .flatten()
        .chain(&schema.const_value)
    {
        if let Scalar::Label(text) = value {
            label_ok(is_enum_text(text))?;
        }
    }
    for child in schema.any_of.iter().flatten() {
        revalidate(child)?;
    }
    Ok(())
}

fn key(w: &mut Bounded, first: &mut bool, name: &str) -> io::Result<()> {
    if *first {
        *first = false;
        w.write_all(b"{")?;
    } else {
        w.write_all(b",")?;
    }
    w.write_all(b"\"")?;
    w.write_all(name.as_bytes())?;
    w.write_all(b"\":")
}

fn write_number(w: &mut Bounded, value: &Number) -> io::Result<()> {
    serde_json::to_writer(w, value).map_err(io::Error::from)
}

fn write_scalar(w: &mut Bounded, value: &Scalar) -> io::Result<()> {
    match value {
        Scalar::Label(text) => json_str(w, text),
        Scalar::Number(n) => write_number(w, n),
        Scalar::Bool(b) => json_bool(w, *b),
        Scalar::Null => w.write_all(b"null"),
    }
}

fn write_strings(w: &mut Bounded, items: &[String]) -> io::Result<()> {
    w.write_all(b"[")?;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            w.write_all(b",")?;
        }
        json_str(w, item)?;
    }
    w.write_all(b"]")
}

fn write_schemas(w: &mut Bounded, items: &[Schema]) -> io::Result<()> {
    w.write_all(b"[")?;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            w.write_all(b",")?;
        }
        write(item, w)?;
    }
    w.write_all(b"]")
}

/// Write one schema object in canonical keyword order.
pub(super) fn write(schema: &Schema, w: &mut Bounded) -> io::Result<()> {
    let mut first = true;
    if let Some(types) = &schema.types {
        key(w, &mut first, "type")?;
        match types {
            TypeSpec::One(t) => json_str(w, t.as_str())?,
            TypeSpec::Many(list) => {
                w.write_all(b"[")?;
                for (index, t) in list.iter().enumerate() {
                    if index > 0 {
                        w.write_all(b",")?;
                    }
                    json_str(w, t.as_str())?;
                }
                w.write_all(b"]")?;
            }
        }
    }
    if let Some(text) = &schema.description {
        key(w, &mut first, "description")?;
        json_str(w, text)?;
    }
    if let Some(props) = &schema.properties {
        key(w, &mut first, "properties")?;
        w.write_all(b"{")?;
        for (index, (name, child)) in props.iter().enumerate() {
            if index > 0 {
                w.write_all(b",")?;
            }
            json_str(w, name)?;
            w.write_all(b":")?;
            write(child, w)?;
        }
        w.write_all(b"}")?;
    }
    if let Some(child) = &schema.items {
        key(w, &mut first, "items")?;
        write(child, w)?;
    }
    if let Some(items) = &schema.required {
        key(w, &mut first, "required")?;
        write_strings(w, items)?;
    }
    if let Some(values) = &schema.enum_values {
        key(w, &mut first, "enum")?;
        w.write_all(b"[")?;
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                w.write_all(b",")?;
            }
            write_scalar(w, value)?;
        }
        w.write_all(b"]")?;
    }
    if let Some(value) = &schema.const_value {
        key(w, &mut first, "const")?;
        write_scalar(w, value)?;
    }
    if let Some(flag) = schema.additional_properties {
        key(w, &mut first, "additionalProperties")?;
        json_bool(w, flag)?;
    }
    if let Some(children) = &schema.any_of {
        key(w, &mut first, "anyOf")?;
        write_schemas(w, children)?;
    }
    for (name, value) in [
        ("minimum", &schema.minimum),
        ("maximum", &schema.maximum),
        ("minLength", &schema.min_length),
        ("maxLength", &schema.max_length),
        ("minItems", &schema.min_items),
        ("maxItems", &schema.max_items),
    ] {
        if let Some(n) = value {
            key(w, &mut first, name)?;
            write_number(w, n)?;
        }
    }
    if first {
        w.write_all(b"{")?;
    }
    w.write_all(b"}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_charsets_are_closed() {
        assert!(is_name("get_weather-2"));
        assert!(!is_name(""));
        assert!(!is_name("has space"));
        assert!(!is_name("점"));
        assert!(!is_name(&"a".repeat(65)));
        assert!(is_name(&"a".repeat(64)));
        assert!(is_enum_text("서울 city_1.2:x/y+z-w"));
        assert!(!is_enum_text("<tag>"));
        assert!(!is_enum_text("quote\"d"));
        assert!(!is_enum_text("back\\slash"));
        assert!(!is_enum_text("line\nbreak"));
        assert!(!is_enum_text(""));
        // 64 bytes of three-byte characters does not fit (21 chars = 63 bytes does).
        assert!(is_enum_text(&"가".repeat(21)));
        assert!(!is_enum_text(&"가".repeat(22)));
    }
}
