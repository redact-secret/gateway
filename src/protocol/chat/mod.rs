//! `POST /v1/chat/completions` request matrix (#18, extended by the Alpha 2 contract, #52).
//!
//! Normative table: `docs/contracts/chat-completions-request.md`; decisions and module
//! ownership: ADR 0025. This module and that document must agree. Every field is exactly
//! one of: inspected application text, validated structural/control data, or rejected.
//! Unknown fields are rejected at every depth. The classifier consumes the parsed tree and
//! moves its strings into the typed [`ChatRequest`], so there is one working structure and
//! no body-sized clone (ADR 0007).
//!
//! Layout (one owner per file; behavior-neutral split, #52):
//!
//! | File | Owns | Owner |
//! | --- | --- | --- |
//! | `mod.rs` | `ChatRequest`, top-level `classify` dispatch, primitives | baseline (touch only to add a dispatch line) |
//! | `messages.rs` | roles, content, per-message slots and writer | #53 |
//! | `tool_calls.rs` | tool history hooks (correlation, arguments tree) | #53 |
//! | `tool_defs.rs`, `schema.rs` | `tools`, `tool_choice`, `parallel_tool_calls`, schema subset | #54 |
//! | `response_format.rs` | `response_format` incl. `json_schema` | #54 |
//! | `metadata.rs` | `metadata` | #55 |
//! | `slots.rs` | `TextSlot`, `SlotMode`, traversal order, revalidation hook | baseline; reserved variants are pre-declared |
//! | `serialize.rs` | bounded writer, canonical key order | baseline |
//! | `controls.rs`, `stop_user.rs` | unchanged Alpha 1 fields | none |
//!
//! Errors are fixed [`ProtocolError`] codes. Which field failed is deliberately not
//! reported: field names and values are caller payload.

use std::fmt;

use serde_json::Number;

use super::ProtocolError;
use super::json::Json;
use crate::admission::RequestLimits;

mod controls;
mod messages;
mod metadata;
mod response_format;
mod schema;
mod serialize;
mod slots;
mod stop_user;
mod tool_calls;
mod tool_defs;

pub use controls::{Params, StreamOptions};
pub use messages::{Content, Message, Role};
pub use metadata::Metadata;
pub use response_format::JsonSchemaFormat;
pub use response_format::ResponseFormat;
pub use serialize::SerializeError;
pub use slots::{SlotMode, TextSlot};
pub use stop_user::Stop;
pub use tool_defs::ToolDefs;

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

