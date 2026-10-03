//! `tools`, `tool_choice`, `parallel_tool_calls`: Responses function tool definitions (#85).
//!
//! Contract: `docs/contracts/responses-request.md#tools-function-tools-only-85`. The
//! Responses shapes are flat and differ from Chat: a tool is
//! `{"type":"function","name":..,"description"?:..,"parameters":..|null,"strict":..|null}`
//! (both `parameters` and `strict` are required keys that may be `null`, as in the pinned
//! SDK types) and `tool_choice` is `"none"`, `"auto"`, `"required"`, or the flat
//! `{"type":"function","name":N}`. Names are NAME labels (detect-only); descriptions are
//! redacted text; `parameters` is the Chat schema subset, charged to the same request-wide
//! derived budget. Cross-field rules (`tool_choice` and `parallel_tool_calls` only beside
//! `tools`, a named choice must be declared) run in [`ToolDefs::finish`], because JSON key
//! order is the caller's.

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};

use super::{Checked, ResponsesSlot};
use crate::protocol::chat::schema::{self, Leaf, MAX_DESCRIPTION_BYTES, Schema, is_name};
use crate::protocol::chat::serialize::{Bounded, json_bool, json_str};
use crate::protocol::chat::tool_calls::Derived;
use crate::protocol::chat::{boolean, string, unsupported};
use crate::protocol::json::Json;
use crate::protocol::{ProtocolError, SerializeError};

/// Most function tools in one request.
pub const MAX_TOOLS: usize = 64;

struct Tool {
    name: String,
    description: Option<String>,
    /// `None` is the explicit JSON `null` (the key is required and written).
    parameters: Option<Schema>,
    /// `None` is the explicit JSON `null` (the key is required and written).
    strict: Option<bool>,
}

enum ToolChoice {
    None,
    Auto,
    Required,
    Function(String),
}

/// Parsed function tools and tool-choice state.
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

    /// Cross-field checks after the key loop.
    pub(super) fn finish(&self) -> Checked<()> {
        if self.tools.is_empty() && (self.choice.is_some() || self.parallel_tool_calls.is_some()) {
            return Err(unsupported());
        }
        if let Some(ToolChoice::Function(name)) = &self.choice
            && !self.tools.iter().any(|t| &t.name == name)
        {
            return Err(unsupported());
        }
        Ok(())
    }
}

fn parse_tool(value: Json, derived: &mut Derived) -> Checked<Tool> {
    let Json::Object(entries) = value else {
        return Err(unsupported());
    };
    let mut type_ok = false;
    let mut name = None;
    let mut description = None;
    let mut parameters = None;
    let mut strict = None;
    for (key, value) in entries {
        match key.as_str() {
            "type" => type_ok = string(value)? == "function",
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
            "parameters" => {
                parameters = Some(match value {
                    Json::Null => None,
                    value => Some(schema::parse_root(value, derived)?),
                });
            }
            "strict" => {
                strict = Some(match value {
                    Json::Null => None,
                    value => Some(boolean(value)?),
                });
            }
            // `allowed_callers`, `async`, `defer_loading`, `output_schema`, `namespace`,
            // and everything else.
            _ => return Err(unsupported()),
        }
    }
    if !type_ok {
        return Err(unsupported());
    }
    Ok(Tool {
        name: name.ok_or_else(unsupported)?,
        description,
        parameters: parameters.ok_or_else(unsupported)?,
        strict: strict.ok_or_else(unsupported)?,
    })
}

pub(super) fn parse_tools(defs: &mut ToolDefs, value: Json, derived: &mut Derived) -> Checked<()> {
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
    Ok(())
}

pub(super) fn parse_tool_choice(
    defs: &mut ToolDefs,
    value: Json,
    derived: &mut Derived,
) -> Checked<()> {
    defs.choice = Some(match value {
        Json::String(text) => match text.as_str() {
            "none" => ToolChoice::None,
            "auto" => ToolChoice::Auto,
            "required" => ToolChoice::Required,
            _ => return Err(unsupported()),
        },
        Json::Object(entries) => {
            let mut type_ok = false;
            let mut name = None;
            for (key, value) in entries {
                match key.as_str() {
                    "type" => type_ok = string(value)? == "function",
                    "name" => name = Some(string(value)?),
                    // A Chat-shaped nested `function`, `allowed_tools`, and the rest.
                    _ => return Err(unsupported()),
                }
            }
            match name {
                Some(name) if type_ok && is_name(&name) => {
                    derived.charge(1, name.len())?;
                    ToolChoice::Function(name)
                }
                _ => return Err(unsupported()),
            }
        }
        _ => return Err(unsupported()),
    });
    Ok(())
}

