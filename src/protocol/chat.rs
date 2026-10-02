//! `POST /v1/chat/completions` text-only request matrix (#18).
//!
//! Normative table: `docs/contracts/chat-completions-request.md`. This module and that
//! document must agree. Every field is exactly one of: inspected application text,
//! validated structural/control data, or rejected. Unknown fields are rejected at every
//! depth. The classifier consumes the parsed tree and moves its strings into the typed
//! [`ChatRequest`], so there is one working structure and no body-sized clone (ADR 0007).
//!
//! Errors are fixed [`ProtocolError`] codes. Which field failed is deliberately not
//! reported: field names and values are caller payload.

use std::fmt;
use std::io::{self, Write};

use serde_json::Number;

use super::ProtocolError;
use super::json::Json;
use crate::admission::RequestLimits;

/// Longest accepted `model` identifier.
pub const MAX_MODEL_BYTES: usize = 128;
/// Most parts in one array `content`.
pub const MAX_CONTENT_PARTS: usize = 64;
/// Most `stop` sequences (the provider's own limit).
pub const MAX_STOP_SEQUENCES: usize = 4;
/// Longest accepted `user` string.
pub const MAX_USER_BYTES: usize = 256;
/// Largest accepted value for the token-count fields.
pub const MAX_TOKEN_COUNT: i64 = 2_147_483_647;

type Checked<T> = Result<T, ProtocolError>;

const fn unsupported() -> ProtocolError {
    ProtocolError::Unsupported
}

/// Message author role. `tool` and `function` are not supported (Alpha 2, #9).
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
    role: Role,
    content: Content,
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

/// `stop`: one string or up to [`MAX_STOP_SEQUENCES`] strings. Inspected text.
pub enum Stop {
    One(String),
    Many(Vec<String>),
}

impl fmt::Debug for Stop {
    /// Prints only the form and sizes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(_) => f.write_str("Stop::One"),
            Self::Many(items) => write!(f, "Stop::Many({})", items.len()),
        }
    }
}

/// Supported `response_format` values (`json_schema` is rejected: schema text).
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

/// `stream_options`: only `include_usage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    pub include_usage: Option<bool>,
}

/// Validated numeric/control parameters. Each present value passed its range check; the
/// original JSON number literal is kept.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params {
    pub temperature: Option<Number>,
    pub top_p: Option<Number>,
    pub max_tokens: Option<Number>,
    pub max_completion_tokens: Option<Number>,
    pub presence_penalty: Option<Number>,
    pub frequency_penalty: Option<Number>,
    /// Only the value `1` is accepted.
    pub n: Option<Number>,
    pub seed: Option<Number>,
}

/// Where an inspected text lives, in deterministic traversal order: messages in order
/// (parts in order), then `stop`, then `user`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSlot {
    Message { index: usize, part: Option<usize> },
    Stop { index: usize },
    User,
}

/// The typed boundary representation of a supported Chat Completions request.
///
/// Fields are private. Inspected text is reachable only through [`Self::for_each_text`]
/// and [`Self::for_each_text_mut`], so a later stage can replace text in place but cannot
/// add fields, change roles, or alter structure.
pub struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    params: Params,
    stream: Option<bool>,
    stream_options: Option<StreamOptions>,
    stop: Option<Stop>,
    user: Option<String>,
    response_format: Option<ResponseFormat>,
}

impl ChatRequest {
    /// Validated structural identifier (see the contract; transmitted verbatim).
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    #[must_use]
    pub const fn params(&self) -> &Params {
        &self.params
    }

    #[must_use]
    pub const fn stream(&self) -> Option<bool> {
        self.stream
    }

    #[must_use]
    pub const fn stream_options(&self) -> Option<StreamOptions> {
        self.stream_options
    }

    #[must_use]
    pub const fn response_format(&self) -> Option<ResponseFormat> {
        self.response_format
    }

