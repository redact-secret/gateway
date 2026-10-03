//! Responses stateless request (contract `docs/contracts/responses-request.md`, ADR 0031).
//!
//! Accepted: `model`, `store` (required, exactly `false`), `instructions`, `input` (a
//! string, text message items, and the manual `function_call` / `function_call_output`
//! round trip, #85), `tools` (function tools only), `tool_choice`, `parallel_tool_calls`,
//! `text` (flat `json_schema` format and `verbosity`), `metadata`, and the structural
//! controls `stream`, `stream_options`, `temperature`, `top_p`, `max_output_tokens`.
//! Everything else is rejected at every depth, never stripped. Chat `tools`,
//! `response_format` and `tool_calls` shapes are not accepted here and never converted.
//!
//! Slot order is fixed and identical for reading and mutation: `instructions`, `input`,
//! `tools`, `tool_choice` name, `text.format`, `metadata`. `model` is scanned detect-only by
//! the boundary before the slots. One [`Derived`] counter, created in [`classify`], bounds
//! everything decoded out of strings (arguments, schemas) for the whole request. The route
//! is still unrouted until #86; [`super::validate_with`] is the only entry point.
//!
//! Layout: `mod.rs` owns the request type, classify dispatch, slots, serializer order;
//! `input.rs` owns `input` items; `tools.rs` owns tools and choice; `text.rs` owns `text`.
//! Schema, argument and metadata parsing are the Chat modules, shared.

use std::fmt;
use std::io::{self, Write};

use serde_json::Number;

use super::chat::metadata::{self, MetaLeaf};
use super::chat::serialize::{Bounded, json_str};
use super::chat::tool_calls::Derived;
use super::chat::{
    MAX_MODEL_BYTES, MAX_TOKEN_COUNT, Metadata, boolean, float_in, int_in, parse_model, unsupported,
};
use super::json::Json;
use super::{ProtocolError, SerializeError, SlotMode};
use crate::admission::RequestLimits;

mod input;
mod text;
mod tools;

pub use input::{
    Content, FunctionCall, FunctionCallOutput, Input, InputItem, Message, Phase, Role,
};
pub use text::{TextConfig, Verbosity};
pub use tools::{MAX_TOOLS, ToolDefs};

/// Most `input_text` parts in one content array (as Chat).
pub const MAX_INPUT_PARTS: usize = 64;

type Checked<T> = Result<T, ProtocolError>;

/// `stream_options` for Responses: only `include_obfuscation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    pub include_obfuscation: Option<bool>,
}

/// Validated numeric controls. Each present value passed its range check; the original
/// JSON number literal is kept.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params {
    pub temperature: Option<Number>,
    pub top_p: Option<Number>,
    pub max_output_tokens: Option<Number>,
}

/// The typed boundary representation of a supported Responses request.
///
/// Fields are private: inspected text is reachable only through [`Self::for_each_text`]
/// and [`Self::for_each_text_mut`], so a later stage can replace text but not structure.
/// `store` is not a field: it is required to be `false` and is always written as `false`.
pub struct ResponsesRequest {
    model: String,
    instructions: Option<String>,
    input: Input,
    tools: ToolDefs,
    text: Option<TextConfig>,
    metadata: Metadata,
    stream: Option<bool>,
    stream_options: Option<StreamOptions>,
    params: Params,
}

