//! Tool history: assistant `tool_calls`, `role: tool` results, `tool_call_id`, nullable
//! assistant content, correlation, and the decoded `function.arguments` tree.
//! Owner: #53. Contract: `docs/contracts/chat-completions-request.md`, ADR 0025.
//!
//! - [`parse_calls`] and [`parse_link`] are called by `messages::parse_message` for the keys
//!   `tool_calls` and `tool_call_id`.
//! - [`validate_history`] is called by `classify` once all messages are parsed
//!   (correlation, ordering, uniqueness), before any inspection.
//! - [`revalidate`] is called by [`super::ChatRequest::revalidate`] after text mutation.
//! - Slot order and the writer are driven from `messages::visit`, `visit_mut` and `write`
//!   through [`visit_call`], [`visit_call_mut`] and [`write_calls`].
//!
//! `function.arguments` is the only string in the whole request that is parsed as JSON.
//! It is parsed once, with the strict duplicate-key-rejecting parser under budgets derived
//! from the request-wide [`Derived`] counters, and held as an [`Arg`] tree: object keys
//! are NAME labels (detect-only slots, never rewritten), string values are redactable
//! text, everything else is preserved structure. On output the tree is re-encoded compactly
//! and escaped once more into the JSON string the provider expects. Tool-result content is
//! plain text and never reaches this module's parser.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};

use serde_json::Number;

use super::serialize::{Bounded, json_str};
use super::{Checked, Message, ProtocolError, RequestLimits, Role, SerializeError, unsupported};
use super::{TextSlot, string};
use crate::protocol::json::{Budget, Json, ParseError, parse_budgeted};

/// Most tool calls in one assistant message.
pub const MAX_TOOL_CALLS: usize = 32;
/// Most nested containers in decoded arguments, the root object included.
pub const MAX_ARGUMENT_DEPTH: u32 = 8;
/// Longest NAME or LINK label.
const MAX_LABEL_BYTES: usize = 64;

/// Request-wide counters for everything decoded out of a string (ADR 0025 D11). One value
/// lives for the whole `classify` call and is shared by every call, tool and schema, so many
/// small ones cannot add up past the bound the parse already enforces.
#[derive(Debug)]
pub struct Derived {
    nodes_left: u32,
    bytes_left: usize,
}

impl Derived {
    #[must_use]
    pub fn new(limits: &RequestLimits) -> Self {
        Self {
            nodes_left: limits.max_nodes,
            bytes_left: usize::try_from(limits.max_body_bytes).unwrap_or(usize::MAX),
        }
    }

    /// Charge `nodes` and `bytes`; shared by tool arguments and schema trees.
    ///
    /// # Errors
    /// [`ProtocolError::LimitExceeded`] when either budget is exhausted.
    pub(super) fn charge(&mut self, nodes: usize, bytes: usize) -> Checked<()> {
        let nodes = u32::try_from(nodes).map_err(|_| ProtocolError::LimitExceeded)?;
        self.nodes_left = self
            .nodes_left
            .checked_sub(nodes)
            .ok_or(ProtocolError::LimitExceeded)?;
        self.bytes_left = self
            .bytes_left
            .checked_sub(bytes)
            .ok_or(ProtocolError::LimitExceeded)?;
        Ok(())
    }
}

/// Decoded `function.arguments`: the approved structure with owned strings.
pub(super) enum Arg {
    Null,
    Bool(bool),
    Number(Number),
    Text(String),
    Array(Vec<Arg>),
    /// Insertion-ordered; keys are NAME labels, unique among siblings.
    Object(Vec<(String, Arg)>),
}

/// One assistant tool call.
pub(super) struct ToolCall {
    id: String,
    name: String,
    args: Arg,
    /// Node count (values plus keys) at parse time; a mutation must not change it.
    nodes: usize,
    /// Longest decoded string a replacement may produce.
    max_string: usize,
    /// Digest of the label slots (`id`, `name`, argument keys in order) at parse time.
    labels: u64,
}

impl fmt::Debug for ToolCall {
    /// Prints nothing from the call.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ToolCall")
    }
}

