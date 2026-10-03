//! `input` and its text-message items (#84). Owner of this file: #84 (string input and
//! text messages); #85 adds the `function_call` and `function_call_output` item forms as
//! new [`InputItem`] variants plus one arm in [`parse_item`], [`visit`], [`visit_mut`],
//! [`write`] and [`revalidate`], nothing else here changes.
//!
//! Contract: `docs/contracts/responses-request.md#input-items`.

use std::fmt;
use std::io::{self, Write};

use super::{Checked, MAX_INPUT_PARTS, ResponsesSlot};
use crate::admission::RequestLimits;
use crate::protocol::ProtocolError;
use crate::protocol::SerializeError;
use crate::protocol::chat::serialize::{Bounded, json_str};
use crate::protocol::chat::{string, unsupported};
use crate::protocol::json::Json;

/// Message author role. Provider-only roles are not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Developer,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Developer => "developer",
        }
    }
}

/// Assistant message `phase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Commentary,
    FinalAnswer,
}

impl Phase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }
}

/// Message content in the form the caller used, so serialization preserves the type.
pub enum Content {
    /// `"content": "text"`.
    Text(String),
    /// `"content": [{"type":"input_text","text":"..."}, ...]`; one text per part.
    Parts(Vec<String>),
}

impl fmt::Debug for Content {
    /// Prints only the form and sizes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => f.write_str("Content::Text"),
            Self::Parts(parts) => write!(f, "Content::Parts({})", parts.len()),
        }
    }
}

/// A text message item (`{"role":..,"content":..}` with optional `"type":"message"`).
pub struct Message {
    /// `type: "message"` was written by the caller; preserved as written.
    explicit_type: bool,
    role: Role,
    content: Content,
    phase: Option<Phase>,
}

impl Message {
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub const fn content(&self) -> &Content {
        &self.content
    }

    #[must_use]
    pub const fn phase(&self) -> Option<Phase> {
        self.phase
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("role", &self.role)
            .field("content", &self.content)
            .finish_non_exhaustive()
    }
}

/// One `input` array item. Closed: #85 adds the function-call forms.
#[derive(Debug)]
pub enum InputItem {
    Message(Message),
}

/// `input`: a string, or an ordered array of items. The caller's form is preserved.
pub enum Input {
    Text(String),
    Items(Vec<InputItem>),
}

impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => f.write_str("Input::Text"),
            Self::Items(items) => write!(f, "Input::Items({})", items.len()),
        }
    }
}

pub(super) fn parse_input(value: Json, limits: &RequestLimits) -> Checked<Input> {
    match value {
        Json::String(text) => Ok(Input::Text(text)),
        Json::Array(items) => {
            if items.is_empty() {
                return Err(unsupported());
            }
            let max = usize::try_from(limits.max_messages).unwrap_or(usize::MAX);
            if items.len() > max {
                return Err(ProtocolError::LimitExceeded);
            }
            items
                .into_iter()
                .map(parse_item)
                .collect::<Checked<Vec<_>>>()
                .map(Input::Items)
        }
        _ => Err(unsupported()),
    }
}

/// An item is selected by `type`, or by `role` and `content` when `type` is absent. Every
/// other `type` (including the ones #85 will add) is rejected until its parser exists.
fn parse_item(value: Json) -> Checked<InputItem> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut explicit_type = false;
    let mut role = None;
    let mut content = None;
    let mut phase = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => {
                if string(value)? != "message" {
                    return Err(unsupported());
                }
                explicit_type = true;
            }
            "role" => role = Some(parse_role(value)?),
            "content" => content = Some(value),
            "phase" => phase = Some(parse_phase(value)?),
            // `id`, `status`, and every other key, at any depth.
            _ => return Err(unsupported()),
        }
    }
    let role = role.ok_or_else(unsupported)?;
    let content = parse_content(content.ok_or_else(unsupported)?, role)?;
    if phase.is_some() && role != Role::Assistant {
        return Err(unsupported());
    }
    Ok(InputItem::Message(Message {
        explicit_type,
        role,
        content,
        phase,
    }))
}

fn parse_role(value: Json) -> Checked<Role> {
    match string(value)?.as_str() {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "system" => Ok(Role::System),
        "developer" => Ok(Role::Developer),
        _ => Err(unsupported()),
    }
}

fn parse_phase(value: Json) -> Checked<Phase> {
    match string(value)?.as_str() {
        "commentary" => Ok(Phase::Commentary),
        "final_answer" => Ok(Phase::FinalAnswer),
        _ => Err(unsupported()),
    }
}

/// A string for every role; a parts array for every role except `assistant` (its provider
/// parts are `output_text` with annotations, which are not accepted as input).
fn parse_content(value: Json, role: Role) -> Checked<Content> {
    match value {
        Json::String(text) => Ok(Content::Text(text)),
        Json::Array(parts) if role != Role::Assistant => {
            if parts.is_empty() {
                return Err(unsupported());
            }
            if parts.len() > MAX_INPUT_PARTS {
                return Err(ProtocolError::LimitExceeded);
            }
            parts
                .into_iter()
                .map(parse_text_part)
                .collect::<Checked<Vec<_>>>()
                .map(Content::Parts)
        }
        _ => Err(unsupported()),
    }
}

