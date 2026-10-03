//! `response_format`. Owner: #54 (adds `json_schema` with `name`/`strict`/`schema`).

use std::io::{self, Write};

use super::SerializeError;
use super::TextSlot;
use super::serialize::Bounded;
use super::{Checked, string, unsupported};
use crate::protocol::json::Json;

/// Supported `response_format` values (`json_schema` is rejected until #54).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseFormat {
    Text,
    JsonObject,
}

impl ResponseFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::JsonObject => "json_object",
        }
    }
}

pub(super) fn parse_response_format(value: Json) -> Checked<ResponseFormat> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut format = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => {
                format = Some(match string(value)?.as_str() {
                    "text" => ResponseFormat::Text,
                    "json_object" => ResponseFormat::JsonObject,
                    _ => return Err(unsupported()),
                });
            }
            _ => return Err(unsupported()),
        }
    }
    format.ok_or_else(unsupported)
}

/// Write `,"response_format":{...}` when present. EXTENSION (#54): the `json_schema` form.
pub(super) fn write(format: Option<ResponseFormat>, w: &mut Bounded) -> io::Result<()> {
    if let Some(format) = format {
        w.write_all(b",\"response_format\":{\"type\":\"")?;
        w.write_all(format.as_str().as_bytes())?;
        w.write_all(b"\"}")?;
    }
    Ok(())
}

/// Visit `response_format` texts. EXTENSION (#54): schema name, description, labels and
/// text, in the order fixed by `slots.rs`. Today the accepted forms carry no text.
pub(super) fn visit(_format: Option<&ResponseFormat>, _f: &mut impl FnMut(TextSlot, &str)) {}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(
    _format: Option<&mut ResponseFormat>,
    _f: &mut impl FnMut(TextSlot, &mut String),
) {
}

/// Revalidate after mutation. EXTENSION (#54).
pub(super) fn revalidate(_format: Option<&ResponseFormat>) -> Result<(), SerializeError> {
    Ok(())
}
