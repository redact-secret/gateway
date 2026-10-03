//! `tools`, `tool_choice`, `parallel_tool_calls`: function tool definitions.
//! Owner: #54 (with `schema.rs` and `response_format.rs`). Contract:
//! `docs/contracts/chat-completions-request.md`, ADR 0025 D5, D6, D11.
//!
//! Function tools only. A tool is `{"type":"function","function":{name, description?,
//! parameters?, strict?}}`: `name` is a NAME label (detect-only, unique across `tools`),
//! `description` is text (redacted in place), `parameters` is the bounded schema subset in
//! `schema.rs` with an object root, and `strict` is a preserved boolean. `tool_choice` is
//! `"none"`, `"auto"`, `"required"`, or a named function that must be one of the declared
//! tools; `tool_choice` and `parallel_tool_calls` are accepted only beside `tools`. The
//! cross-field checks run in [`finish`] because JSON key order is the caller's.

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};

use super::schema::{self, Leaf, MAX_DESCRIPTION_BYTES, Schema, is_name};
use super::serialize::{Bounded, json_bool, json_str};
use super::tool_calls::Derived;
use super::{Checked, SerializeError, TextSlot, boolean, string, unsupported};
use crate::protocol::ProtocolError;
use crate::protocol::json::Json;

/// Most function tools in one request.
pub const MAX_TOOLS: usize = 64;

struct Tool {
    name: String,
    description: Option<String>,
    parameters: Option<Schema>,
    strict: Option<bool>,
}

enum ToolChoice {
    None,
    Auto,
    Required,
    Function(String),
}

/// Parsed tool definitions and tool-choice state.
#[derive(Default)]
pub struct ToolDefs {
    tools: Vec<Tool>,
    choice: Option<ToolChoice>,
    parallel_tool_calls: Option<bool>,
}

impl fmt::Debug for ToolDefs {
    /// Prints only the count.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ToolDefs({})", self.tools.len())
    }
}

impl ToolDefs {
    /// Number of declared tools.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

fn parse_tool(value: Json, derived: &mut Derived) -> Checked<Tool> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut type_ok = false;
    let mut function = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => type_ok = string(value)? == "function",
            "function" => function = Some(value),
            _ => return Err(unsupported()),
        }
    }
    let Some(Json::Object(entries)) = function else {
        return Err(unsupported());
    };
    if !type_ok {
        return Err(unsupported());
    }
    let mut name = None;
    let mut description = None;
    let mut parameters = None;
    let mut strict = None;
    for (key, value) in entries {
        match key.as_str() {
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
            "parameters" => parameters = Some(schema::parse_root(value, derived)?),
            "strict" => strict = Some(boolean(value)?),
            _ => return Err(unsupported()),
        }
    }
    Ok(Tool {
        name: name.ok_or_else(unsupported)?,
        description,
        parameters,
        strict,
    })
}

fn parse_tool_choice(value: Json, derived: &mut Derived) -> Checked<ToolChoice> {
    match value {
        Json::String(text) => match text.as_str() {
            "none" => Ok(ToolChoice::None),
            "auto" => Ok(ToolChoice::Auto),
            "required" => Ok(ToolChoice::Required),
            _ => Err(unsupported()),
        },
        Json::Object(entries) => {
            let mut type_ok = false;
            let mut name = None;
            for (key, value) in entries {
                match key.as_str() {
                    "type" => type_ok = string(value)? == "function",
                    "function" => {
                        let Json::Object(inner) = value else {
                            return Err(unsupported());
                        };
                        for (key, value) in inner {
                            if key != "name" {
                                return Err(unsupported());
                            }
                            name = Some(string(value)?);
                        }
                    }
                    _ => return Err(unsupported()),
                }
            }
            match name {
                Some(name) if type_ok && is_name(&name) => {
                    derived.charge(1, name.len())?;
                    Ok(ToolChoice::Function(name))
                }
                _ => Err(unsupported()),
            }
        }
        _ => Err(unsupported()),
    }
}