    /// Visit every inspected text in traversal order.
    pub fn for_each_text(&self, mut f: impl FnMut(TextSlot, &str)) {
        for (index, message) in self.messages.iter().enumerate() {
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
        match &self.stop {
            Some(Stop::One(text)) => f(TextSlot::Stop { index: 0 }, text),
            Some(Stop::Many(items)) => {
                for (index, text) in items.iter().enumerate() {
                    f(TextSlot::Stop { index }, text);
                }
            }
            None => {}
        }
        if let Some(text) = &self.user {
            f(TextSlot::User, text);
        }
    }

    /// Visit every inspected text mutably, in the same order as [`Self::for_each_text`].
    pub fn for_each_text_mut(&mut self, mut f: impl FnMut(TextSlot, &mut String)) {
        for (index, message) in self.messages.iter_mut().enumerate() {
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
        match &mut self.stop {
            Some(Stop::One(text)) => f(TextSlot::Stop { index: 0 }, text),
            Some(Stop::Many(items)) => {
                for (index, text) in items.iter_mut().enumerate() {
                    f(TextSlot::Stop { index }, text);
                }
            }
            None => {}
        }
        if let Some(text) = &mut self.user {
            f(TextSlot::User, text);
        }
    }

    /// Number of inspected texts.
    #[must_use]
    pub fn text_count(&self) -> usize {
        let mut count = 0_usize;
        self.for_each_text(|_, _| count = count.saturating_add(1));
        count
    }

    /// Serialize a fresh JSON document from the typed request (never from the original
    /// bytes): canonical key order, the same keys, types, and array order, with every
    /// string escaped by the JSON writer. The output is bounded while it is produced, so an
    /// oversized result is refused before it is fully allocated.
    ///
    /// # Errors
    /// [`SerializeError::Limit`] when the output would exceed `max_bytes`;
    /// [`SerializeError::Invalid`] for any other writer failure (not reachable for the
    /// supported matrix, but never ignored).
    pub fn serialize_bounded(&self, max_bytes: usize) -> Result<Vec<u8>, SerializeError> {
        let mut estimate = 256_usize;
        self.for_each_text(|_, text| estimate = estimate.saturating_add(text.len()));
        let mut out = Bounded {
            buf: Vec::with_capacity(estimate.min(max_bytes)),
            max: max_bytes,
            overflow: false,
        };
        match self.write_to(&mut out) {
            Ok(()) => Ok(out.buf),
            Err(_) if out.overflow => Err(SerializeError::Limit),
            Err(_) => Err(SerializeError::Invalid),
        }
    }

    fn write_to(&self, w: &mut Bounded) -> io::Result<()> {
        w.write_all(b"{\"model\":")?;
        json_str(w, &self.model)?;
        w.write_all(b",\"messages\":[")?;
        for (index, message) in self.messages.iter().enumerate() {
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
        w.write_all(b"]")?;
        if let Some(stream) = self.stream {
            w.write_all(b",\"stream\":")?;
            json_bool(w, stream)?;
        }
        if let Some(options) = self.stream_options {
            w.write_all(b",\"stream_options\":{")?;
            if let Some(usage) = options.include_usage {
                w.write_all(b"\"include_usage\":")?;
                json_bool(w, usage)?;
            }
            w.write_all(b"}")?;
        }
        let params = &self.params;
        for (key, value) in [
            ("temperature", &params.temperature),
            ("top_p", &params.top_p),
            ("max_tokens", &params.max_tokens),
            ("max_completion_tokens", &params.max_completion_tokens),
            ("presence_penalty", &params.presence_penalty),
            ("frequency_penalty", &params.frequency_penalty),
            ("n", &params.n),
            ("seed", &params.seed),
        ] {
            if let Some(number) = value {
                w.write_all(b",\"")?;
                w.write_all(key.as_bytes())?;
                w.write_all(b"\":")?;
                serde_json::to_writer(&mut *w, number).map_err(io::Error::from)?;
            }
        }
        match &self.stop {
            Some(Stop::One(text)) => {
                w.write_all(b",\"stop\":")?;
                json_str(w, text)?;
            }
            Some(Stop::Many(items)) => {
                w.write_all(b",\"stop\":[")?;
                for (index, text) in items.iter().enumerate() {
                    if index > 0 {
                        w.write_all(b",")?;
                    }
                    json_str(w, text)?;
                }
                w.write_all(b"]")?;
            }
            None => {}
        }
        if let Some(user) = &self.user {
            w.write_all(b",\"user\":")?;
            json_str(w, user)?;
        }
        if let Some(format) = self.response_format {
            w.write_all(b",\"response_format\":{\"type\":\"")?;
            w.write_all(format.as_str().as_bytes())?;
            w.write_all(b"\"}")?;
        }
        w.write_all(b"}")
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            model: "synthetic-model".to_owned(),
            messages: Vec::new(),
            params: Params::default(),
            stream: None,
            stream_options: None,
            stop: None,
            user: None,
            response_format: None,
        }
    }
}

impl fmt::Debug for ChatRequest {
    /// Never prints model, text, or numeric content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatRequest")
            .field("messages", &self.messages.len())
            .field("texts", &self.text_count())
            .finish_non_exhaustive()
    }
}

/// Why serialization of the transformed request failed. Carries nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerializeError {
    /// The output would exceed the transformed-output bound.
    Limit,
    /// The writer failed for another reason.
    Invalid,
}

