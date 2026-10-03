//! `input`: string, text-message items (#84) and the manual function round-trip items
//! `function_call` and `function_call_output` (#85).
//!
//! `function_call.arguments` is the only input string parsed as JSON; it goes through the
//! same strict, budgeted parser and decoded tree as Chat `function.arguments`
//! ([`ToolCall`]). `function_call_output.output` is plain text and is never parsed.
//! Correlation (unique call ids, an output after its call, each call answered once) is
//! checked at parse time and again at revalidation, before and after the core call.
//!
//! Contract: `docs/contracts/responses-request.md#input-items`.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};

use super::{Checked, MAX_INPUT_PARTS, ResponsesSlot};
use crate::admission::RequestLimits;
use crate::protocol::ProtocolError;
use crate::protocol::SerializeError;
use crate::protocol::chat::serialize::{Bounded, json_str};
use crate::protocol::chat::tool_calls::{
    CallLeaf, Derived, ToolCall, is_link, link_digest, parse_link,
};
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

/// An app-submitted `function_call` item: `call_id`, `name` and the decoded `arguments`.
pub struct FunctionCall {
    call: ToolCall,
}

impl FunctionCall {
    /// The call's `call_id` (a LINK label).
    #[must_use]
    pub fn call_id(&self) -> &str {
        self.call.id()
    }
}

impl fmt::Debug for FunctionCall {
    /// Prints nothing from the call.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FunctionCall")
    }
}

/// A `function_call_output` item: the answered `call_id` and the plain-text `output`.
pub struct FunctionCallOutput {
    call_id: String,
    /// Digest of `call_id` at parse time; a label must not change.
    link: u64,
    output: Content,
}

impl FunctionCallOutput {
    #[must_use]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    #[must_use]
    pub const fn output(&self) -> &Content {
        &self.output
    }
}

impl fmt::Debug for FunctionCallOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionCallOutput")
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

/// One `input` array item.
#[derive(Debug)]
pub enum InputItem {
    Message(Message),
    FunctionCall(FunctionCall),
    FunctionCallOutput(FunctionCallOutput),
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

pub(super) fn parse_input(
    value: Json,
    limits: &RequestLimits,
    derived: &mut Derived,
) -> Checked<Input> {
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
            let items = items
                .into_iter()
                .map(|item| parse_item(item, limits, derived))
                .collect::<Checked<Vec<_>>>()?;
            validate_history(&items)?;
            Ok(Input::Items(items))
        }
        _ => Err(unsupported()),
    }
}

/// An item is selected by `type`, or by `role` and `content` when `type` is absent. Every
/// `type` other than `message`, `function_call` and `function_call_output` is rejected.
fn parse_item(value: Json, limits: &RequestLimits, derived: &mut Derived) -> Checked<InputItem> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let kind = entries
        .iter()
        .find(|(key, _)| key == "type")
        .map(|(_, value)| value);
    match kind {
        None => parse_message(entries),
        Some(Json::String(kind)) => match kind.as_str() {
            "message" => parse_message(entries),
            "function_call" => parse_function_call(entries, limits, derived),
            "function_call_output" => parse_function_call_output(entries),
            _ => Err(unsupported()),
        },
        Some(_) => Err(unsupported()),
    }
}

fn parse_function_call(
    entries: Vec<(String, Json)>,
    limits: &RequestLimits,
    derived: &mut Derived,
) -> Checked<InputItem> {
    let mut call_id = None;
    let mut name = None;
    let mut arguments = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => {}
            "call_id" => call_id = Some(parse_link(value)?),
            "name" => name = Some(string(value)?),
            "arguments" => arguments = Some(string(value)?),
            // `id`, `status`, `caller`, `namespace`, `async`, and every other key.
            _ => return Err(unsupported()),
        }
    }
    let (Some(call_id), Some(name), Some(arguments)) = (call_id, name, arguments) else {
        return Err(unsupported());
    };
    let call = ToolCall::new(call_id, name, &arguments, limits, derived)?;
    Ok(InputItem::FunctionCall(FunctionCall { call }))
}

fn parse_function_call_output(entries: Vec<(String, Json)>) -> Checked<InputItem> {
    let mut call_id = None;
    let mut output = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => {}
            "call_id" => call_id = Some(parse_link(value)?),
            "output" => output = Some(parse_output(value)?),
            // `id`, `status`, `caller`, `name`, `namespace`, and every other key.
            _ => return Err(unsupported()),
        }
    }
    let (Some(call_id), Some(output)) = (call_id, output) else {
        return Err(unsupported());
    };
    Ok(InputItem::FunctionCallOutput(FunctionCallOutput {
        link: link_digest(Some(&call_id)),
        call_id,
        output,
    }))
}