pub(super) fn parse_parallel(defs: &mut ToolDefs, value: Json) -> Checked<()> {
    defs.parallel_tool_calls = Some(boolean(value)?);
    Ok(())
}

const fn slot_of(tool: usize, leaf: Leaf) -> ResponsesSlot {
    match leaf {
        Leaf::Label(leaf) => ResponsesSlot::ToolSchemaLabel { tool, leaf },
        Leaf::Text(leaf) => ResponsesSlot::ToolSchemaText { tool, leaf },
    }
}

/// Visit tool texts (traversal position 3), then the `tool_choice` name (position 4).
pub(super) fn visit(defs: &ToolDefs, f: &mut impl FnMut(ResponsesSlot, &str)) {
    for (tool, def) in defs.tools.iter().enumerate() {
        f(ResponsesSlot::ToolName { tool }, &def.name);
        if let Some(text) = &def.description {
            f(ResponsesSlot::ToolDescription { tool }, text);
        }
        if let Some(parameters) = &def.parameters {
            let mut leaf = 0_usize;
            schema::visit(parameters, &mut leaf, &mut |kind, text| {
                f(slot_of(tool, kind), text);
            });
        }
    }
    if let Some(ToolChoice::Function(name)) = &defs.choice {
        f(ResponsesSlot::ToolChoiceName, name);
    }
}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(defs: &mut ToolDefs, f: &mut impl FnMut(ResponsesSlot, &mut String)) {
    for (tool, def) in defs.tools.iter_mut().enumerate() {
        f(ResponsesSlot::ToolName { tool }, &mut def.name);
        if let Some(text) = &mut def.description {
            f(ResponsesSlot::ToolDescription { tool }, text);
        }
        if let Some(parameters) = &mut def.parameters {
            let mut leaf = 0_usize;
            schema::visit_mut(parameters, &mut leaf, &mut |kind, text| {
                f(slot_of(tool, kind), text);
            });
        }
    }
    if let Some(ToolChoice::Function(name)) = &mut defs.choice {
        f(ResponsesSlot::ToolChoiceName, name);
    }
}

/// Write `,"tools":...,"tool_choice":...,"parallel_tool_calls":...` when present. Within a
/// tool: `type`, `name`, `description`, `parameters`, `strict`.
pub(super) fn write(defs: &ToolDefs, w: &mut Bounded) -> io::Result<()> {
    if !defs.tools.is_empty() {
        w.write_all(b",\"tools\":[")?;
        for (index, tool) in defs.tools.iter().enumerate() {
            if index > 0 {
                w.write_all(b",")?;
            }
            w.write_all(b"{\"type\":\"function\",\"name\":")?;
            json_str(w, &tool.name)?;
            if let Some(text) = &tool.description {
                w.write_all(b",\"description\":")?;
                json_str(w, text)?;
            }
            w.write_all(b",\"parameters\":")?;
            match &tool.parameters {
                Some(parameters) => schema::write(parameters, w)?,
                None => w.write_all(b"null")?,
            }
            w.write_all(b",\"strict\":")?;
            match tool.strict {
                Some(strict) => json_bool(w, strict)?,
                None => w.write_all(b"null")?,
            }
            w.write_all(b"}")?;
        }
        w.write_all(b"]")?;
    }
    match &defs.choice {
        Some(ToolChoice::None) => w.write_all(b",\"tool_choice\":\"none\"")?,
        Some(ToolChoice::Auto) => w.write_all(b",\"tool_choice\":\"auto\"")?,
        Some(ToolChoice::Required) => w.write_all(b",\"tool_choice\":\"required\"")?,
        Some(ToolChoice::Function(name)) => {
            w.write_all(b",\"tool_choice\":{\"type\":\"function\",\"name\":")?;
            json_str(w, name)?;
            w.write_all(b"}")?;
        }
        None => {}
    }
    if let Some(flag) = defs.parallel_tool_calls {
        w.write_all(b",\"parallel_tool_calls\":")?;
        json_bool(w, flag)?;
    }
    Ok(())
}

/// Revalidation after mutation: names within charset and still unique, descriptions within
/// their bound after redaction, the named choice still declared, every schema well formed.
pub(super) fn revalidate(defs: &ToolDefs) -> Result<(), SerializeError> {
    if defs.tools.is_empty() && (defs.choice.is_some() || defs.parallel_tool_calls.is_some()) {
        return Err(SerializeError::Invalid);
    }
    if defs.tools.len() > MAX_TOOLS {
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