/// Output sink that refuses to grow past its bound.
struct Bounded {
    buf: Vec<u8>,
    max: usize,
    overflow: bool,
}

impl Write for Bounded {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len().saturating_add(data.len()) > self.max {
            self.overflow = true;
            return Err(io::Error::other("output bound"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn json_str(w: &mut Bounded, text: &str) -> io::Result<()> {
    serde_json::to_writer(w, text).map_err(io::Error::from)
}

fn json_bool(w: &mut Bounded, value: bool) -> io::Result<()> {
    w.write_all(if value { b"true" } else { b"false" })
}

/// Classify a parsed document against the matrix, consuming it.
///
/// # Errors
/// [`ProtocolError::Unsupported`] for any unknown field, wrong type, out-of-contract
/// value, or unsupported form; [`ProtocolError::LimitExceeded`] for a message-count
/// limit.
pub(super) fn classify(document: Json, limits: &RequestLimits) -> Checked<ChatRequest> {
    let Json::Object(entries) = document else {
        return Err(unsupported());
    };
    let mut model = None;
    let mut messages = None;
    let mut params = Params::default();
    let mut stream = None;
    let mut stream_options = None;
    let mut stop = None;
    let mut user = None;
    let mut response_format = None;
    for (key, value) in entries {
        match key.as_str() {
            "model" => model = Some(parse_model(value)?),
            "messages" => messages = Some(parse_messages(value, limits)?),
            "stream" => stream = Some(boolean(value)?),
            "stream_options" => stream_options = Some(parse_stream_options(value)?),
            "temperature" => params.temperature = Some(float_in(value, 0.0, 2.0)?),
            "top_p" => params.top_p = Some(float_in(value, 0.0, 1.0)?),
            "presence_penalty" => params.presence_penalty = Some(float_in(value, -2.0, 2.0)?),
            "frequency_penalty" => params.frequency_penalty = Some(float_in(value, -2.0, 2.0)?),
            "max_tokens" => params.max_tokens = Some(int_in(value, 1, MAX_TOKEN_COUNT)?),
            "max_completion_tokens" => {
                params.max_completion_tokens = Some(int_in(value, 1, MAX_TOKEN_COUNT)?);
            }
            "n" => params.n = Some(int_in(value, 1, 1)?),
            "seed" => params.seed = Some(int_in(value, i64::MIN, i64::MAX)?),
            "stop" => stop = Some(parse_stop(value)?),
            "user" => user = Some(parse_user(value)?),
            "response_format" => response_format = Some(parse_response_format(value)?),
            // Everything else, including tools, functions, metadata, logit_bias,
            // audio/modalities, and any unknown key, is rejected.
            _ => return Err(unsupported()),
        }
    }
    Ok(ChatRequest {
        model: model.ok_or_else(unsupported)?,
        messages: messages.ok_or_else(unsupported)?,
        params,
        stream,
        stream_options,
        stop,
        user,
        response_format,
    })
}

fn boolean(value: Json) -> Checked<bool> {
    match value {
        Json::Bool(b) => Ok(b),
        _ => Err(unsupported()),
    }
}

fn string(value: Json) -> Checked<String> {
    match value {
        Json::String(s) => Ok(s),
        _ => Err(unsupported()),
    }
}

/// A JSON number that is finite and within `lo..=hi` (integers are accepted).
fn float_in(value: Json, lo: f64, hi: f64) -> Checked<Number> {
    let Json::Number(number) = value else {
        return Err(unsupported());
    };
    match number.as_f64() {
        Some(f) if f.is_finite() && f >= lo && f <= hi => Ok(number),
        _ => Err(unsupported()),
    }
}

/// A JSON integer literal within `lo..=hi`. Floating literals (even `1.0`) and integers
/// beyond `i64` are rejected.
fn int_in(value: Json, lo: i64, hi: i64) -> Checked<Number> {
    let Json::Number(number) = value else {
        return Err(unsupported());
    };
    match number.as_i64() {
        Some(i) if i >= lo && i <= hi => Ok(number),
        _ => Err(unsupported()),
    }
}

fn parse_model(value: Json) -> Checked<String> {
    let model = string(value)?;
    let well_formed = !model.is_empty()
        && model.len() <= MAX_MODEL_BYTES
        && model.chars().enumerate().all(|(i, c)| {
            c.is_ascii_alphanumeric()
                || (i > 0 && matches!(c, '.' | '_' | ':' | '/' | '@' | '+' | '-'))
        });
    if well_formed {
        Ok(model)
    } else {
        Err(unsupported())
    }
}

fn parse_user(value: Json) -> Checked<String> {
    let user = string(value)?;
    if user.len() <= MAX_USER_BYTES {
        Ok(user)
    } else {
        Err(unsupported())
    }
}

fn parse_stop(value: Json) -> Checked<Stop> {
    match value {
        Json::String(s) => Ok(Stop::One(s)),
        Json::Array(items) if !items.is_empty() && items.len() <= MAX_STOP_SEQUENCES => items
            .into_iter()
            .map(string)
            .collect::<Checked<Vec<_>>>()
            .map(Stop::Many),
        _ => Err(unsupported()),
    }
}

fn parse_response_format(value: Json) -> Checked<ResponseFormat> {
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

fn parse_stream_options(value: Json) -> Checked<StreamOptions> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut include_usage = None;
    for (key, value) in entries {
        match key.as_str() {
            "include_usage" => include_usage = Some(boolean(value)?),
            _ => return Err(unsupported()),
        }
    }
    Ok(StreamOptions { include_usage })
}

fn parse_messages(value: Json, limits: &RequestLimits) -> Checked<Vec<Message>> {
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
            // `name`, `tool_calls`, `tool_call_id`, `function_call`, `refusal`, `audio`,
            // and any unknown key.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::json::{Budget, parse_budgeted};

    fn run(body: &str) -> Checked<ChatRequest> {
        let limits = RequestLimits::provisional();
        let budget = Budget {
            max_depth: limits.max_depth,
            max_nodes: limits.max_nodes,
            max_string_bytes: 1 << 20,
            max_total_string_bytes: 1 << 20,
        };
        let doc = parse_budgeted(body.as_bytes(), &budget).map_err(|_| ProtocolError::Malformed)?;
        classify(doc, &limits)
    }

    fn texts(request: &ChatRequest) -> Vec<(TextSlot, String)> {
        let mut out = Vec::new();
        request.for_each_text(|slot, text| out.push((slot, text.to_owned())));
        out
    }

    #[test]
    fn minimal_request_parses_into_the_typed_contract() {
        let r = run(r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}"#)
            .expect("supported");
        assert_eq!(r.model(), "gpt-4o-mini");
        assert_eq!(r.messages().len(), 1);
        assert_eq!(r.messages()[0].role(), Role::User);
        assert_eq!(
            texts(&r),
            vec![(
                TextSlot::Message {
                    index: 0,
                    part: None
                },
                "hi".to_owned()
            )]
        );
        assert_eq!(r.stream(), None);
        assert_eq!(r.params(), &Params::default());
    }

    #[test]
    fn every_allowed_field_is_accepted_and_typed() {
        let r = run(r#"{"model":"ft:gpt-4o:org:name:id","stream":true,
                "stream_options":{"include_usage":true},
                "messages":[
                  {"role":"system","content":"s"},
                  {"role":"developer","content":"d"},
                  {"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]},
                  {"role":"assistant","content":"r"}],
                "temperature":0.7,"top_p":1,"presence_penalty":-2,"frequency_penalty":2.0,
                "max_tokens":16,"max_completion_tokens":32,"n":1,"seed":-5,
                "stop":["x","y"],"user":"u-1","response_format":{"type":"json_object"}}"#)
        .expect("supported");
        assert_eq!(r.stream(), Some(true));
        assert_eq!(
            r.stream_options(),
            Some(StreamOptions {
                include_usage: Some(true)
            })
        );
        assert_eq!(r.response_format(), Some(ResponseFormat::JsonObject));
        assert_eq!(r.params().seed.as_ref().and_then(Number::as_i64), Some(-5));
        let t: Vec<String> = texts(&r).into_iter().map(|(_, s)| s).collect();
        assert_eq!(t, ["s", "d", "a", "b", "r", "x", "y", "u-1"]);
        assert_eq!(r.text_count(), 8);
    }

    #[test]
    fn stop_may_be_a_single_string() {
        let r = run(r#"{"model":"m","messages":[{"role":"user","content":""}],"stop":"END"}"#)
            .expect("supported");
        assert_eq!(
            texts(&r).last().map(|(s, _)| *s),
            Some(TextSlot::Stop { index: 0 })
        );
    }

    #[test]
    fn text_can_be_replaced_in_place_without_changing_structure() {
        let mut r = run(
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"a"}]}],"user":"u"}"#,
        )
        .expect("supported");
        r.for_each_text_mut(|_, t| t.push('!'));
        let t: Vec<String> = texts(&r).into_iter().map(|(_, s)| s).collect();
        assert_eq!(t, ["a!", "u!"]);
        assert!(matches!(r.messages()[0].content(), Content::Parts(p) if p.len() == 1));
    }