/// Where an inspected Responses text lives, in traversal order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsesSlot {
    /// `instructions`. Redact.
    Instructions,
    /// String `input`. Redact.
    Input,
    /// Text of `input[item]` (`part` is `Some` for an `input_text` parts array). Redact.
    Message { item: usize, part: Option<usize> },
    /// `function_call.call_id`. Detect-only.
    CallId { item: usize },
    /// `function_call.name`. Detect-only.
    CallName { item: usize },
    /// Object key inside the decoded `function_call.arguments`. Detect-only.
    CallArgumentKey { item: usize, leaf: usize },
    /// String leaf inside the decoded `function_call.arguments`. Redact.
    CallArgumentText { item: usize, leaf: usize },
    /// `function_call_output.call_id`. Detect-only.
    OutputCallId { item: usize },
    /// `function_call_output.output` (`part` is `Some` for a parts array). Redact.
    Output { item: usize, part: Option<usize> },
    /// `tools[tool].name`. Detect-only.
    ToolName { tool: usize },
    /// `tools[tool].description`. Redact.
    ToolDescription { tool: usize },
    /// Schema label under `tools[tool].parameters`. Detect-only.
    ToolSchemaLabel { tool: usize, leaf: usize },
    /// Schema free text under `tools[tool].parameters`. Redact.
    ToolSchemaText { tool: usize, leaf: usize },
    /// `tool_choice.name`. Detect-only.
    ToolChoiceName,
    /// `text.format.name`. Detect-only.
    FormatName,
    /// `text.format.description`. Redact.
    FormatDescription,
    /// Schema label under `text.format.schema`. Detect-only.
    FormatSchemaLabel { leaf: usize },
    /// Schema free text under `text.format.schema`. Redact.
    FormatSchemaText { leaf: usize },
    /// `metadata` key. Detect-only.
    MetadataKey { entry: usize },
    /// `metadata` value. Redact.
    MetadataValue { entry: usize },
}

impl ResponsesSlot {
    /// The slot class's mode. Exhaustive on purpose: a new variant must choose.
    #[must_use]
    pub const fn mode(self) -> SlotMode {
        match self {
            Self::Instructions
            | Self::Input
            | Self::Message { .. }
            | Self::CallArgumentText { .. }
            | Self::Output { .. }
            | Self::ToolDescription { .. }
            | Self::ToolSchemaText { .. }
            | Self::FormatDescription
            | Self::FormatSchemaText { .. }
            | Self::MetadataValue { .. } => SlotMode::Redact,
            Self::CallId { .. }
            | Self::CallName { .. }
            | Self::CallArgumentKey { .. }
            | Self::OutputCallId { .. }
            | Self::ToolName { .. }
            | Self::ToolSchemaLabel { .. }
            | Self::ToolChoiceName
            | Self::FormatName
            | Self::FormatSchemaLabel { .. }
            | Self::MetadataKey { .. } => SlotMode::DetectOnly,
        }
    }
}

impl ResponsesRequest {
    /// Validated structural identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    #[must_use]
    pub const fn input(&self) -> &Input {
        &self.input
    }

    #[must_use]
    pub const fn tools(&self) -> &ToolDefs {
        &self.tools
    }

    #[must_use]
    pub const fn text(&self) -> Option<&TextConfig> {
        self.text.as_ref()
    }

