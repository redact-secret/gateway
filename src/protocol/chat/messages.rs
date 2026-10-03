//! Messages, roles and content (`messages[]`). Owner: #53 (tool-history roles, tool calls,
//! tool results, nullable content). Nobody else edits this file.
//!
//! Alpha 1 accepts `system`, `developer`, `user`, `assistant` with string or text-part
//! `content` and no other key. The extension points for #53 are marked `EXTENSION (#53)`.

use std::fmt;
use std::io::{self, Write};

use super::serialize::{Bounded, json_str};
use super::slots::TextSlot;
use super::tool_calls::{self, Derived, ToolCall};
use super::{Checked, MAX_CONTENT_PARTS, ProtocolError, RequestLimits, string, unsupported};
use crate::protocol::json::Json;

/// Message author role. The legacy role `function` is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    /// Tool result (`tool_call_id` plus text content).
    Tool,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// Message content in the form the caller used, so serialization can preserve the type.
pub enum Content {
    /// `"content": "text"`.
    Text(String),
    /// `"content": [{"type":"text","text":"..."}, ...]`; each entry is one part's text.
    Parts(Vec<String>),
    /// `"content": null`, accepted only on an assistant message that has `tool_calls`.
    Null,
}

impl fmt::Debug for Content {
    /// Prints only the form and sizes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => f.write_str("Content::Text"),
            Self::Parts(parts) => write!(f, "Content::Parts({})", parts.len()),
            Self::Null => f.write_str("Content::Null"),
        }
    }
}

/// One supported message: `role`, text `content`, and for tool history `tool_calls`
/// (assistant) or `tool_call_id` (tool result).
pub struct Message {
    pub(super) role: Role,
    pub(super) content: Content,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) tool_call_id: Option<String>,
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
            .field("tool_calls", &self.tool_calls.len())
            .finish()
    }
}

pub(super) fn parse_messages(
    value: Json,
    limits: &RequestLimits,
    derived: &mut Derived,
) -> Checked<Vec<Message>> {
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
    items
        .into_iter()
        .map(|item| parse_message(item, limits, derived))
        .collect()
}

fn parse_message(value: Json, limits: &RequestLimits, derived: &mut Derived) -> Checked<Message> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut role = None;
    let mut content = None;
    let mut calls = None;
    let mut call_id = None;
    for (key, value) in entries {
        match key.as_str() {
            "role" => role = Some(parse_role(value)?),
            "content" => content = Some(parse_content(value)?),
            "tool_calls" => calls = Some(tool_calls::parse_calls(value, limits, derived)?),
            "tool_call_id" => call_id = Some(tool_calls::parse_link(value)?),
            // `name`, `function_call`, `refusal`, `audio`, and any unknown key.
            _ => return Err(unsupported()),
        }
    }
    let role = role.ok_or_else(unsupported)?;
    let content = content.ok_or_else(unsupported)?;
    // Role, content and tool-history consistency (contract table); every violation is
    // `unsupported_input`, decided before inspection.
    let consistent = match role {
        Role::System | Role::Developer | Role::User => {
            !matches!(content, Content::Null) && calls.is_none() && call_id.is_none()
        }
        Role::Assistant => {
            call_id.is_none() && (calls.is_some() || !matches!(content, Content::Null))
        }
        Role::Tool => !matches!(content, Content::Null) && calls.is_none() && call_id.is_some(),
    };
    if !consistent {
        return Err(unsupported());
    }
    Ok(Message {
        role,
        content,
        tool_calls: calls.unwrap_or_default(),
        tool_call_id: call_id,
    })
}

fn parse_role(value: Json) -> Checked<Role> {
    match string(value)?.as_str() {
        "system" => Ok(Role::System),
        "developer" => Ok(Role::Developer),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
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
        // Role consistency is checked by the caller.
        Json::Null => Ok(Content::Null),
        // numbers, objects, booleans: opaque or unclassified content.
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

/// Visit every message text in order: for a tool result its `tool_call_id`, then content
/// (parts in order); otherwise content, then each tool call's slots (`id`, `name`, argument
/// keys and text leaves in document order), then the next message.
pub(super) fn visit(messages: &[Message], f: &mut impl FnMut(TextSlot, &str)) {
    for (index, message) in messages.iter().enumerate() {
        if let Some(id) = &message.tool_call_id {
            f(TextSlot::ToolResultId { message: index }, id);
        }
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
            Content::Null => {}
        }
        for (call, tool_call) in message.tool_calls.iter().enumerate() {
            tool_calls::visit_call(index, call, tool_call, f);
        }
    }
}

/// Mutable twin of [`visit`]; the order must be identical.
pub(super) fn visit_mut(messages: &mut [Message], f: &mut impl FnMut(TextSlot, &mut String)) {
    for (index, message) in messages.iter_mut().enumerate() {
        if let Some(id) = &mut message.tool_call_id {
            f(TextSlot::ToolResultId { message: index }, id);
        }
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
            Content::Null => {}
        }
        for (call, tool_call) in message.tool_calls.iter_mut().enumerate() {
            tool_calls::visit_call_mut(index, call, tool_call, f);
        }
    }
}

/// Write `,"messages":[...]` (the caller has already written `model`). Within a message:
/// `role`, `content`, then `tool_calls` or `tool_call_id`.
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
            Content::Null => w.write_all(b"null")?,
        }
        if !message.tool_calls.is_empty() {
            tool_calls::write_calls(&message.tool_calls, w)?;
        }
        if let Some(id) = &message.tool_call_id {
            w.write_all(b",\"tool_call_id\":")?;
            json_str(w, id)?;
        }
        w.write_all(b"}")?;
    }
    w.write_all(b"]")
}
