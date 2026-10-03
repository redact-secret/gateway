//! `response_format`. Owner: #54 (adds `json_schema` with `name`/`description`/`strict`/
//! `schema`). Contract: `docs/contracts/chat-completions-request.md`, ADR 0025 D7.
//!
//! `{"type":"json_schema","json_schema":{name, description?, strict?, schema}}`: `name` is a
//! NAME label (detect-only), `description` is text (redacted in place), `strict` is a
//! preserved boolean, and `schema` is the bounded subset in `schema.rs` with an object root,
//! charged to the same request-wide derived budget as tool schemas.

use std::fmt;
use std::io::{self, Write};

use super::schema::{self, Leaf, MAX_DESCRIPTION_BYTES, Schema, is_name};
use super::serialize::{Bounded, json_bool, json_str};
use super::tool_calls::Derived;
use super::{Checked, SerializeError, TextSlot, boolean, string, unsupported};
use crate::protocol::ProtocolError;
use crate::protocol::json::Json;

/// A `json_schema` response format.
#[derive(Clone, PartialEq)]
pub struct JsonSchemaFormat {
    name: String,
    description: Option<String>,
    strict: Option<bool>,
    schema: Schema,
}

impl fmt::Debug for JsonSchemaFormat {
    /// Never prints the name, text, or schema.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JsonSchemaFormat")
    }
}

/// Supported `response_format` values.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema(Box<JsonSchemaFormat>),
}

impl ResponseFormat {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::JsonObject => "json_object",
            Self::JsonSchema(_) => "json_schema",
        }
    }
}

fn parse_json_schema(value: Json, derived: &mut Derived) -> Checked<JsonSchemaFormat> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut name = None;
    let mut description = None;
    let mut strict = None;
    let mut schema = None;
    for (key, value) in entries {
        match key.as_str() {
            "name" => {
                let text = string(value)?;
                if !is_name(&text) {
                    return Err(unsupported());
                }
                derived.charge(1, text.len())?;
                name = Some(text);
            }
            "description" => {
                let text = string(value)?;
                if text.len() > MAX_DESCRIPTION_BYTES {
                    return Err(ProtocolError::LimitExceeded);
                }
                derived.charge(1, text.len())?;
                description = Some(text);
            }
            "strict" => strict = Some(boolean(value)?),
            "schema" => schema = Some(schema::parse_root(value, derived)?),
            _ => return Err(unsupported()),
        }
    }
    Ok(JsonSchemaFormat {
        name: name.ok_or_else(unsupported)?,
        description,
        strict,
        schema: schema.ok_or_else(unsupported)?,
    })
}

pub(super) fn parse_response_format(value: Json, derived: &mut Derived) -> Checked<ResponseFormat> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut kind = None;
    let mut body = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => kind = Some(string(value)?),
            "json_schema" => body = Some(value),
            _ => return Err(unsupported()),
        }
    }
    match (kind.as_deref(), body) {
        (Some("text"), None) => Ok(ResponseFormat::Text),
        (Some("json_object"), None) => Ok(ResponseFormat::JsonObject),
        (Some("json_schema"), Some(body)) => Ok(ResponseFormat::JsonSchema(Box::new(
            parse_json_schema(body, derived)?,
        ))),
        _ => Err(unsupported()),
    }
}

/// Write `,"response_format":{...}` when present.
pub(super) fn write(format: Option<&ResponseFormat>, w: &mut Bounded) -> io::Result<()> {
    let Some(format) = format else {
        return Ok(());
    };
    w.write_all(b",\"response_format\":{\"type\":\"")?;
    w.write_all(format.as_str().as_bytes())?;
    w.write_all(b"\"")?;
    if let ResponseFormat::JsonSchema(body) = format {
        w.write_all(b",\"json_schema\":{\"name\":")?;
        json_str(w, &body.name)?;
        if let Some(text) = &body.description {
            w.write_all(b",\"description\":")?;
            json_str(w, text)?;
        }
        w.write_all(b",\"schema\":")?;
        schema::write(&body.schema, w)?;
        if let Some(strict) = body.strict {
            w.write_all(b",\"strict\":")?;
            json_bool(w, strict)?;
        }
        w.write_all(b"}")?;
    }
    w.write_all(b"}")
}

/// Visit `response_format` texts: schema name, description, then schema leaves in
/// canonical order (traversal position 6 in `slots.rs`).
pub(super) fn visit(format: Option<&ResponseFormat>, f: &mut impl FnMut(TextSlot, &str)) {
    if let Some(ResponseFormat::JsonSchema(body)) = format {
        f(TextSlot::ResponseSchemaName, &body.name);
        if let Some(text) = &body.description {
            f(TextSlot::ResponseSchemaDescription, text);
        }
        let mut leaf = 0_usize;
        schema::visit(&body.schema, &mut leaf, &mut |kind, text| match kind {
            Leaf::Label(leaf) => f(TextSlot::ResponseSchemaLabel { leaf }, text),
            Leaf::Text(leaf) => f(TextSlot::ResponseSchemaText { leaf }, text),
        });
    }
}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(
    format: Option<&mut ResponseFormat>,
    f: &mut impl FnMut(TextSlot, &mut String),
) {
    if let Some(ResponseFormat::JsonSchema(body)) = format {
        f(TextSlot::ResponseSchemaName, &mut body.name);
        if let Some(text) = &mut body.description {
            f(TextSlot::ResponseSchemaDescription, text);
        }
        let mut leaf = 0_usize;
        schema::visit_mut(&mut body.schema, &mut leaf, &mut |kind, text| match kind {
            Leaf::Label(leaf) => f(TextSlot::ResponseSchemaLabel { leaf }, text),
            Leaf::Text(leaf) => f(TextSlot::ResponseSchemaText { leaf }, text),
        });
    }
}

/// Revalidate after mutation: the name still a NAME label, the description within its bound
/// after redaction, and the schema still well formed.
pub(super) fn revalidate(format: Option<&ResponseFormat>) -> Result<(), SerializeError> {
    if let Some(ResponseFormat::JsonSchema(body)) = format {
        if !is_name(&body.name) {
            return Err(SerializeError::Invalid);
        }
        if body
            .description
            .as_ref()
            .is_some_and(|d| d.len() > MAX_DESCRIPTION_BYTES)
        {
            return Err(SerializeError::Limit);
        }
        schema::revalidate(&body.schema)?;
    }
    Ok(())
}