    #[must_use]
    pub const fn metadata(&self) -> &Metadata {
        &self.metadata
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
    pub const fn params(&self) -> &Params {
        &self.params
    }

    /// Visit every inspected text in traversal order.
    pub fn for_each_text(&self, mut f: impl FnMut(ResponsesSlot, &str)) {
        if let Some(text) = &self.instructions {
            f(ResponsesSlot::Instructions, text);
        }
        input::visit(&self.input, &mut f);
        tools::visit(&self.tools, &mut f);
        text::visit(self.text.as_ref(), &mut f);
        metadata::visit_leaves(&self.metadata, &mut |leaf, text| {
            f(metadata_slot(leaf), text);
        });
    }

    /// Visit every inspected text mutably, in the same order as [`Self::for_each_text`].
    pub fn for_each_text_mut(&mut self, mut f: impl FnMut(ResponsesSlot, &mut String)) {
        if let Some(text) = &mut self.instructions {
            f(ResponsesSlot::Instructions, text);
        }
        input::visit_mut(&mut self.input, &mut f);
        tools::visit_mut(&mut self.tools, &mut f);
        text::visit_mut(self.text.as_mut(), &mut f);
        metadata::visit_leaves_mut(&mut self.metadata, &mut |leaf, text| {
            f(metadata_slot(leaf), text);
        });
    }

    /// Re-check what a replacement can break before serialization. The output bound is
    /// enforced by the serializer.
    ///
    /// # Errors
    /// [`SerializeError::Invalid`] when the model identifier or the input shape is no
    /// longer valid.
    pub fn revalidate(&self) -> Result<(), SerializeError> {
        if self.model.is_empty() || self.model.len() > MAX_MODEL_BYTES {
            return Err(SerializeError::Invalid);
        }
        if self.stream_options.is_some() && self.stream != Some(true) {
            return Err(SerializeError::Invalid);
        }
        input::revalidate(&self.input)?;
        tools::revalidate(&self.tools)?;
        text::revalidate(self.text.as_ref())?;
        metadata::revalidate(&self.metadata)
    }

    /// Serialize a fresh JSON document, bounded while it is produced. Canonical order:
    /// `model`, `instructions`, `input`, `store` (always `false`), `tools`, `tool_choice`,
    /// `parallel_tool_calls`, `text`, `metadata`, `stream`, `stream_options`, `temperature`,
    /// `top_p`, `max_output_tokens`.
    ///
    /// # Errors
    /// [`SerializeError::Limit`] when the output would exceed `max_bytes`;
    /// [`SerializeError::Invalid`] for any other writer failure.
    pub fn serialize_bounded(&self, max_bytes: usize) -> Result<Vec<u8>, SerializeError> {
        let mut estimate = 256_usize;
        self.for_each_text(|_, text| estimate = estimate.saturating_add(text.len()));
        let mut out = Bounded::new(max_bytes, estimate);
        match self.write_to(&mut out) {
            Ok(()) => Ok(out.into_inner()),
            Err(_) if out.overflowed() => Err(SerializeError::Limit),
            Err(_) => Err(SerializeError::Invalid),
        }
    }

    fn write_to(&self, w: &mut Bounded) -> io::Result<()> {
        w.write_all(b"{\"model\":")?;
        json_str(w, &self.model)?;
        if let Some(text) = &self.instructions {
            w.write_all(b",\"instructions\":")?;
            json_str(w, text)?;
        }
        input::write(&self.input, w)?;
        w.write_all(b",\"store\":false")?;
        tools::write(&self.tools, w)?;
        text::write(self.text.as_ref(), w)?;
        metadata::write(&self.metadata, w)?;
        if let Some(stream) = self.stream {
            w.write_all(if stream {
                b",\"stream\":true"
            } else {
                b",\"stream\":false"
            })?;
        }
        if let Some(options) = self.stream_options {
            w.write_all(b",\"stream_options\":{")?;
            if let Some(obfuscation) = options.include_obfuscation {
                w.write_all(b"\"include_obfuscation\":")?;
                w.write_all(if obfuscation { b"true" } else { b"false" })?;
            }
            w.write_all(b"}")?;
        }
        for (key, value) in [
            ("temperature", &self.params.temperature),
            ("top_p", &self.params.top_p),
            ("max_output_tokens", &self.params.max_output_tokens),
        ] {
            if let Some(number) = value {
                w.write_all(b",\"")?;
                w.write_all(key.as_bytes())?;
                w.write_all(b"\":")?;
                serde_json::to_writer(&mut *w, number).map_err(io::Error::from)?;
            }
        }
        w.write_all(b"}")
    }

    #[cfg(test)]
    pub(crate) fn for_test(model: &str, instructions: Option<&str>, input: &str) -> Self {
        Self {
            model: model.to_owned(),
            instructions: instructions.map(str::to_owned),
            input: Input::Text(input.to_owned()),
            tools: ToolDefs::default(),
            text: None,
            metadata: Metadata::default(),
            stream: None,
            stream_options: None,
            params: Params::default(),
        }
    }
}

impl fmt::Debug for ResponsesRequest {
    /// Never prints request content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponsesRequest")
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

/// Classify a parsed document against the matrix, consuming it. Strings move into the
/// typed request (no body-sized clone).
///
/// # Errors
/// [`ProtocolError::Unsupported`] for any unknown field, wrong type, `null`, out-of-contract
/// value, missing or non-`false` `store`; [`ProtocolError::LimitExceeded`] for an item or
/// part count limit.
pub(super) fn classify(document: Json, limits: &RequestLimits) -> Checked<ResponsesRequest> {
    let Json::Object(entries) = document else {
        return Err(unsupported());
    };
    let mut model = None;
    let mut instructions = None;
    let mut input = None;
    let mut tool_defs = ToolDefs::default();
    let mut text = None;
    let mut meta = Metadata::default();
    let mut store_false = false;
    let mut stream = None;
    let mut stream_options = None;
    let mut params = Params::default();
    // Request-wide counters for strings decoded out of strings and for schema trees,
    // shared by every call, tool and schema (ADR 0025 D11).
    let mut derived = Derived::new(limits);
    for (key, value) in entries {
        match key.as_str() {
            "model" => model = Some(parse_model(value)?),
            "instructions" => match value {
                Json::String(text) => instructions = Some(text),
                _ => return Err(unsupported()),
            },
            "input" => input = Some(input::parse_input(value, limits, &mut derived)?),
            "tools" => tools::parse_tools(&mut tool_defs, value, &mut derived)?,
            "tool_choice" => tools::parse_tool_choice(&mut tool_defs, value, &mut derived)?,
            "parallel_tool_calls" => tools::parse_parallel(&mut tool_defs, value)?,
            "text" => text = Some(text::parse(value, &mut derived)?),
            "metadata" => meta = metadata::parse(value, &mut derived)?,
            // Required, exactly `false` (ADR 0031). `true`, `null` and absence reject.
            "store" => match value {
                Json::Bool(false) => store_false = true,
                _ => return Err(unsupported()),
            },
            "stream" => stream = Some(boolean(value)?),
            "stream_options" => stream_options = Some(parse_stream_options(value)?),
            "temperature" => params.temperature = Some(float_in(value, 0.0, 2.0)?),
            "top_p" => params.top_p = Some(float_in(value, 0.0, 1.0)?),
            "max_output_tokens" => {
                params.max_output_tokens = Some(int_in(value, 1, MAX_TOKEN_COUNT)?);
            }
            // Everything else: previous_response_id, conversation, prompt, background,
            // include, reasoning, unknown keys.
            _ => return Err(unsupported()),
        }
    }
    if !store_false || (stream_options.is_some() && stream != Some(true)) {
        return Err(unsupported());
    }
    tool_defs.finish()?;
    Ok(ResponsesRequest {
        model: model.ok_or_else(unsupported)?,
        instructions,
        input: input.ok_or_else(unsupported)?,
        tools: tool_defs,
        text,
        metadata: meta,
        stream,
        stream_options,
        params,
    })
}

const fn metadata_slot(leaf: MetaLeaf) -> ResponsesSlot {
    match leaf {
        MetaLeaf::Key(entry) => ResponsesSlot::MetadataKey { entry },
        MetaLeaf::Value(entry) => ResponsesSlot::MetadataValue { entry },
    }
}

fn parse_stream_options(value: Json) -> Checked<StreamOptions> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut include_obfuscation = None;
    for (key, value) in entries {
        match key.as_str() {
            "include_obfuscation" => include_obfuscation = Some(boolean(value)?),
            _ => return Err(unsupported()),
        }
    }
    Ok(StreamOptions {
        include_obfuscation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::json::{Budget, parse_budgeted};

    fn run(body: &str) -> Checked<ResponsesRequest> {
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

    fn texts(r: &ResponsesRequest) -> Vec<(ResponsesSlot, String)> {
        let mut out = Vec::new();
        r.for_each_text(|slot, text| out.push((slot, text.to_owned())));
        out
    }

    #[test]
    fn traversal_order_is_instructions_then_input_regardless_of_key_order() {
        let r = run(r#"{"input":[{"role":"user","content":[{"type":"input_text","text":"a"},{"type":"input_text","text":"b"}]},{"role":"assistant","content":"c"}],"store":false,"instructions":"i","model":"m"}"#)
            .expect("supported");
        assert_eq!(
            texts(&r),
            vec![
                (ResponsesSlot::Instructions, "i".to_owned()),
                (
                    ResponsesSlot::Message {
                        item: 0,
                        part: Some(0)
                    },
                    "a".to_owned()
                ),
                (
                    ResponsesSlot::Message {
                        item: 0,
                        part: Some(1)
                    },
                    "b".to_owned()
                ),
                (
                    ResponsesSlot::Message {
                        item: 1,
                        part: None
                    },
                    "c".to_owned()
                ),
            ]
        );
        assert!(
            texts(&r)
                .iter()
                .all(|(slot, _)| slot.mode() == SlotMode::Redact)
        );
        assert_eq!(r.instructions(), Some("i"));
    }

    #[test]
    fn full_request_slot_order_and_modes_are_fixed_and_mutation_matches() {
        let body = r#"{"metadata":{"k":"mv"},"text":{"format":{"type":"json_schema","name":"fn","description":"fd","schema":{"type":"object","properties":{"p":{"type":"string","description":"fs"}},"required":["p"]}}},"tool_choice":{"type":"function","name":"t"},"tools":[{"type":"function","name":"t","description":"td","parameters":{"type":"object","properties":{"q":{"type":"string"}}},"strict":null}],"input":[{"type":"function_call","call_id":"c","name":"f","arguments":"{\"a\":\"x\"}"},{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"o"}]}],"store":false,"model":"m","instructions":"i"}"#;
        let mut r = run(body).expect("supported");
        let mut read = Vec::new();
        r.for_each_text(|slot, text| read.push((slot, text.to_owned())));
        let slots: Vec<_> = read.iter().map(|(s, _)| *s).collect();
        use ResponsesSlot as S;
        assert_eq!(
            slots,
            [
                S::Instructions,
                S::CallId { item: 0 },
                S::CallName { item: 0 },
                S::CallArgumentKey { item: 0, leaf: 0 },
                S::CallArgumentText { item: 0, leaf: 1 },
                S::OutputCallId { item: 1 },
                S::Output {
                    item: 1,
                    part: Some(0)
                },
                S::ToolName { tool: 0 },
                S::ToolDescription { tool: 0 },
                S::ToolSchemaLabel { tool: 0, leaf: 0 },
                S::ToolChoiceName,
                S::FormatName,
                S::FormatDescription,
                S::FormatSchemaLabel { leaf: 0 },
                S::FormatSchemaText { leaf: 1 },
                S::FormatSchemaLabel { leaf: 2 },
                S::MetadataKey { entry: 0 },
                S::MetadataValue { entry: 0 },
            ]
        );
        let redact = |s: &ResponsesSlot| s.mode() == SlotMode::Redact;
        let expected_redact = [
            S::Instructions,
            S::CallArgumentText { item: 0, leaf: 1 },
            S::Output {
                item: 1,
                part: Some(0),
            },
            S::ToolDescription { tool: 0 },
            S::FormatDescription,
            S::FormatSchemaText { leaf: 1 },
            S::MetadataValue { entry: 0 },
        ];
        assert_eq!(
            slots.iter().filter(|s| redact(s)).count(),
            expected_redact.len()
        );
        assert!(expected_redact.iter().all(redact));
        let mut mutated = Vec::new();
        r.for_each_text_mut(|slot, _| mutated.push(slot));
        assert_eq!(mutated, slots);
        assert!(r.revalidate().is_ok());
        let out = r.serialize_bounded(4096).expect("fits");
        let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(v["input"][0]["arguments"], "{\"a\":\"x\"}");
        assert!(format!("{r:?}").len() < 200);
    }

    #[test]
    fn mutable_traversal_matches_the_read_only_order_and_is_revalidated() {
        let mut r = run(r#"{"model":"m","store":false,"input":[{"role":"user","content":"x"}],"instructions":"i"}"#)
            .expect("supported");
        let mut seen = Vec::new();
        r.for_each_text_mut(|slot, text| {
            seen.push(slot);
            text.push('!');
        });
        assert_eq!(
            seen,
            [
                ResponsesSlot::Instructions,
                ResponsesSlot::Message {
                    item: 0,
                    part: None
                }
            ]
        );
        assert!(r.revalidate().is_ok());
        assert_eq!(
            r.serialize_bounded(1024).expect("fits"),
            br#"{"model":"m","instructions":"i!","input":[{"role":"user","content":"x!"}],"store":false}"#
        );
        assert!(!format!("{r:?}").contains('!'));
    }

    #[test]
    fn numbers_keep_their_literals_and_store_is_not_a_field() {
        let r = run(r#"{"model":"m","store":false,"input":"x","temperature":1.50,"top_p":1,"max_output_tokens":7}"#)
            .expect("supported");
        let out = String::from_utf8(r.serialize_bounded(1024).expect("fits")).expect("utf8");
        assert!(
            out.ends_with(r#""store":false,"temperature":1.5,"top_p":1,"max_output_tokens":7}"#)
        );
    }
}