/// One of `tools`, `tool_choice`, `parallel_tool_calls`.
pub(super) fn parse_field(
    defs: &mut ToolDefs,
    key: &str,
    value: Json,
    derived: &mut Derived,
) -> Checked<()> {
    match key {
        "tools" => {
            let Json::Array(items) = value else {
                return Err(unsupported());
            };
            if items.is_empty() {
                return Err(unsupported());
            }
            if items.len() > MAX_TOOLS {
                return Err(ProtocolError::LimitExceeded);
            }
            let mut names = HashSet::with_capacity(items.len());
            let mut tools = Vec::with_capacity(items.len());
            for item in items {
                let tool = parse_tool(item, derived)?;
                if !names.insert(tool.name.clone()) {
                    return Err(unsupported());
                }
                tools.push(tool);
            }
            defs.tools = tools;
        }
        "tool_choice" => defs.choice = Some(parse_tool_choice(value, derived)?),
        "parallel_tool_calls" => defs.parallel_tool_calls = Some(boolean(value)?),
        _ => return Err(unsupported()),
    }
    Ok(())
}

fn declared(defs: &ToolDefs, name: &str) -> bool {
    defs.tools.iter().any(|t| t.name == name)
}

/// Cross-field checks after the key loop: `tool_choice` and `parallel_tool_calls` require
/// `tools`, and a named `tool_choice` must be a declared tool.
pub(super) fn finish(defs: &ToolDefs) -> Checked<()> {
    if defs.tools.is_empty() && (defs.choice.is_some() || defs.parallel_tool_calls.is_some()) {
        return Err(unsupported());
    }
    if let Some(ToolChoice::Function(name)) = &defs.choice
        && !declared(defs, name)
    {
        return Err(unsupported());
    }
    Ok(())
}

/// Visit tool-definition texts (traversal position 2 in `slots.rs`).
pub(super) fn visit(defs: &ToolDefs, f: &mut impl FnMut(TextSlot, &str)) {
    for (tool, def) in defs.tools.iter().enumerate() {
        f(TextSlot::ToolDefName { tool }, &def.name);
        if let Some(text) = &def.description {
            f(TextSlot::ToolDefDescription { tool }, text);
        }
        if let Some(parameters) = &def.parameters {
            let mut leaf = 0_usize;
            schema::visit(parameters, &mut leaf, &mut |kind, text| match kind {
                Leaf::Label(leaf) => f(TextSlot::ToolDefSchemaLabel { tool, leaf }, text),
                Leaf::Text(leaf) => f(TextSlot::ToolDefSchemaText { tool, leaf }, text),
            });
        }
    }
    if let Some(ToolChoice::Function(name)) = &defs.choice {
        f(TextSlot::ToolChoiceName, name);
    }
}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(defs: &mut ToolDefs, f: &mut impl FnMut(TextSlot, &mut String)) {
    for (tool, def) in defs.tools.iter_mut().enumerate() {
        f(TextSlot::ToolDefName { tool }, &mut def.name);
        if let Some(text) = &mut def.description {
            f(TextSlot::ToolDefDescription { tool }, text);
        }
        if let Some(parameters) = &mut def.parameters {
            let mut leaf = 0_usize;
            schema::visit_mut(parameters, &mut leaf, &mut |kind, text| match kind {
                Leaf::Label(leaf) => f(TextSlot::ToolDefSchemaLabel { tool, leaf }, text),
                Leaf::Text(leaf) => f(TextSlot::ToolDefSchemaText { tool, leaf }, text),
            });
        }
    }
    if let Some(ToolChoice::Function(name)) = &mut defs.choice {
        f(TextSlot::ToolChoiceName, name);
    }
}

