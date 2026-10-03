//! Messages, roles and content (`messages[]`). Owner: #53 (tool-history roles, tool calls,
//! tool results, nullable content). Nobody else edits this file.
//!
//! Alpha 1 accepts `system`, `developer`, `user`, `assistant` with string or text-part
//! `content` and no other key. The extension points for #53 are marked `EXTENSION (#53)`.

use std::fmt;
use std::io::{self, Write};

use super::serialize::{Bounded, json_str};
use super::slots::TextSlot;
use super::tool_calls;
use super::{Checked, MAX_CONTENT_PARTS, ProtocolError, RequestLimits, string, unsupported};
use crate::protocol::json::Json;

/// Message author role. `tool` and `function` are not supported yet (planned: #53).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Message content in the form the caller used, so serialization can preserve the type.
pub enum Content {
    /// `"content": "text"`.
    Text(String),
    /// `"content": [{"type":"text","text":"..."}, ...]`; each entry is one part's text.
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

/// One supported message: `role` plus text `content` and nothing else.
pub struct Message {
    pub(super) role: Role,
    pub(super) content: Content,
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
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("role", &self.role)
            .field("content", &self.content)
            .finish()
    }
}

pub(super) fn parse_messages(value: Json, limits: &RequestLimits) -> Checked<Vec<Message>> {
    let Json::Array(items) = value else {
        return Err(unsupported());
    };
    if items.is_empty() {
        return Err(unsupported());
    }
    let max = usize::try_from(limits.max_messages).unwrap_or(usize::MAX);
    if items.len() > max {
        return Err(ProtocolError::LimitExceeded);
    }
    items.into_iter().map(parse_message).collect()
}

fn parse_message(value: Json) -> Checked<Message> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut role = None;
    let mut content = None;
    for (key, value) in entries {
        match key.as_str() {
            "role" => role = Some(parse_role(value)?),
            "content" => content = Some(parse_content(value)?),
            // EXTENSION (#53): the tool-history keys are routed here and rejected until
            // #53 lands.
            "tool_calls" | "tool_call_id" => tool_calls::parse_message_field(&key, value)?,
            // `name`, `function_call`, `refusal`, `audio`, and any unknown key.
            _ => return Err(unsupported()),
        }
    }
    Ok(Message {
        role: role.ok_or_else(unsupported)?,
        content: content.ok_or_else(unsupported)?,
    })
}

fn parse_role(value: Json) -> Checked<Role> {
    match string(value)?.as_str() {
        "system" => Ok(Role::System),
        "developer" => Ok(Role::Developer),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        _ => Err(unsupported()),
    }
}

fn parse_content(value: Json) -> Checked<Content> {
    match value {
        Json::String(text) => Ok(Content::Text(text)),
        Json::Array(parts) => {
            if parts.is_empty() {
                return Err(unsupported());
            }
            if parts.len() > MAX_CONTENT_PARTS {
                return Err(ProtocolError::LimitExceeded);
            }
            parts
                .into_iter()
                .map(parse_text_part)
                .collect::<Checked<Vec<_>>>()
                .map(Content::Parts)
        }
        // null, numbers, objects, booleans: opaque or unclassified content.
        _ => Err(unsupported()),
    }
}

/// `{"type":"text","text":"..."}` and nothing else. Image, audio, file, and refusal
/// parts, and any extra key, are rejected.
fn parse_text_part(value: Json) -> Checked<String> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut kind_is_text = false;
    let mut text = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => kind_is_text = string(value)? == "text",
            "text" => text = Some(string(value)?),
            _ => return Err(unsupported()),
        }
    }
    match (kind_is_text, text) {
        (true, Some(text)) => Ok(text),
        _ => Err(unsupported()),
    }
}

/// Visit every message text in order (parts in order). EXTENSION (#53): after a message's
/// content, its tool-call slots (`id`, `name`, argument keys, argument text) are visited in
/// the order fixed by the contract, then the next message.
pub(super) fn visit(messages: &[Message], f: &mut impl FnMut(TextSlot, &str)) {
    for (index, message) in messages.iter().enumerate() {
        match &message.content {
            Content::Text(text) => f(TextSlot::Message { index, part: None }, text),
            Content::Parts(parts) => {
                for (part, text) in parts.iter().enumerate() {
                    f(
                        TextSlot::Message {
                            index,
                            part: Some(part),
                        },
                        text,
                    );
                }
            }
        }
    }
}

/// Mutable twin of [`visit`]; the order must be identical.
pub(super) fn visit_mut(messages: &mut [Message], f: &mut impl FnMut(TextSlot, &mut String)) {
    for (index, message) in messages.iter_mut().enumerate() {
        match &mut message.content {
            Content::Text(text) => f(TextSlot::Message { index, part: None }, text),
            Content::Parts(parts) => {
                for (part, text) in parts.iter_mut().enumerate() {
                    f(
                        TextSlot::Message {
                            index,
                            part: Some(part),
                        },
                        text,
                    );
                }
            }
        }
    }
}

/// Write `,"messages":[...]` (the caller has already written `model`).
pub(super) fn write(messages: &[Message], w: &mut Bounded) -> io::Result<()> {
    w.write_all(b",\"messages\":[")?;
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            w.write_all(b",")?;
        }
        w.write_all(b"{\"role\":\"")?;
        w.write_all(message.role.as_str().as_bytes())?;
        w.write_all(b"\",\"content\":")?;
        match &message.content {
            Content::Text(text) => json_str(w, text)?,
            Content::Parts(parts) => {
                w.write_all(b"[")?;
                for (part, text) in parts.iter().enumerate() {
                    if part > 0 {
                        w.write_all(b",")?;
                    }
                    w.write_all(b"{\"type\":\"text\",\"text\":")?;
                    json_str(w, text)?;
                    w.write_all(b"}")?;
                }
                w.write_all(b"]")?;
            }
        }
        w.write_all(b"}")?;
    }
    w.write_all(b"]")
}