    #[test]
    fn rejected_forms_are_unsupported() {
        let ok_messages = r#""messages":[{"role":"user","content":"x"}]"#;
        let cases = [
            // top level
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"unknown":1}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"tools":[]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"tool_choice":"none"}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"functions":[]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"function_call":"none"}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"metadata":{}}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"logit_bias":{}}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"audio":{}}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"modalities":["text"]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"store":true}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"n":2}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"n":1.0}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_schema"}}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"response_format":{"type":"text","x":1}}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stream_options":{"x":1}}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stream":"true"}"#,
            // messages
            r#"{"model":"m","messages":[]}"#,
            r#"{"model":"m","messages":{}}"#,
            r#"{"model":"m","messages":[{"role":"tool","content":"x"}]}"#,
            r#"{"model":"m","messages":[{"role":"function","content":"x"}]}"#,
            r#"{"model":"m","messages":[{"role":"USER","content":"x"}]}"#,
            r#"{"model":"m","messages":[{"role":"user"}]}"#,
            r#"{"model":"m","messages":[{"content":"x"}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":null}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":7}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":{"type":"text","text":"x"}}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x","name":"n"}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":"x","extra":1}]}"#,
            r#"{"model":"m","messages":[{"role":"assistant","content":"x","tool_calls":[]}]}"#,
            r#"{"model":"m","messages":[{"role":"assistant","content":"x","refusal":null}]}"#,
            r#"{"model":"m","messages":[{"role":"tool","content":"x","tool_call_id":"c"}]}"#,
            // parts
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"u"}}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{}}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"file","file":{}}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"refusal","refusal":"x"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"x","cache":1}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"text":"x"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":[{"type":"TEXT","text":"x"}]}]}"#,
            r#"{"model":"m","messages":[{"role":"user","content":["x"]}]}"#,
            // required and typed
            r#"{"messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"m"}"#,
            r#"{"model":7,"messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"","messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"-lead","messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"has space","messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"quote\"d","messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"m\u0000","messages":[{"role":"user","content":"x"}]}"#,
            r#"{"model":"모델","messages":[{"role":"user","content":"x"}]}"#,
            r#"[]"#,
            r#""text""#,
            r#"null"#,
            r#"7"#,
            r#"{}"#,
        ];
        for body in cases {
            assert_eq!(run(body).unwrap_err(), ProtocolError::Unsupported, "{body}");
        }
        // The same field in a valid shell, to prove the shell itself is fine.
        assert!(run(&format!(r#"{{"model":"m",{ok_messages}}}"#)).is_ok());
    }

    #[test]
    fn numeric_ranges_and_types_are_enforced() {
        let shell = |field: &str| {
            format!(r#"{{"model":"m","messages":[{{"role":"user","content":"x"}}],{field}}}"#)
        };
        for good in [
            r#""temperature":0"#,
            r#""temperature":2"#,
            r#""temperature":-0.0"#,
            r#""top_p":0.5"#,
            r#""max_tokens":2147483647"#,
            r#""seed":9223372036854775807"#,
            r#""seed":-9223372036854775808"#,
        ] {
            assert!(run(&shell(good)).is_ok(), "{good}");
        }
        for bad in [
            r#""temperature":2.0001"#,
            r#""temperature":-0.1"#,
            r#""temperature":"1""#,
            r#""temperature":null"#,
            r#""temperature":1e400"#,
            r#""top_p":1.1"#,
            r#""presence_penalty":2.5"#,
            r#""max_tokens":0"#,
            r#""max_tokens":-1"#,
            r#""max_tokens":2147483648"#,
            r#""max_tokens":1.5"#,
            r#""max_tokens":1.0"#,
            r#""max_tokens":1e3"#,
            r#""max_tokens":99999999999999999999999999"#,
            r#""seed":9223372036854775808"#,
            r#""seed":1.5"#,
        ] {
            assert!(run(&shell(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn counts_over_the_limits_are_limit_failures() {
        let part = r#"{"type":"text","text":"x"}"#;
        let many = vec![part; MAX_CONTENT_PARTS + 1].join(",");
        let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":[{many}]}}]}}"#);
        assert_eq!(run(&body).unwrap_err(), ProtocolError::LimitExceeded);

        let msg = r#"{"role":"user","content":"x"}"#;
        let many = vec![msg; 257].join(",");
        let body = format!(r#"{{"model":"m","messages":[{many}]}}"#);
        assert_eq!(run(&body).unwrap_err(), ProtocolError::LimitExceeded);

        let body = r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stop":["a","b","c","d","e"]}"#;
        assert_eq!(run(body).unwrap_err(), ProtocolError::Unsupported);
        let long_user = "u".repeat(MAX_USER_BYTES + 1);
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"x"}}],"user":"{long_user}"}}"#
        );
        assert_eq!(run(&body).unwrap_err(), ProtocolError::Unsupported);
        let long_model = "m".repeat(MAX_MODEL_BYTES + 1);
        let body =
            format!(r#"{{"model":"{long_model}","messages":[{{"role":"user","content":"x"}}]}}"#);
        assert_eq!(run(&body).unwrap_err(), ProtocolError::Unsupported);
    }

    #[test]
    fn serialization_round_trips_through_the_matrix_and_is_bounded() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"aé\"\n"}]},{"role":"assistant","content":"한국어"}],"stream":true,"stream_options":{"include_usage":true},"temperature":0.5,"max_tokens":7,"n":1,"seed":-3,"stop":["x"],"user":"u","response_format":{"type":"json_object"}}"#;
        let request = run(body).expect("supported");
        let out = request.serialize_bounded(4096).expect("fits");
        // The fresh document satisfies the same matrix and carries the same texts.
        let again = run(std::str::from_utf8(&out).expect("utf8")).expect("round trip");
        assert_eq!(texts(&request), texts(&again));
        assert_eq!(request.params(), again.params());
        assert_eq!(
            request.serialize_bounded(4096).expect("fits"),
            again.serialize_bounded(4096).expect("fits")
        );
        // Exactly at the bound passes, one byte under refuses (never truncates).
        assert!(request.serialize_bounded(out.len()).is_ok());
        assert_eq!(
            request.serialize_bounded(out.len() - 1).unwrap_err(),
            SerializeError::Limit
        );
        assert_eq!(
            request.serialize_bounded(0).unwrap_err(),
            SerializeError::Limit
        );
    }

    #[test]
    fn debug_output_never_prints_content() {
        let r = run(
            r#"{"model":"SYNTHETIC_MODEL","messages":[{"role":"user","content":"SYNTHETIC_TEXT"}]}"#,
        )
        .expect("supported");
        let shown = format!("{r:?} {:?}", r.messages());
        assert!(!shown.contains("SYNTHETIC"), "{shown}");
    }
}