/// Exactly `{"type":"input_text","text":"<string>"}`; image, file, audio parts and every
/// extra key are rejected. Shared with #85's `function_call_output` parts.
pub(super) fn parse_text_part(value: Json) -> Checked<String> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut kind_is_text = false;
    let mut text = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => kind_is_text = string(value)? == "input_text",
            "text" => text = Some(string(value)?),
            _ => return Err(unsupported()),
        }
    }
    match (kind_is_text, text) {
        (true, Some(text)) => Ok(text),
        _ => Err(unsupported()),
    }
}

pub(super) fn visit(input: &Input, f: &mut impl FnMut(ResponsesSlot, &str)) {
    match input {
        Input::Text(text) => f(ResponsesSlot::Input, text),
        Input::Items(items) => {
            for (item, entry) in items.iter().enumerate() {
                match entry {
                    InputItem::Message(m) => match &m.content {
                        Content::Text(text) => f(ResponsesSlot::Message { item, part: None }, text),
                        Content::Parts(parts) => {
                            for (part, text) in parts.iter().enumerate() {
                                f(
                                    ResponsesSlot::Message {
                                        item,
                                        part: Some(part),
                                    },
                                    text,
                                );
                            }
                        }
                    },
                }
            }
        }
    }
}

/// Mutable twin of [`visit`]; the order must be identical.
pub(super) fn visit_mut(input: &mut Input, f: &mut impl FnMut(ResponsesSlot, &mut String)) {
    match input {
        Input::Text(text) => f(ResponsesSlot::Input, text),
        Input::Items(items) => {
            for (item, entry) in items.iter_mut().enumerate() {
                match entry {
                    InputItem::Message(m) => match &mut m.content {
                        Content::Text(text) => f(ResponsesSlot::Message { item, part: None }, text),
                        Content::Parts(parts) => {
                            for (part, text) in parts.iter_mut().enumerate() {
                                f(
                                    ResponsesSlot::Message {
                                        item,
                                        part: Some(part),
                                    },
                                    text,
                                );
                            }
                        }
                    },
                }
            }
        }
    }
}

/// Redaction replaces text only; re-check the shape bounds anyway so a structural change
/// can never be forwarded.
pub(super) fn revalidate(input: &Input) -> Result<(), SerializeError> {
    match input {
        Input::Text(_) => Ok(()),
        Input::Items(items) => {
            if items.is_empty() {
                return Err(SerializeError::Invalid);
            }
            for InputItem::Message(m) in items {
                match &m.content {
                    Content::Parts(parts) if parts.is_empty() || parts.len() > MAX_INPUT_PARTS => {
                        return Err(SerializeError::Invalid);
                    }
                    Content::Parts(_) if m.role == Role::Assistant => {
                        return Err(SerializeError::Invalid);
                    }
                    _ => {}
                }
                if m.phase.is_some() && m.role != Role::Assistant {
                    return Err(SerializeError::Invalid);
                }
            }
            Ok(())
        }
    }
}

/// Write `,"input":...` (the caller has already written what precedes it). Within a
/// message: `type` (only if the caller wrote it), `role`, `content`, `phase`.
pub(super) fn write(input: &Input, w: &mut Bounded) -> io::Result<()> {
    w.write_all(b",\"input\":")?;
    match input {
        Input::Text(text) => json_str(w, text),
        Input::Items(items) => {
            w.write_all(b"[")?;
            for (index, entry) in items.iter().enumerate() {
                if index > 0 {
                    w.write_all(b",")?;
                }
                match entry {
                    InputItem::Message(m) => write_message(m, w)?,
                }
            }
            w.write_all(b"]")
        }
    }
}

fn write_message(m: &Message, w: &mut Bounded) -> io::Result<()> {
    w.write_all(b"{")?;
    if m.explicit_type {
        w.write_all(b"\"type\":\"message\",")?;
    }
    w.write_all(b"\"role\":\"")?;
    w.write_all(m.role.as_str().as_bytes())?;
    w.write_all(b"\",\"content\":")?;
    match &m.content {
        Content::Text(text) => json_str(w, text)?,
        Content::Parts(parts) => {
            w.write_all(b"[")?;
            for (part, text) in parts.iter().enumerate() {
                if part > 0 {
                    w.write_all(b",")?;
                }
                w.write_all(b"{\"type\":\"input_text\",\"text\":")?;
                json_str(w, text)?;
                w.write_all(b"}")?;
            }
            w.write_all(b"]")?;
        }
    }
    if let Some(phase) = m.phase {
        w.write_all(b",\"phase\":\"")?;
        w.write_all(phase.as_str().as_bytes())?;
        w.write_all(b"\"")?;
    }
    w.write_all(b"}")
}