/// NAME: 1 to 64 bytes of `[A-Za-z0-9_-]`.
fn is_name(text: &str) -> bool {
    (1..=MAX_LABEL_BYTES).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// LINK: 1 to 64 bytes of `[A-Za-z0-9_.:-]`.
fn is_link(text: &str) -> bool {
    (1..=MAX_LABEL_BYTES).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// Digest of a call's label slots, in traversal order. Deterministic (fixed-key hasher).
/// Revalidation compares it so that a label rewritten to another *valid* label is caught too.
fn call_labels(id: &str, name: &str, args: &Arg) -> u64 {
    fn keys(arg: &Arg, h: &mut DefaultHasher) {
        match arg {
            Arg::Array(items) => items.iter().for_each(|item| keys(item, h)),
            Arg::Object(entries) => {
                for (key, value) in entries {
                    key.hash(h);
                    keys(value, h);
                }
            }
            Arg::Null | Arg::Bool(_) | Arg::Number(_) | Arg::Text(_) => {}
        }
    }
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    name.hash(&mut h);
    keys(args, &mut h);
    h.finish()
}

/// Digest of a tool result's `tool_call_id` (0 when the message has none).
pub(super) fn link_digest(id: Option<&str>) -> u64 {
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

/// `tool_call_id` of a `role: tool` message (a LINK label).
pub(super) fn parse_link(value: Json) -> Checked<String> {
    let text = string(value)?;
    if is_link(&text) {
        Ok(text)
    } else {
        Err(unsupported())
    }
}

/// `tool_calls`: 1 to 32 calls.
pub(super) fn parse_calls(
    value: Json,
    limits: &RequestLimits,
    derived: &mut Derived,
) -> Checked<Vec<ToolCall>> {
    let Json::Array(items) = value else {
        return Err(unsupported());
    };
    if items.is_empty() {
        return Err(unsupported());
    }
    if items.len() > MAX_TOOL_CALLS {
        return Err(ProtocolError::LimitExceeded);
    }
    items
        .into_iter()
        .map(|item| parse_call(item, limits, derived))
        .collect()
}

fn parse_call(value: Json, limits: &RequestLimits, derived: &mut Derived) -> Checked<ToolCall> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut id = None;
    let mut is_function = false;
    let mut function = None;
    for (key, value) in entries {
        match key.as_str() {
            "id" => id = Some(parse_link(value)?),
            "type" => is_function = string(value)? == "function",
            "function" => function = Some(value),
            _ => return Err(unsupported()),
        }
    }
    let (Some(id), true, Some(Json::Object(function))) = (id, is_function, function) else {
        return Err(unsupported());
    };
    let mut name = None;
    let mut arguments = None;
    for (key, value) in function {
        match key.as_str() {
            "name" => {
                let text = string(value)?;
                if !is_name(&text) {
                    return Err(unsupported());
                }
                name = Some(text);
            }
            "arguments" => arguments = Some(string(value)?),
            _ => return Err(unsupported()),
        }
    }
    let (Some(name), Some(arguments)) = (name, arguments) else {
        return Err(unsupported());
    };
    let max_string = usize::try_from(limits.max_string_bytes).unwrap_or(usize::MAX);
    let (args, nodes) = parse_arguments(&arguments, max_string, derived)?;
    let labels = call_labels(&id, &name, &args);
    Ok(ToolCall {
        id,
        name,
        args,
        nodes,
        max_string,
        labels,
    })
}

/// False when an integer literal outside the `i64` range appears outside a string. The
/// parser would otherwise silently turn it into a float (ADR 0025 D10). Never panics;
/// anything malformed is left to the strict parser.
fn integers_fit(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0_usize;
    let mut in_string = false;
    while let Some(&b) = bytes.get(i) {
        if in_string {
            match b {
                b'\\' => i = i.saturating_add(1),
                b'"' => in_string = false,
                _ => {}
            }
            i = i.saturating_add(1);
        } else if b == b'-' || b.is_ascii_digit() {
            let start = i;
            while bytes
                .get(i)
                .is_some_and(|c| matches!(c, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E'))
            {
                i = i.saturating_add(1);
            }
            let token = text.get(start..i).unwrap_or_default();
            let digits = token.strip_prefix('-').unwrap_or(token);
            if !digits.is_empty()
                && digits.bytes().all(|c| c.is_ascii_digit())
                && token.parse::<i64>().is_err()
            {
                return false;
            }
        } else {
            if b == b'"' {
                in_string = true;
            }
            i = i.saturating_add(1);
        }
    }
    true
}

fn parse_arguments(text: &str, max_string: usize, derived: &mut Derived) -> Checked<(Arg, usize)> {
    if !integers_fit(text) {
        return Err(unsupported());
    }
    let budget = Budget {
        max_depth: MAX_ARGUMENT_DEPTH,
        max_nodes: derived.nodes_left,
        max_string_bytes: max_string,
        max_total_string_bytes: derived.bytes_left,
    };
    let tree = parse_budgeted(text.as_bytes(), &budget).map_err(|e| match e {
        ParseError::Malformed => ProtocolError::Malformed,
        ParseError::LimitExceeded => ProtocolError::LimitExceeded,
    })?;
    if !matches!(tree, Json::Object(_)) {
        return Err(unsupported());
    }
    let mut nodes = 0_usize;
    let mut bytes = 0_usize;
    let arg = convert(tree, &mut nodes, &mut bytes)?;
    derived.charge(nodes, bytes)?;
    Ok((arg, nodes))
}

fn convert(value: Json, nodes: &mut usize, bytes: &mut usize) -> Checked<Arg> {
    *nodes = nodes.saturating_add(1);
    Ok(match value {
        Json::Null => Arg::Null,
        Json::Bool(b) => Arg::Bool(b),
        Json::Number(n) => {
            // `i64` or finite `f64` only; an integer beyond `i64` is rejected.
            if n.is_u64() && n.as_i64().is_none() {
                return Err(unsupported());
            }
            Arg::Number(n)
        }
        Json::String(s) => {
            *bytes = bytes.saturating_add(s.len());
            Arg::Text(s)
        }
        Json::Array(items) => Arg::Array(
            items
                .into_iter()
                .map(|item| convert(item, nodes, bytes))
                .collect::<Checked<Vec<_>>>()?,
        ),
        Json::Object(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                if !is_name(&key) {
                    return Err(unsupported());
                }
                *nodes = nodes.saturating_add(1);
                *bytes = bytes.saturating_add(key.len());
                out.push((key, convert(value, nodes, bytes)?));
            }
            Arg::Object(out)
        }
    })
}

/// Cross-message tool-call correlation, before any inspection: ids are unique across the
/// request; a `role: tool` message answers an id issued by the assistant message it
/// directly follows (or follows through other results of that message), once.
pub(super) fn validate_history(messages: &[Message]) -> Checked<()> {
    let mut seen: HashSet<&str> = HashSet::new();
    // Ids issued by the latest assistant message and whether each was answered.
    let mut open: Vec<(&str, bool)> = Vec::new();
    for message in messages {
        if message.role == Role::Tool {
            let id = message.tool_call_id.as_deref().ok_or_else(unsupported)?;
            let slot = open
                .iter_mut()
                .find(|(issued, _)| *issued == id)
                .ok_or_else(unsupported)?;
            if slot.1 {
                return Err(unsupported());
            }
            slot.1 = true;
        } else {
            open.clear();
            for call in &message.tool_calls {
                if !seen.insert(call.id.as_str()) {
                    return Err(unsupported());
                }
                open.push((call.id.as_str(), false));
            }
        }
    }
    Ok(())
}

/// Revalidation after mutation: labels still conform, linkage still holds, every decoded
/// tree has the node count it had with unique conforming keys, and no replacement string
/// outgrew the per-string bound. Aggregate size is bounded by the serializer's output bound.
pub(super) fn revalidate(messages: &[Message]) -> Result<(), SerializeError> {
    for message in messages {
        if let Some(id) = &message.tool_call_id
            && !is_link(id)
        {
            return Err(SerializeError::Invalid);
        }
        if link_digest(message.tool_call_id.as_deref()) != message.link {
            return Err(SerializeError::Invalid);
        }
        for call in &message.tool_calls {
            if !is_link(&call.id) || !is_name(&call.name) {
                return Err(SerializeError::Invalid);
            }
            if call_labels(&call.id, &call.name, &call.args) != call.labels {
                return Err(SerializeError::Invalid);
            }
            let mut nodes = 0_usize;
            check_arg(&call.args, call.max_string, &mut nodes)?;
            if nodes != call.nodes {
                return Err(SerializeError::Invalid);
            }
        }
    }
    validate_history(messages).map_err(|_| SerializeError::Invalid)
}

fn check_arg(arg: &Arg, max_string: usize, nodes: &mut usize) -> Result<(), SerializeError> {
    *nodes = nodes.saturating_add(1);
    match arg {
        Arg::Null | Arg::Bool(_) | Arg::Number(_) => Ok(()),
        Arg::Text(text) if text.len() > max_string => Err(SerializeError::Limit),
        Arg::Text(_) => Ok(()),
        Arg::Array(items) => items
            .iter()
            .try_for_each(|item| check_arg(item, max_string, nodes)),
        Arg::Object(entries) => {
            let mut keys: HashSet<&str> = HashSet::new();
            for (key, value) in entries {
                *nodes = nodes.saturating_add(1);
                if !is_name(key) || !keys.insert(key.as_str()) {
                    return Err(SerializeError::Invalid);
                }
                check_arg(value, max_string, nodes)?;
            }
            Ok(())
        }
    }
}

/// Visit one call's slots: `id`, `name`, then argument keys and string values in document
/// order. `leaf` counts keys and string values together, from 0.
pub(super) fn visit_call(
    message: usize,
    call: usize,
    tool_call: &ToolCall,
    f: &mut impl FnMut(TextSlot, &str),
) {
    f(TextSlot::ToolCallId { message, call }, &tool_call.id);
    f(TextSlot::ToolCallName { message, call }, &tool_call.name);
    let mut leaf = 0_usize;
    walk(&tool_call.args, (message, call), &mut leaf, f);
}

fn walk(arg: &Arg, at: (usize, usize), leaf: &mut usize, f: &mut impl FnMut(TextSlot, &str)) {
    let (message, call) = at;
    match arg {
        Arg::Null | Arg::Bool(_) | Arg::Number(_) => {}
        Arg::Text(text) => {
            let slot = TextSlot::ToolCallArgumentText {
                message,
                call,
                leaf: *leaf,
            };
            *leaf = leaf.saturating_add(1);
            f(slot, text);
        }
        Arg::Array(items) => {
            for item in items {
                walk(item, at, leaf, f);
            }
        }
        Arg::Object(entries) => {
            for (key, value) in entries {
                let slot = TextSlot::ToolCallArgumentKey {
                    message,
                    call,
                    leaf: *leaf,
                };
                *leaf = leaf.saturating_add(1);
                f(slot, key);
                walk(value, at, leaf, f);
            }
        }
    }
}

/// Mutable twin of [`visit_call`]; the order must be identical.
pub(super) fn visit_call_mut(
    message: usize,
    call: usize,
    tool_call: &mut ToolCall,
    f: &mut impl FnMut(TextSlot, &mut String),
) {
    f(TextSlot::ToolCallId { message, call }, &mut tool_call.id);
    f(
        TextSlot::ToolCallName { message, call },
        &mut tool_call.name,
    );
    let mut leaf = 0_usize;
    walk_mut(&mut tool_call.args, (message, call), &mut leaf, f);
}

fn walk_mut(
    arg: &mut Arg,
    at: (usize, usize),
    leaf: &mut usize,
    f: &mut impl FnMut(TextSlot, &mut String),
) {
    let (message, call) = at;
    match arg {
        Arg::Null | Arg::Bool(_) | Arg::Number(_) => {}
        Arg::Text(text) => {
            let slot = TextSlot::ToolCallArgumentText {
                message,
                call,
                leaf: *leaf,
            };
            *leaf = leaf.saturating_add(1);
            f(slot, text);
        }
        Arg::Array(items) => {
            for item in items {
                walk_mut(item, at, leaf, f);
            }
        }
        Arg::Object(entries) => {
            for (key, value) in entries {
                let slot = TextSlot::ToolCallArgumentKey {
                    message,
                    call,
                    leaf: *leaf,
                };
                *leaf = leaf.saturating_add(1);
                f(slot, key);
                walk_mut(value, at, leaf, f);
            }
        }
    }
}

/// Write `,"tool_calls":[...]` with each `arguments` tree re-encoded compactly and escaped
/// as a JSON string.
pub(super) fn write_calls(calls: &[ToolCall], w: &mut Bounded) -> io::Result<()> {
    w.write_all(b",\"tool_calls\":[")?;
    for (index, call) in calls.iter().enumerate() {
        if index > 0 {
            w.write_all(b",")?;
        }
        w.write_all(b"{\"id\":")?;
        json_str(w, &call.id)?;
        w.write_all(b",\"type\":\"function\",\"function\":{\"name\":")?;
        json_str(w, &call.name)?;
        w.write_all(b",\"arguments\":")?;
        let mut encoded = Vec::new();
        encode(&call.args, &mut encoded)?;
        let encoded = String::from_utf8(encoded).map_err(io::Error::other)?;
        json_str(w, &encoded)?;
        w.write_all(b"}}")?;
    }
    w.write_all(b"]")
}

fn encode(arg: &Arg, out: &mut Vec<u8>) -> io::Result<()> {
    match arg {
        Arg::Null => out.write_all(b"null"),
        Arg::Bool(true) => out.write_all(b"true"),
        Arg::Bool(false) => out.write_all(b"false"),
        Arg::Number(n) => serde_json::to_writer(&mut *out, n).map_err(io::Error::from),
        Arg::Text(text) => serde_json::to_writer(&mut *out, text).map_err(io::Error::from),
        Arg::Array(items) => {
            out.write_all(b"[")?;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                encode(item, out)?;
            }
            out.write_all(b"]")
        }
        Arg::Object(entries) => {
            out.write_all(b"{")?;
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                serde_json::to_writer(&mut *out, key).map_err(io::Error::from)?;
                out.write_all(b":")?;
                encode(value, out)?;
            }
            out.write_all(b"}")
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::super::{ChatRequest, classify};
    use super::*;
    use crate::protocol::json::parse_strict;

    fn limits() -> RequestLimits {
        RequestLimits::provisional()
    }

    fn run_with(body: &str, limits: &RequestLimits) -> Checked<ChatRequest> {
        let budget = Budget {
            max_depth: limits.max_depth,
            max_nodes: 1 << 20,
            max_string_bytes: 1 << 20,
            max_total_string_bytes: 1 << 20,
        };
        let doc = parse_budgeted(body.as_bytes(), &budget).map_err(|_| ProtocolError::Malformed)?;
        classify(doc, limits)
    }

    fn run(body: &str) -> Checked<ChatRequest> {
        run_with(body, &limits())
    }

    /// A request with one assistant call whose `arguments` is `args` (JSON text), then a
    /// matching tool result.
    fn with_args(args: &str) -> String {
        serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "f", "arguments": args}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ]
        })
        .to_string()
    }

    fn outcome_of(args: &str) -> Result<(), ProtocolError> {
        run(&with_args(args)).map(|_| ())
    }

    fn wire(request: &ChatRequest) -> serde_json::Value {
        serde_json::from_slice(&request.serialize_bounded(1 << 20).unwrap()).unwrap()
    }

    #[test]
    fn slots_follow_the_contract_order_with_keys_as_labels() {
        let r = run(&with_args(
            r#"{"a":"x","b":{"c":["y",1,null,true,"z"]},"d":2.5}"#,
        ))
        .unwrap();
        let mut seen = Vec::new();
        r.for_each_text(|slot, text| seen.push((slot, text.to_owned())));
        let key = |leaf: usize| TextSlot::ToolCallArgumentKey {
            message: 0,
            call: 0,
            leaf,
        };
        let text = |leaf: usize| TextSlot::ToolCallArgumentText {
            message: 0,
            call: 0,
            leaf,
        };
        let got: Vec<(TextSlot, &str)> = seen.iter().map(|(s, t)| (*s, t.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (
                    TextSlot::ToolCallId {
                        message: 0,
                        call: 0
                    },
                    "c1"
                ),
                (
                    TextSlot::ToolCallName {
                        message: 0,
                        call: 0
                    },
                    "f"
                ),
                (key(0), "a"),
                (text(1), "x"),
                (key(2), "b"),
                (key(3), "c"),
                (text(4), "y"),
                (text(5), "z"),
                (key(6), "d"),
                (TextSlot::ToolResultId { message: 1 }, "c1"),
                (
                    TextSlot::Message {
                        index: 1,
                        part: None
                    },
                    "ok"
                ),
            ]
        );
        assert_eq!(r.redactable_count(), 4);
        assert_eq!(r.text_count(), 11);
    }

    #[test]
    fn round_trip_preserves_structure_and_emits_compact_arguments() {
        let r = run(&with_args(
            r#" { "city" : "서울" , "n" : [ 1 , -2 , 3.5 , true , null , {"k":"vé\"q"} ] } "#,
        ))
        .unwrap();
        let out = wire(&r);
        let call = &out["messages"][0]["tool_calls"][0];
        assert_eq!(call["id"], "c1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "f");
        let args = call["function"]["arguments"].as_str().unwrap();
        assert_eq!(
            args,
            r#"{"city":"서울","n":[1,-2,3.5,true,null,{"k":"vé\"q"}]}"#
        );
        assert!(out["messages"][0]["content"].is_null());
        assert_eq!(out["messages"][1]["tool_call_id"], "c1");
        assert_eq!(out["messages"][1]["content"], "ok");
        // Serializing twice is byte-identical.
        assert_eq!(
            r.serialize_bounded(1 << 20).unwrap(),
            r.serialize_bounded(1 << 20).unwrap()
        );
        // Floats keep f64 precision in canonical form.
        let r = run(&with_args(r#"{"x":1e3}"#)).unwrap();
        assert_eq!(
            wire(&r)["messages"][0]["tool_calls"][0]["function"]["arguments"],
            r#"{"x":1000.0}"#
        );
    }

    #[test]
    fn argument_failures_are_classified() {
        let malformed = Err(ProtocolError::Malformed);
        let unsupported = Err(ProtocolError::Unsupported);
        assert_eq!(outcome_of(""), malformed);
        assert_eq!(outcome_of("{"), malformed);
        assert_eq!(outcome_of(r#"{"a":1,"a":2}"#), malformed);
        assert_eq!(outcome_of(r#"{"a":1} x"#), malformed);
        assert_eq!(outcome_of(r#"{"a":"\ud800"}"#), malformed);
        assert_eq!(outcome_of("[1]"), unsupported);
        assert_eq!(outcome_of("null"), unsupported);
        assert_eq!(outcome_of(r#""x""#), unsupported);
        // Keys outside NAME.
        assert_eq!(outcome_of(r#"{"has space":1}"#), unsupported);
        assert_eq!(outcome_of(r#"{"":1}"#), unsupported);
        assert_eq!(outcome_of(r#"{"a.b":1}"#), unsupported);
        let long = format!(r#"{{"{}":1}}"#, "k".repeat(65));
        assert_eq!(outcome_of(&long), unsupported);
        // Escaped duplicate collides after decoding.
        assert_eq!(outcome_of(r#"{"a":1,"a":2}"#), malformed);
        // Integers beyond i64 are not coerced; i64 extremes are fine.
        assert_eq!(outcome_of(r#"{"a":9223372036854775808}"#), unsupported);
        assert_eq!(outcome_of(r#"{"a":-9223372036854775809}"#), unsupported);
        assert_eq!(
            outcome_of(r#"{"a":123456789012345678901234567890}"#),
            unsupported
        );
        assert_eq!(
            outcome_of(r#"{"a":9223372036854775807,"b":-9223372036854775808}"#),
            Ok(())
        );
        // A long digit run inside a string is just text.
        assert_eq!(
            outcome_of(r#"{"a":"123456789012345678901234567890"}"#),
            Ok(())
        );
        assert_eq!(outcome_of(r#"{"a":1e999}"#), malformed);
        assert_eq!(outcome_of("{}"), Ok(()));
    }

    #[test]
    fn argument_depth_and_derived_budgets_are_limits() {
        // Depth 8 containers including the root is accepted; 9 is a limit.
        let ok = format!("{}1{}", r#"{"a":"#.repeat(8), "}".repeat(8));
        assert_eq!(outcome_of(&ok), Ok(()));
        let deep = format!("{}1{}", r#"{"a":"#.repeat(9), "}".repeat(9));
        assert_eq!(outcome_of(&deep), Err(ProtocolError::LimitExceeded));
        // Derived node budget is request-wide: each call fits, the sum does not.
        let tight = RequestLimits {
            max_nodes: 40,
            ..limits()
        };
        let one = r#"{"a":[1,2,3,4,5,6,7,8,9,10]}"#;
        let call = |id: &str| serde_json::json!({"id": id, "type": "function", "function": {"name": "f", "arguments": one}});
        let body = |n: usize| {
            let calls: Vec<_> = (0..n).map(|i| call(&format!("c{i}"))).collect();
            serde_json::json!({"model":"m","messages":[
                {"role":"assistant","content":null,"tool_calls":calls}]})
            .to_string()
        };
        // 13 nodes per call (object, key, array, 10 numbers); the outer parse stays small.
        assert!(run_with(&body(3), &tight).is_ok());
        assert_eq!(
            run_with(&body(4), &tight).err(),
            Some(ProtocolError::LimitExceeded)
        );
        // Derived decoded bytes are request-wide too.
        let tiny = RequestLimits {
            max_body_bytes: 64,
            ..limits()
        };
        let text = serde_json::json!({"a": "x".repeat(40)}).to_string();
        let two = |id: &str| serde_json::json!({"id": id, "type": "function", "function": {"name": "f", "arguments": text}});
        let both = serde_json::json!({"model":"m","messages":[
            {"role":"assistant","content":null,"tool_calls":[two("a"), two("b")]}]})
        .to_string();
        assert_eq!(
            run_with(&both, &tiny).err(),
            Some(ProtocolError::LimitExceeded)
        );
        // At most 32 calls per message.
        assert!(run(&body(32)).is_ok());
        assert_eq!(run(&body(33)).err(), Some(ProtocolError::LimitExceeded));
    }

    fn msgs(messages: &str) -> Result<(), ProtocolError> {
        run(&format!(r#"{{"model":"m","messages":[{messages}]}}"#)).map(|_| ())
    }

    const CALL: &str = r#"{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}"#;

    fn assistant(call: &str) -> String {
        format!(r#"{{"role":"assistant","content":null,"tool_calls":[{call}]}}"#)
    }

    #[test]
    fn role_content_and_field_consistency() {
        let u = Err(ProtocolError::Unsupported);
        assert_eq!(msgs(&assistant(CALL)), Ok(()));
        // null content only on an assistant with calls.
        assert_eq!(msgs(r#"{"role":"assistant","content":null}"#), u);
        assert_eq!(msgs(r#"{"role":"user","content":null}"#), u);
        assert_eq!(msgs(r#"{"role":"system","content":null}"#), u);
        // Empty tool_calls, wrong type, non-array.
        assert_eq!(
            msgs(r#"{"role":"assistant","content":"x","tool_calls":[]}"#),
            u
        );
        assert_eq!(
            msgs(r#"{"role":"assistant","content":"x","tool_calls":null}"#),
            u
        );
        assert_eq!(
            msgs(r#"{"role":"assistant","content":"x","tool_calls":{}}"#),
            u
        );
        // tool_calls on other roles; tool_call_id on non-tool roles.
        assert_eq!(
            msgs(&format!(
                r#"{{"role":"user","content":"x","tool_calls":[{CALL}]}}"#
            )),
            u
        );
        assert_eq!(
            msgs(&format!(
                r#"{{"role":"tool","tool_call_id":"c1","content":"x","tool_calls":[{CALL}]}}"#
            )),
            u
        );
        assert_eq!(
            msgs(r#"{"role":"user","content":"x","tool_call_id":"c1"}"#),
            u
        );
        assert_eq!(
            msgs(r#"{"role":"assistant","content":"x","tool_call_id":"c1"}"#),
            u
        );
        // Tool message needs id and non-null content.
        let a = assistant(CALL);
        assert_eq!(msgs(&format!(r#"{a},{{"role":"tool","content":"x"}}"#)), u);
        assert_eq!(
            msgs(&format!(
                r#"{a},{{"role":"tool","tool_call_id":"c1","content":null}}"#
            )),
            u
        );
        assert_eq!(
            msgs(&format!(r#"{a},{{"role":"tool","tool_call_id":"c1"}}"#)),
            u
        );
        assert_eq!(
            msgs(&format!(
                r#"{a},{{"role":"tool","tool_call_id":"c1","content":"x"}}"#
            )),
            Ok(())
        );
        assert_eq!(
            msgs(&format!(
                r#"{a},{{"role":"tool","tool_call_id":"c1","content":[{{"type":"text","text":"x"}}]}}"#
            )),
            Ok(())
        );
        // Unknown and legacy fields, name, function role.
        assert_eq!(
            msgs(&format!(
                r#"{a},{{"role":"tool","tool_call_id":"c1","content":"x","name":"f"}}"#
            )),
            u
        );
        assert_eq!(msgs(r#"{"role":"function","name":"f","content":"x"}"#), u);
        assert_eq!(
            msgs(r#"{"role":"assistant","content":"x","function_call":{}}"#),
            u
        );
        for bad in [
            r#"{"id":"c1","type":"function","index":0,"function":{"name":"f","arguments":"{}"}}"#,
            r#"{"id":"c1","type":"function","function":{"name":"f","arguments":"{}","x":1}}"#,
            r#"{"id":"c1","type":"custom","function":{"name":"f","arguments":"{}"}}"#,
            r#"{"id":"c1","function":{"name":"f","arguments":"{}"}}"#,
            r#"{"type":"function","function":{"name":"f","arguments":"{}"}}"#,
            r#"{"id":"c1","type":"function","function":{"arguments":"{}"}}"#,
            r#"{"id":"c1","type":"function","function":{"name":"f"}}"#,
            r#"{"id":"c1","type":"function","function":{"name":"f","arguments":{}}}"#,
            r#"{"id":"c1","type":"function"}"#,
            r#"{"id":"has space","type":"function","function":{"name":"f","arguments":"{}"}}"#,
            r#"{"id":"","type":"function","function":{"name":"f","arguments":"{}"}}"#,
            r#"{"id":"c1","type":"function","function":{"name":"a.b","arguments":"{}"}}"#,
            r#"{"id":"c1","type":"function","function":{"name":"","arguments":"{}"}}"#,
        ] {
            assert_eq!(msgs(&assistant(bad)), u, "{bad}");
        }
        // Label charsets: LINK allows `.` and `:`, bounded to 64 bytes.
        assert_eq!(
            msgs(&assistant(
                r#"{"id":"c.1:x-y_z","type":"function","function":{"name":"f","arguments":"{}"}}"#
            )),
            Ok(())
        );
        let long = "i".repeat(65);
        assert_eq!(
            msgs(&assistant(&format!(
                r#"{{"id":"{long}","type":"function","function":{{"name":"f","arguments":"{{}}"}}}}"#
            ))),
            u
        );
    }

    #[test]
    fn correlation_rules() {
        let u = Err(ProtocolError::Unsupported);
        let call = |id: &str| {
            format!(
                r#"{{"id":"{id}","type":"function","function":{{"name":"f","arguments":"{{}}"}}}}"#
            )
        };
        let two = format!(
            r#"{{"role":"assistant","content":null,"tool_calls":[{},{}]}}"#,
            call("a"),
            call("b")
        );
        let res = |id: &str| format!(r#"{{"role":"tool","tool_call_id":"{id}","content":"r"}}"#);
        let user = r#"{"role":"user","content":"u"}"#;
        // Parallel calls answered in any order; unanswered calls are not enforced.
        assert_eq!(msgs(&format!("{two},{},{}", res("b"), res("a"))), Ok(()));
        assert_eq!(msgs(&format!("{two},{}", res("a"))), Ok(()));
        // Unknown id, answered twice, orphan result, result after an intervening message.
        assert_eq!(msgs(&format!("{two},{}", res("zz"))), u);
        assert_eq!(msgs(&format!("{two},{},{}", res("a"), res("a"))), u);
        assert_eq!(msgs(&res("a")), u);
        assert_eq!(msgs(&format!("{user},{}", res("a"))), u);
        assert_eq!(msgs(&format!("{two},{user},{}", res("a"))), u);
        // A result must follow the assistant message that issued its id, not an earlier one.
        let later = format!(
            r#"{{"role":"assistant","content":null,"tool_calls":[{}]}}"#,
            call("c")
        );
        assert_eq!(msgs(&format!("{two},{},{later},{}", res("a"), res("a"))), u);
        assert_eq!(
            msgs(&format!("{two},{},{later},{}", res("a"), res("c"))),
            Ok(())
        );
        // Ids are unique across the request, across messages and within one.
        assert_eq!(msgs(&format!("{two},{}", assistant(&call("a")))), u);
        let dup = format!(
            r#"{{"role":"assistant","content":null,"tool_calls":[{},{}]}}"#,
            call("x"),
            call("x")
        );
        assert_eq!(msgs(&dup), u);
        // A plain assistant message ends the window.
        assert_eq!(
            msgs(&format!(
                "{two},{{\"role\":\"assistant\",\"content\":\"t\"}},{}",
                res("a")
            )),
            u
        );
    }

    #[test]
    fn revalidation_catches_label_and_bound_violations() {
        let mut r = run(&with_args(r#"{"a":"x"}"#)).unwrap();
        assert!(r.revalidate().is_ok());
        // Replacing an argument key with a non-NAME string (a rewrite of a label) fails.
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolCallArgumentKey { .. }) {
                *text = "has space".to_owned();
            }
        });
        assert_eq!(r.revalidate(), Err(SerializeError::Invalid));

        // A key rewritten onto a sibling's name would collide.
        let mut r = run(&with_args(r#"{"a":1,"b":2}"#)).unwrap();
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolCallArgumentKey { leaf: 1, .. }) {
                *text = "a".to_owned();
            }
        });
        assert_eq!(r.revalidate(), Err(SerializeError::Invalid));

        // A key rewritten to another *valid* name is still a label rewrite.
        let mut r = run(&with_args(r#"{"a":1}"#)).unwrap();
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolCallArgumentKey { .. }) {
                *text = "zz".to_owned();
            }
        });
        assert_eq!(r.revalidate(), Err(SerializeError::Invalid));

        // A rewritten tool-result id (to another issued id) is caught by the digest.
        let two = serde_json::json!({"model":"m","messages":[
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"a","type":"function","function":{"name":"f","arguments":"{}"}},
                {"id":"b","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"a","content":"r"}]})
        .to_string();
        let mut r = run(&two).unwrap();
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolResultId { .. }) {
                *text = "b".to_owned();
            }
        });
        assert_eq!(r.revalidate(), Err(SerializeError::Invalid));

        // A rewritten call id breaks linkage.
        let mut r = run(&with_args("{}")).unwrap();
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolCallId { .. }) {
                *text = "other".to_owned();
            }
        });
        assert_eq!(r.revalidate(), Err(SerializeError::Invalid));

        // A replacement longer than the per-string bound is a limit, not truncated.
        let small = RequestLimits {
            max_string_bytes: 8,
            ..limits()
        };
        let mut r = run_with(&with_args(r#"{"a":"x"}"#), &small).unwrap();
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolCallArgumentText { .. }) {
                *text = "123456789".to_owned();
            }
        });
        assert_eq!(r.revalidate(), Err(SerializeError::Limit));

        // Text replacement of a leaf is fine and re-encodes as a valid argument string.
        let mut r = run(&with_args(r#"{"a":"x"}"#)).unwrap();
        r.for_each_text_mut(|slot, text| {
            if matches!(slot, TextSlot::ToolCallArgumentText { .. }) {
                *text = "<SECRET_1> \"q\"".to_owned();
            }
        });
        assert!(r.revalidate().is_ok());
        let args = wire(&r)["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .to_owned();
        let parsed = parse_strict(args.as_bytes()).unwrap();
        assert!(matches!(parsed, Json::Object(ref e) if e.len() == 1));
    }

    #[test]
    fn debug_output_never_prints_call_content() {
        let r = run(&with_args(r#"{"SYNTHETIC_KEY":"SYNTHETIC_VALUE"}"#)).unwrap();
        let shown = format!("{r:?} {:?}", r.messages());
        assert!(!shown.contains("SYNTHETIC"));
        assert!(!shown.contains("c1"));
    }
}