/// Write `,"tools":...,"tool_choice":...,"parallel_tool_calls":...` when present.
pub(super) fn write(defs: &ToolDefs, w: &mut Bounded) -> io::Result<()> {
    if !defs.tools.is_empty() {
        w.write_all(b",\"tools\":[")?;
        for (index, tool) in defs.tools.iter().enumerate() {
            if index > 0 {
                w.write_all(b",")?;
            }
            w.write_all(b"{\"type\":\"function\",\"function\":{\"name\":")?;
            json_str(w, &tool.name)?;
            if let Some(text) = &tool.description {
                w.write_all(b",\"description\":")?;
                json_str(w, text)?;
            }
            if let Some(parameters) = &tool.parameters {
                w.write_all(b",\"parameters\":")?;
                schema::write(parameters, w)?;
            }
            if let Some(strict) = tool.strict {
                w.write_all(b",\"strict\":")?;
                json_bool(w, strict)?;
            }
            w.write_all(b"}}")?;
        }
        w.write_all(b"]")?;
    }
    match &defs.choice {
        Some(ToolChoice::None) => w.write_all(b",\"tool_choice\":\"none\"")?,
        Some(ToolChoice::Auto) => w.write_all(b",\"tool_choice\":\"auto\"")?,
        Some(ToolChoice::Required) => w.write_all(b",\"tool_choice\":\"required\"")?,
        Some(ToolChoice::Function(name)) => {
            w.write_all(b",\"tool_choice\":{\"type\":\"function\",\"function\":{\"name\":")?;
            json_str(w, name)?;
            w.write_all(b"}}")?;
        }
        None => {}
    }
    if let Some(flag) = defs.parallel_tool_calls {
        w.write_all(b",\"parallel_tool_calls\":")?;
        json_bool(w, flag)?;
    }
    Ok(())
}

