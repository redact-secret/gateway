//! `tools`, `tool_choice`, `parallel_tool_calls`: function tool definitions.
//! Owner: #54 (with `schema.rs` and `response_format.rs`). Contract:
//! `docs/contracts/chat-completions-request.md`, ADR 0025.
//!
//! Today `tools`, `tool_choice` and `parallel_tool_calls` are all rejected. `classify`
//! already routes those three keys to [`parse_field`] and calls [`finish`] after the key
//! loop, so #54 edits only this file, `schema.rs` and `response_format.rs`.

use std::io;

use super::serialize::Bounded;
use super::{Checked, RequestLimits, SerializeError, TextSlot, unsupported};
use crate::protocol::json::Json;

/// Parsed tool definitions and tool-choice state. Fields are added by #54.
#[derive(Debug, Default)]
pub struct ToolDefs {}

/// One of `tools`, `tool_choice`, `parallel_tool_calls`. Rejected until #54.
pub(super) fn parse_field(
    _defs: &mut ToolDefs,
    _key: &str,
    _value: Json,
    _limits: &RequestLimits,
) -> Checked<()> {
    Err(unsupported())
}

/// Cross-field checks after the key loop (for example `tool_choice` names a declared tool,
/// `tool_choice` and `parallel_tool_calls` require `tools`).
pub(super) const fn finish(_defs: &ToolDefs) -> Checked<()> {
    Ok(())
}

/// Visit tool-definition texts (traversal position 2 in `slots.rs`).
pub(super) fn visit(_defs: &ToolDefs, _f: &mut impl FnMut(TextSlot, &str)) {}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(_defs: &mut ToolDefs, _f: &mut impl FnMut(TextSlot, &mut String)) {}

/// Write `,"tools":...,"tool_choice":...,"parallel_tool_calls":...` when present.
pub(super) const fn write(_defs: &ToolDefs, _w: &mut Bounded) -> io::Result<()> {
    Ok(())
}

/// Revalidation after mutation.
pub(super) const fn revalidate(_defs: &ToolDefs) -> Result<(), SerializeError> {
    Ok(())
}