/// The typed boundary representation of a supported Chat Completions request.
///
/// Fields are private. Inspected text is reachable only through [`Self::for_each_text`]
/// and [`Self::for_each_text_mut`], so a later stage can replace text in place but cannot
/// add fields, change roles, or alter structure.
pub struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    tool_defs: ToolDefs,
    params: Params,
    stream: Option<bool>,
    stream_options: Option<StreamOptions>,
    stop: Option<Stop>,
    user: Option<String>,
    metadata: Metadata,
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
    pub const fn response_format(&self) -> Option<&ResponseFormat> {
        self.response_format.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            model: "synthetic-model".to_owned(),
            messages: Vec::new(),
            tool_defs: ToolDefs::default(),
            params: Params::default(),
            stream: None,
            stream_options: None,
            stop: None,
            user: None,
            metadata: Metadata::default(),
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
    let mut tool_defs = ToolDefs::default();
    let mut metadata = Metadata::default();
    let mut params = Params::default();
    let mut stream = None;
    let mut stream_options = None;
    let mut stop = None;
    let mut user = None;
    let mut response_format = None;
    // Request-wide counters for strings decoded out of strings (tool arguments, ADR 0025 D11).
    let mut derived = tool_calls::Derived::new(limits);
    for (key, value) in entries {
        match key.as_str() {
            "model" => model = Some(controls::parse_model(value)?),
            "messages" => messages = Some(messages::parse_messages(value, limits, &mut derived)?),
            "tools" | "tool_choice" | "parallel_tool_calls" => {
                tool_defs::parse_field(&mut tool_defs, key.as_str(), value, &mut derived)?;
            }
            "metadata" => metadata = metadata::parse(value, limits)?,
            "stream" => stream = Some(boolean(value)?),
            "stream_options" => stream_options = Some(controls::parse_stream_options(value)?),
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
            "stop" => stop = Some(stop_user::parse_stop(value)?),
            "user" => user = Some(stop_user::parse_user(value)?),
            "response_format" => {
                response_format =
                    Some(response_format::parse_response_format(value, &mut derived)?);
            }
            // Everything else, including tools, functions, metadata, logit_bias,
            // audio/modalities, and any unknown key, is rejected.
            _ => return Err(unsupported()),
        }
    }
    let messages = messages.ok_or_else(unsupported)?;
    tool_calls::validate_history(&messages)?;
    tool_defs::finish(&tool_defs)?;
    Ok(ChatRequest {
        model: model.ok_or_else(unsupported)?,
        messages,
        tool_defs,
        params,
        stream,
        stream_options,
        stop,
        user,
        metadata,
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
        assert_eq!(r.response_format(), Some(&ResponseFormat::JsonObject));
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

    /// Pins the canonical outbound bytes for an Alpha 1 request so the module split (and
    /// every later field) cannot change them silently.
    #[test]
    fn alpha1_serialization_bytes_are_pinned() {
        let body = concat!(
            r#"{"model":"m","messages":[{"role":"system","content":"s"},"#,
            r#"{"role":"user","content":[{"type":"text","text":"a\"\n"},{"type":"text","text":"한국어"}]},"#,
            r#"{"role":"assistant","content":"r"}],"stream":true,"stream_options":{"include_usage":true},"#,
            r#""temperature":0.5,"top_p":1,"max_tokens":7,"max_completion_tokens":9,"#,
            r#""presence_penalty":-1.5,"frequency_penalty":2,"n":1,"seed":-3,"stop":["x","y"],"#,
            r#""user":"u","response_format":{"type":"json_object"}}"#
        );
        let request = run(body).expect("supported");
        let out = request.serialize_bounded(4096).expect("fits");
        assert_eq!(std::str::from_utf8(&out).expect("utf8"), body);
        // Non-canonical input order still produces the canonical order.
        let shuffled = r#"{"user":"u","stop":"x","messages":[{"content":"s","role":"user"}],"model":"m","response_format":{"type":"text"},"n":1,"stream":false}"#;
        let out = run(shuffled)
            .expect("supported")
            .serialize_bounded(4096)
            .expect("fits");
        assert_eq!(
            std::str::from_utf8(&out).expect("utf8"),
            r#"{"model":"m","messages":[{"role":"user","content":"s"}],"stream":false,"n":1,"stop":"x","user":"u","response_format":{"type":"text"}}"#
        );
    }

    #[test]
    fn slot_modes_are_fixed_by_class_and_alpha1_slots_are_all_redact() {
        let r = run(
            r#"{"model":"m","messages":[{"role":"user","content":"a"}],"stop":"b","user":"c"}"#,
        )
        .expect("supported");
        let mut modes = Vec::new();
        r.for_each_text(|slot, _| modes.push(slot.mode()));
        assert_eq!(modes, [SlotMode::Redact; 3]);
        assert_eq!(r.text_count(), r.redactable_count());
        // Identifier-like reserved classes are detect-only; free text is redact.
        assert_eq!(
            TextSlot::ToolCallId {
                message: 0,
                call: 0
            }
            .mode(),
            SlotMode::DetectOnly
        );
        assert_eq!(
            TextSlot::MetadataKey { entry: 0 }.mode(),
            SlotMode::DetectOnly
        );
        assert_eq!(
            TextSlot::MetadataValue { entry: 0 }.mode(),
            SlotMode::Redact
        );
        assert_eq!(
            TextSlot::ToolDefDescription { tool: 0 }.mode(),
            SlotMode::Redact
        );
        assert!(r.revalidate().is_ok());
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
