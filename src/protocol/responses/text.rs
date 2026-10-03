//! `text`: structured-output format and verbosity (#85).
//!
//! Contract: `docs/contracts/responses-request.md#text-structured-output-85`. The Responses
//! `json_schema` format is flat (`name`, `schema`, `strict`, `description` beside `type`);
//! the Chat nested `json_schema` object is rejected, never converted. `name` is a NAME
//! label (detect-only), `description` is redacted text, `schema` is the Chat schema subset
//! with an object root charged to the request-wide derived budget. `{}` is accepted and
//! written back as `{}`.

use std::fmt;
use std::io::{self, Write};

use super::{Checked, ResponsesSlot};
use crate::protocol::chat::schema::{self, Leaf, MAX_DESCRIPTION_BYTES, Schema, is_name};
use crate::protocol::chat::serialize::{Bounded, json_bool, json_str};
use crate::protocol::chat::tool_calls::Derived;
use crate::protocol::chat::{boolean, string, unsupported};
use crate::protocol::json::Json;
use crate::protocol::{ProtocolError, SerializeError};

/// `text.verbosity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Low,
    Medium,
    High,
}

impl Verbosity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// The flat `json_schema` format.
struct JsonSchemaFormat {
    name: String,
    description: Option<String>,
    strict: Option<bool>,
    schema: Schema,
}

/// `text.format`.
enum Format {
    Text,
    JsonObject,
    JsonSchema(Box<JsonSchemaFormat>),
}

/// The accepted `text` object. `Default` is the empty object `{}`.
#[derive(Default)]
pub struct TextConfig {
    format: Option<Format>,
    verbosity: Option<Verbosity>,
}

impl fmt::Debug for TextConfig {
    /// Never prints the name, text, or schema.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TextConfig")
    }
}

impl TextConfig {
    /// The `text.format.type` written by the caller, when a format is present.
    #[must_use]
    pub const fn format_type(&self) -> Option<&'static str> {
        match &self.format {
            Some(Format::Text) => Some("text"),
            Some(Format::JsonObject) => Some("json_object"),
            Some(Format::JsonSchema(_)) => Some("json_schema"),
            None => None,
        }
    }

    #[must_use]
    pub const fn verbosity(&self) -> Option<Verbosity> {
        self.verbosity
    }
}

fn parse_json_schema(
    entries: Vec<(String, Json)>,
    derived: &mut Derived,
) -> Checked<JsonSchemaFormat> {
    let mut name = None;
    let mut description = None;
    let mut strict = None;
    let mut schema = None;
    for (key, value) in entries {
        match key.as_str() {
            // Already checked to be exactly `json_schema` by the caller.
            "type" => {}
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
            // The Chat nested `json_schema` object and everything else.
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

fn parse_format(value: Json, derived: &mut Derived) -> Checked<Format> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let kind = match entries.iter().find(|(key, _)| key == "type") {
        Some((_, Json::String(kind))) => kind.clone(),
        _ => return Err(unsupported()),
    };
    match kind.as_str() {
        "text" | "json_object" => {
            // Only `type` is allowed with these two.
            if entries.len() != 1 {
                return Err(unsupported());
            }
            Ok(if kind == "text" {
                Format::Text
            } else {
                Format::JsonObject
            })
        }
        "json_schema" => Ok(Format::JsonSchema(Box::new(parse_json_schema(
            entries, derived,
        )?))),
        _ => Err(unsupported()),
    }
}

/// The `text` value: an object with at most `format` and `verbosity`.
pub(super) fn parse(value: Json, derived: &mut Derived) -> Checked<TextConfig> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut config = TextConfig::default();
    for (key, value) in entries {
        match key.as_str() {
            "format" => config.format = Some(parse_format(value, derived)?),
            "verbosity" => {
                config.verbosity = Some(match string(value)?.as_str() {
                    "low" => Verbosity::Low,
                    "medium" => Verbosity::Medium,
                    "high" => Verbosity::High,
                    _ => return Err(unsupported()),
                });
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(config)
}

const fn slot_of(leaf: Leaf) -> ResponsesSlot {
    match leaf {
        Leaf::Label(leaf) => ResponsesSlot::FormatSchemaLabel { leaf },
        Leaf::Text(leaf) => ResponsesSlot::FormatSchemaText { leaf },
    }
}

/// Visit format texts: name, description, then schema leaves (traversal position 5).
pub(super) fn visit(config: Option<&TextConfig>, f: &mut impl FnMut(ResponsesSlot, &str)) {
    if let Some(TextConfig {
        format: Some(Format::JsonSchema(body)),
        ..
    }) = config
    {
        f(ResponsesSlot::FormatName, &body.name);
        if let Some(text) = &body.description {
            f(ResponsesSlot::FormatDescription, text);
        }
        let mut leaf = 0_usize;
        schema::visit(&body.schema, &mut leaf, &mut |kind, text| {
            f(slot_of(kind), text);
        });
    }
}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(
    config: Option<&mut TextConfig>,
    f: &mut impl FnMut(ResponsesSlot, &mut String),
) {
    if let Some(TextConfig {
        format: Some(Format::JsonSchema(body)),
        ..
    }) = config
    {
        f(ResponsesSlot::FormatName, &mut body.name);
        if let Some(text) = &mut body.description {
            f(ResponsesSlot::FormatDescription, text);
        }
        let mut leaf = 0_usize;
        schema::visit_mut(&mut body.schema, &mut leaf, &mut |kind, text| {
            f(slot_of(kind), text);
        });
    }
}

/// Write `,"text":{...}` when present: `format` (`type`, `name`, `description`, `schema`,
/// `strict` for `json_schema`), then `verbosity`.
pub(super) fn write(config: Option<&TextConfig>, w: &mut Bounded) -> io::Result<()> {
    let Some(config) = config else {
        return Ok(());
    };
    w.write_all(b",\"text\":{")?;
    let mut first = true;
    if let Some(format) = &config.format {
        first = false;
        w.write_all(b"\"format\":{\"type\":\"")?;
        match format {
            Format::Text => w.write_all(b"text\"")?,
            Format::JsonObject => w.write_all(b"json_object\"")?,
            Format::JsonSchema(body) => {
                w.write_all(b"json_schema\",\"name\":")?;
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
            }
        }
        w.write_all(b"}")?;
    }
    if let Some(verbosity) = config.verbosity {
        if !first {
            w.write_all(b",")?;
        }
        w.write_all(b"\"verbosity\":\"")?;
        w.write_all(verbosity.as_str().as_bytes())?;
        w.write_all(b"\"")?;
    }
    w.write_all(b"}")
}

/// Revalidate after mutation: name still a NAME label, description within its bound after
/// redaction, schema still well formed.
pub(super) fn revalidate(config: Option<&TextConfig>) -> Result<(), SerializeError> {
    if let Some(TextConfig {
        format: Some(Format::JsonSchema(body)),
        ..
    }) = config
    {
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