/// Revalidation after mutation: labels still within their charset and length, names still
/// unique, descriptions within their bound after redaction, the named `tool_choice` still a
/// declared tool, and every schema still well formed. Nothing is repaired.
pub(super) fn revalidate(defs: &ToolDefs) -> Result<(), SerializeError> {
    if defs.tools.is_empty() && (defs.choice.is_some() || defs.parallel_tool_calls.is_some()) {
        return Err(SerializeError::Invalid);
    }
    let mut names = HashSet::with_capacity(defs.tools.len());
    for tool in &defs.tools {
        if !is_name(&tool.name) || !names.insert(tool.name.as_str()) {
            return Err(SerializeError::Invalid);
        }
        if tool
            .description
            .as_ref()
            .is_some_and(|d| d.len() > MAX_DESCRIPTION_BYTES)
        {
            return Err(SerializeError::Limit);
        }
        if let Some(parameters) = &tool.parameters {
            schema::revalidate(parameters)?;
        }
    }
    if let Some(ToolChoice::Function(name)) = &defs.choice
        && !names.contains(name.as_str())
    {
        return Err(SerializeError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{ChatRequest, RequestLimits, classify};
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

    fn wrap(extra: &str) -> String {
        format!(r#"{{"model":"m","messages":[{{"role":"user","content":"q"}}],{extra}}}"#)
    }

    fn tool(parameters: &str) -> String {
        wrap(&format!(
            r#""tools":[{{"type":"function","function":{{"name":"f","parameters":{parameters}}}}}]"#
        ))
    }

    #[test]
    fn full_tool_definition_round_trips_in_canonical_order() {
        let body = wrap(concat!(
            r#""parallel_tool_calls":false,"#,
            r#""tool_choice":{"function":{"name":"get_weather"},"type":"function"},"#,
            r#""tools":[{"function":{"strict":true,"parameters":{"additionalProperties":false,"#,
            r#""required":["city","unit"],"properties":{"city":{"description":"도시 이름","type":"string","maxLength":40},"#,
            r#""unit":{"enum":["celsius","fahrenheit"],"type":["string","null"]},"#,
            r#""n":{"type":"integer","minimum":1,"maximum":1e2},"#,
            r#""tags":{"type":"array","items":{"type":"string","const":"x y"},"minItems":0,"maxItems":8},"#,
            r#""any":{"anyOf":[{"type":"boolean"},{"type":"null"}]}},"type":"object"},"#,
            r#""description":"Look up weather.","name":"get_weather"},"type":"function"}]"#
        ));
        let r = run(&body).expect("supported");
        let out = r.serialize_bounded(8192).expect("fits");
        let text = std::str::from_utf8(&out).expect("utf8");
        assert_eq!(
            text,
            concat!(
                r#"{"model":"m","messages":[{"role":"user","content":"q"}],"#,
                r#""tools":[{"type":"function","function":{"name":"get_weather","description":"Look up weather.","#,
                r#""parameters":{"type":"object","properties":{"#,
                r#""city":{"type":"string","description":"도시 이름","maxLength":40},"#,
                r#""unit":{"type":["string","null"],"enum":["celsius","fahrenheit"]},"#,
                r#""n":{"type":"integer","minimum":1,"maximum":100.0},"#,
                r#""tags":{"type":"array","items":{"type":"string","const":"x y"},"minItems":0,"maxItems":8},"#,
                r#""any":{"anyOf":[{"type":"boolean"},{"type":"null"}]}},"#,
                r#""required":["city","unit"],"additionalProperties":false},"strict":true}}],"#,
                r#""tool_choice":{"type":"function","function":{"name":"get_weather"}},"#,
                r#""parallel_tool_calls":false}"#
            )
        );
        // The fresh document satisfies the same matrix and serializes identically.
        let again = run(text).expect("round trip");
        assert_eq!(again.serialize_bounded(8192).expect("fits"), out);
        assert!(r.revalidate().is_ok());
    }

    #[test]
    fn slot_order_and_modes_follow_the_contract() {
        let r = run(&wrap(concat!(
            r#""tools":[{"type":"function","function":{"name":"a","description":"da","parameters":"#,
            r#"{"type":"object","description":"root","properties":{"p":{"type":"string","description":"dp","enum":["e1"]}},"required":["p"]}}},"#,
            r#"{"type":"function","function":{"name":"b"}}],"tool_choice":{"type":"function","function":{"name":"b"}}"#
        )))
        .expect("supported");
        let mut seen = Vec::new();
        r.for_each_text(|slot, text| seen.push((slot, slot.mode(), text.to_owned())));
        let shape: Vec<(TextSlot, &str)> = seen.iter().map(|(s, _, t)| (*s, t.as_str())).collect();
        assert_eq!(
            shape,
            vec![
                (
                    TextSlot::Message {
                        index: 0,
                        part: None
                    },
                    "q"
                ),
                (TextSlot::ToolDefName { tool: 0 }, "a"),
                (TextSlot::ToolDefDescription { tool: 0 }, "da"),
                (TextSlot::ToolDefSchemaText { tool: 0, leaf: 0 }, "root"),
                (TextSlot::ToolDefSchemaLabel { tool: 0, leaf: 1 }, "p"),
                (TextSlot::ToolDefSchemaText { tool: 0, leaf: 2 }, "dp"),
                (TextSlot::ToolDefSchemaLabel { tool: 0, leaf: 3 }, "e1"),
                (TextSlot::ToolDefSchemaLabel { tool: 0, leaf: 4 }, "p"),
                (TextSlot::ToolDefName { tool: 1 }, "b"),
                (TextSlot::ToolChoiceName, "b"),
            ]
        );
        assert_eq!(r.text_count(), 10);
        // Mutable traversal matches the read-only one.
        let mut r = r;
        let mut mutable = Vec::new();
        r.for_each_text_mut(|slot, text| mutable.push((slot, text.clone())));
        let read: Vec<(TextSlot, String)> = seen.into_iter().map(|(s, _, t)| (s, t)).collect();
        assert_eq!(mutable, read);
    }

    #[test]
    fn accepted_forms() {
        for choice in [r#""none""#, r#""auto""#, r#""required""#] {
            let body = wrap(&format!(
                r#""tools":[{{"type":"function","function":{{"name":"f"}}}}],"tool_choice":{choice},"parallel_tool_calls":true"#
            ));
            assert!(run(&body).is_ok(), "{choice}");
        }
        // Empty properties and empty required are fine; `type` array of one; zero bounds.
        assert!(
            run(&tool(
                r#"{"type":"object","properties":{},"required":[],"additionalProperties":true}"#
            ))
            .is_ok()
        );
        assert!(
            run(&tool(
                r#"{"type":"object","properties":{"a":{"type":["integer"],"minimum":-3.5,"maxLength":0}}}"#
            ))
            .is_ok()
        );
        // Enum members may be numbers, booleans, null, and labels.
        assert!(
            run(&tool(
                r#"{"type":"object","properties":{"a":{"enum":[1,2.5,true,null,"x"]}}}"#
            ))
            .is_ok()
        );
    }

    #[test]
    fn rejected_forms_are_unsupported() {
        let cases = [
            // tools
            wrap(r#""tools":[]"#),
            wrap(r#""tools":{}"#),
            wrap(r#""tools":[{"type":"custom","custom":{"name":"f"}}]"#),
            wrap(r#""tools":[{"type":"function"}]"#),
            wrap(r#""tools":[{"function":{"name":"f"}}]"#),
            wrap(r#""tools":[{"type":"function","function":{}}]"#),
            wrap(r#""tools":[{"type":"function","function":{"name":"bad name!"}}]"#),
            wrap(r#""tools":[{"type":"function","function":{"name":""}}]"#),
            wrap(r#""tools":[{"type":"function","function":{"name":"f","extra":1}}]"#),
            wrap(r#""tools":[{"type":"function","extra":1,"function":{"name":"f"}}]"#),
            wrap(r#""tools":[{"type":"function","function":{"name":"f","strict":"yes"}}]"#),
            wrap(r#""tools":[{"type":"function","function":{"name":"f","description":7}}]"#),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}},{"type":"function","function":{"name":"f"}}]"#,
            ),
            // tool_choice and parallel_tool_calls
            wrap(r#""tool_choice":"auto""#),
            wrap(r#""parallel_tool_calls":true"#),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"function","function":{"name":"g"}}"#,
            ),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"sometimes""#,
            ),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"function"}"#,
            ),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"allowed_tools","allowed_tools":{}}"#,
            ),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"function","function":{"name":"f","x":1}}"#,
            ),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"custom","custom":{"name":"f"}}"#,
            ),
            wrap(
                r#""tools":[{"type":"function","function":{"name":"f"}}],"parallel_tool_calls":"x""#,
            ),
            // schema: root and types
            tool(r#"{}"#),
            tool(r#"{"type":"array"}"#),
            tool(r#"{"type":["object","null"]}"#),
            tool(r#"[]"#),
            tool(r#"true"#),
            tool(r#"{"type":"object","properties":{"a":{"type":"date"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":[]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":["string","string"]}}}"#),
            tool(r#"{"type":"object","properties":{"a":true}}"#),
            // schema: references and rejected keywords
            tool(r#"{"type":"object","$ref":"defs-a"}"#),
            tool(
                r#"{"type":"object","properties":{"a":{"$ref":"https://example.invalid/s.json"}}}"#,
            ),
            tool(r#"{"type":"object","$defs":{}}"#),
            tool(r#"{"type":"object","definitions":{}}"#),
            tool(r#"{"type":"object","$id":"x"}"#),
            tool(r#"{"type":"object","$schema":"https://json-schema.org/draft/2020-12/schema"}"#),
            tool(r#"{"type":"object","allOf":[{"type":"object"}]}"#),
            tool(r#"{"type":"object","oneOf":[{"type":"object"}]}"#),
            tool(r#"{"type":"object","not":{"type":"object"}}"#),
            tool(r#"{"type":"object","if":{"type":"object"}}"#),
            tool(r#"{"type":"object","patternProperties":{}}"#),
            tool(r#"{"type":"object","propertyNames":{}}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":"string","pattern":"^a"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":"string","format":"email"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":"string","default":"x"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":"string","examples":["x"]}}}"#),
            tool(r#"{"type":"object","title":5}"#),
            tool(r#"{"type":"object","x-vendor":1}"#),
            tool(r#"{"type":"object","properties":{"a":{"type":"string","unknownKeyword":1}}}"#),
            // schema: value shapes
            tool(r#"{"type":"object","additionalProperties":{"type":"string"}}"#),
            tool(r#"{"type":"object","additionalProperties":"false"}"#),
            tool(r#"{"type":"object","properties":[]}"#),
            tool(r#"{"type":"object","properties":{"bad key":{"type":"string"}}}"#),
            tool(r#"{"type":"object","properties":{"":{"type":"string"}}}"#),
            tool(r#"{"type":"object","required":"a"}"#),
            tool(r#"{"type":"object","required":["a","a"]}"#),
            tool(r#"{"type":"object","required":["bad key"]}"#),
            tool(r#"{"type":"object","required":[1]}"#),
            tool(r#"{"type":"object","properties":{"a":{"enum":[]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"enum":"x"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"enum":[["x"]]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"enum":[{"k":"v"}]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"enum":["<tag>"]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"enum":["quote\"d"]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"const":{"k":1}}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"const":"line\nbreak"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"items":[{"type":"string"}]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"anyOf":[]}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"anyOf":{}}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"minimum":"1"}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"minimum":null}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"minimum":18446744073709551615}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"minLength":-1}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"minLength":1.5}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"maxItems":1.0}}}"#),
            tool(r#"{"type":"object","properties":{"a":{"description":7}}}"#),
        ];
        for body in &cases {
            assert_eq!(run(body).unwrap_err(), ProtocolError::Unsupported, "{body}");
        }
        // The shell itself is fine.
        assert!(run(&tool(r#"{"type":"object"}"#)).is_ok());
    }

    #[test]
    fn counts_and_depth_over_the_bounds_are_limit_failures() {
        let make_tools = |n: usize| {
            let items: Vec<String> = (0..n)
                .map(|i| format!(r#"{{"type":"function","function":{{"name":"f{i}"}}}}"#))
                .collect();
            wrap(&format!(r#""tools":[{}]"#, items.join(",")))
        };
        assert!(run(&make_tools(64)).is_ok());
        assert_eq!(
            run(&make_tools(65)).unwrap_err(),
            ProtocolError::LimitExceeded
        );

        let props = |n: usize| {
            let items: Vec<String> = (0..n)
                .map(|i| format!(r#""p{i}":{{"type":"string"}}"#))
                .collect();
            tool(&format!(
                r#"{{"type":"object","properties":{{{}}}}}"#,
                items.join(",")
            ))
        };
        assert!(run(&props(64)).is_ok());
        assert_eq!(run(&props(65)).unwrap_err(), ProtocolError::LimitExceeded);

        let en = |n: usize| {
            let items: Vec<String> = (0..n).map(|i| format!(r#""v{i}""#)).collect();
            tool(&format!(
                r#"{{"type":"object","properties":{{"a":{{"enum":[{}]}}}}}}"#,
                items.join(",")
            ))
        };
        assert!(run(&en(64)).is_ok());
        assert_eq!(run(&en(65)).unwrap_err(), ProtocolError::LimitExceeded);

        let any = |n: usize| {
            let items = vec![r#"{"type":"null"}"#; n].join(",");
            tool(&format!(
                r#"{{"type":"object","properties":{{"a":{{"anyOf":[{items}]}}}}}}"#
            ))
        };
        assert!(run(&any(8)).is_ok());
        assert_eq!(run(&any(9)).unwrap_err(), ProtocolError::LimitExceeded);

        // Depth: root is 1, so seven nested `items` below it reach depth 8; one more is 9.
        let deep = |levels: usize| {
            let mut s = r#"{"type":"string"}"#.to_owned();
            for _ in 0..levels {
                s = format!(r#"{{"type":"array","items":{s}}}"#);
            }
            tool(&format!(r#"{{"type":"object","properties":{{"a":{s}}}}}"#))
        };
        // Wrapper depth: body{ tools[ tool{ function{ parameters{ properties{ a{ ...
        // The request-wide container limit (16) or the schema limit (8) rejects first.
        assert!(run(&deep(2)).is_ok());
        assert_eq!(run(&deep(7)).unwrap_err(), ProtocolError::LimitExceeded);

        let big = "a".repeat(MAX_DESCRIPTION_BYTES + 1);
        let long = tool(&format!(r#"{{"type":"object","description":"{big}"}}"#));
        assert_eq!(run(&long).unwrap_err(), ProtocolError::LimitExceeded);
        let ok = "a".repeat(MAX_DESCRIPTION_BYTES);
        assert!(
            run(&tool(&format!(
                r#"{{"type":"object","description":"{ok}"}}"#
            )))
            .is_ok()
        );
    }

    #[test]
    fn schema_object_count_is_bounded_per_schema() {
        let items = [r#"{"type":"null"}"#; 8].join(",");
        let group = format!(r#"{{"anyOf":[{items}]}}"#);
        // 1 root + 1 property holder per group... build 30 properties of 9 objects each
        // (270 objects) which exceeds 256 while each count stays under its own bound.
        let props: Vec<String> = (0..30).map(|i| format!(r#""p{i}":{group}"#)).collect();
        let body = tool(&format!(
            r#"{{"type":"object","properties":{{{}}}}}"#,
            props.join(",")
        ));
        assert_eq!(run(&body).unwrap_err(), ProtocolError::LimitExceeded);
    }

    #[test]
    fn debug_never_prints_names_or_text() {
        let r = run(&tool(
            r#"{"type":"object","properties":{"SYNTHETIC_KEY":{"type":"string","description":"SYNTHETIC_TEXT"}}}"#,
        ))
        .expect("supported");
        let shown = format!("{r:?} {:?}", r.tool_defs);
        assert!(!shown.contains("SYNTHETIC"), "{shown}");
        assert_eq!(r.tool_defs.tool_count(), 1);
    }

    #[test]
    fn revalidation_catches_broken_labels_bounds_and_linkage() {
        let body = wrap(concat!(
            r#""tools":[{"type":"function","function":{"name":"f","description":"d","parameters":"#,
            r#"{"type":"object","properties":{"a":{"type":"string","enum":["x"]}},"required":["a"]}}}],"#,
            r#""tool_choice":{"type":"function","function":{"name":"f"}}"#
        ));
        let fresh = || run(&body).expect("supported");
        assert!(fresh().revalidate().is_ok());

        // A description that grew past its bound after redaction.
        let mut r = fresh();
        r.for_each_text_mut(|slot, t| {
            if matches!(slot, TextSlot::ToolDefDescription { .. }) {
                *t = "x".repeat(MAX_DESCRIPTION_BYTES + 1);
            }
        });
        assert_eq!(r.revalidate().unwrap_err(), SerializeError::Limit);

        // A rewritten label (never allowed) breaks the charset check.
        for target in ["name", "key", "required", "enum", "choice"] {
            let mut r = fresh();
            r.for_each_text_mut(|slot, t| {
                let hit = matches!(
                    (target, slot),
                    ("name", TextSlot::ToolDefName { .. })
                        | ("key", TextSlot::ToolDefSchemaLabel { leaf: 0, .. })
                        | ("required", TextSlot::ToolDefSchemaLabel { leaf: 2, .. })
                        | ("enum", TextSlot::ToolDefSchemaLabel { leaf: 1, .. })
                        | ("choice", TextSlot::ToolChoiceName)
                );
                if hit {
                    *t = "<SECRET 1>".to_owned();
                }
            });
            assert_eq!(
                r.revalidate().unwrap_err(),
                SerializeError::Invalid,
                "{target}"
            );
        }

        // tool_choice that no longer names a declared tool.
        let mut r = fresh();
        r.for_each_text_mut(|slot, t| {
            if matches!(slot, TextSlot::ToolChoiceName) {
                "other".clone_into(t);
            }
        });
        assert_eq!(r.revalidate().unwrap_err(), SerializeError::Invalid);
    }
}