/// A string (plain text, never parsed), or 1 to 64 `input_text` parts for every call.
fn parse_output(value: Json) -> Checked<Content> {
    match value {
        Json::String(text) => Ok(Content::Text(text)),
        Json::Array(parts) => {
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

/// Correlation before any inspection: `call_id`s of `function_call` items are unique; an
/// output answers a call that appears earlier in the array, once.
fn validate_history(items: &[InputItem]) -> Checked<()> {
    // call_id -> answered
    let mut calls: HashMap<&str, bool> = HashMap::new();
    for item in items {
        match item {
            InputItem::Message(_) => {}
            InputItem::FunctionCall(call) => {
                if calls.insert(call.call_id(), false).is_some() {
                    return Err(unsupported());
                }
            }
            InputItem::FunctionCallOutput(out) => match calls.get_mut(out.call_id.as_str()) {
                Some(answered) if !*answered => *answered = true,
                _ => return Err(unsupported()),
            },
        }
    }
    Ok(())
}

fn parse_message(entries: Vec<(String, Json)>) -> Checked<InputItem> {
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
                    InputItem::Message(m) => {
                        visit_content(&m.content, f, |part| ResponsesSlot::Message { item, part })
                    }
                    InputItem::FunctionCall(c) => {
                        c.call
                            .visit(&mut |leaf, text| f(call_slot(item, leaf), text));
                    }
                    InputItem::FunctionCallOutput(o) => {
                        f(ResponsesSlot::OutputCallId { item }, &o.call_id);
                        visit_content(&o.output, f, |part| ResponsesSlot::Output { item, part });
                    }
                }
            }
        }
    }
}

const fn call_slot(item: usize, leaf: CallLeaf) -> ResponsesSlot {
    match leaf {
        CallLeaf::Id => ResponsesSlot::CallId { item },
        CallLeaf::Name => ResponsesSlot::CallName { item },
        CallLeaf::ArgKey(leaf) => ResponsesSlot::CallArgumentKey { item, leaf },
        CallLeaf::ArgText(leaf) => ResponsesSlot::CallArgumentText { item, leaf },
    }
}

fn visit_content(
    content: &Content,
    f: &mut impl FnMut(ResponsesSlot, &str),
    slot: impl Fn(Option<usize>) -> ResponsesSlot,
) {
    match content {
        Content::Text(text) => f(slot(None), text),
        Content::Parts(parts) => {
            for (part, text) in parts.iter().enumerate() {
                f(slot(Some(part)), text);
            }
        }
    }
}

fn visit_content_mut(
    content: &mut Content,
    f: &mut impl FnMut(ResponsesSlot, &mut String),
    slot: impl Fn(Option<usize>) -> ResponsesSlot,
) {
    match content {
        Content::Text(text) => f(slot(None), text),
        Content::Parts(parts) => {
            for (part, text) in parts.iter_mut().enumerate() {
                f(slot(Some(part)), text);
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
                    InputItem::Message(m) => visit_content_mut(&mut m.content, f, |part| {
                        ResponsesSlot::Message { item, part }
                    }),
                    InputItem::FunctionCall(c) => {
                        c.call
                            .visit_mut(&mut |leaf, text| f(call_slot(item, leaf), text));
                    }
                    InputItem::FunctionCallOutput(o) => {
                        f(ResponsesSlot::OutputCallId { item }, &mut o.call_id);
                        visit_content_mut(&mut o.output, f, |part| ResponsesSlot::Output {
                            item,
                            part,
                        });
                    }
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
            for entry in items {
                match entry {
                    InputItem::Message(m) => {
                        match &m.content {
                            Content::Parts(parts)
                                if parts.is_empty() || parts.len() > MAX_INPUT_PARTS =>
                            {
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
                    InputItem::FunctionCall(c) => c.call.revalidate()?,
                    InputItem::FunctionCallOutput(o) => {
                        if !is_link(&o.call_id) || link_digest(Some(&o.call_id)) != o.link {
                            return Err(SerializeError::Invalid);
                        }
                        if matches!(&o.output, Content::Parts(parts)
                            if parts.is_empty() || parts.len() > MAX_INPUT_PARTS)
                        {
                            return Err(SerializeError::Invalid);
                        }
                    }
                }
            }
            validate_history(items).map_err(|_| SerializeError::Invalid)
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
                    InputItem::FunctionCall(c) => write_call(c, w)?,
                    InputItem::FunctionCallOutput(o) => write_output(o, w)?,
                }
            }
            w.write_all(b"]")
        }
    }
}

fn write_content(content: &Content, w: &mut Bounded) -> io::Result<()> {
    match content {
        Content::Text(text) => json_str(w, text),
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
            w.write_all(b"]")
        }
    }
}

/// `type`, `call_id`, `name`, `arguments` (the decoded tree re-encoded compactly).
fn write_call(c: &FunctionCall, w: &mut Bounded) -> io::Result<()> {
    w.write_all(b"{\"type\":\"function_call\",\"call_id\":")?;
    json_str(w, c.call.id())?;
    w.write_all(b",\"name\":")?;
    json_str(w, c.call.name())?;
    w.write_all(b",\"arguments\":")?;
    c.call.write_arguments(w)?;
    w.write_all(b"}")
}

/// `type`, `call_id`, `output`.
fn write_output(o: &FunctionCallOutput, w: &mut Bounded) -> io::Result<()> {
    w.write_all(b"{\"type\":\"function_call_output\",\"call_id\":")?;
    json_str(w, &o.call_id)?;
    w.write_all(b",\"output\":")?;
    write_content(&o.output, w)?;
    w.write_all(b"}")
}

fn write_message(m: &Message, w: &mut Bounded) -> io::Result<()> {
    w.write_all(b"{")?;
    if m.explicit_type {
        w.write_all(b"\"type\":\"message\",")?;
    }
    w.write_all(b"\"role\":\"")?;
    w.write_all(m.role.as_str().as_bytes())?;
    w.write_all(b"\",\"content\":")?;
    write_content(&m.content, w)?;
    if let Some(phase) = m.phase {
        w.write_all(b",\"phase\":\"")?;
        w.write_all(phase.as_str().as_bytes())?;
        w.write_all(b"\"")?;
    }
    w.write_all(b"}")
}
