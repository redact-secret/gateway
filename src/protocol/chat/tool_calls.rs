//! Tool history: assistant `tool_calls`, `role: tool` results, `tool_call_id`, nullable
//! assistant content, correlation, and the decoded `function.arguments` tree.
//! Owner: #53. Contract: `docs/contracts/chat-completions-request.md`, ADR 0025.
//!
//! Today every one of these forms is rejected. This module is wired in so #53 edits only
//! this file and `messages.rs`:
//!
//! - [`parse_message_field`] is called by `messages::parse_message` for the keys
//!   `tool_calls` and `tool_call_id`.
//! - [`validate_history`] is called by `classify` once all messages are parsed
//!   (correlation, ordering, uniqueness).
//! - [`revalidate`] is called by [`super::ChatRequest::revalidate`] after text mutation
//!   (re-encode every decoded `arguments` tree and verify the shape is unchanged).
//! - Slot visiting stays in `messages::visit` / `visit_mut` and writing in
//!   `messages::write`, because those are per-message.

use super::{Checked, Message, SerializeError, unsupported};
use crate::protocol::json::Json;

/// A tool-history key on a message (`tool_calls`, `tool_call_id`). Rejected until #53.
pub(super) fn parse_message_field(_key: &str, _value: Json) -> Checked<()> {
    Err(unsupported())
}

/// Cross-message tool-call correlation. No tool history is accepted yet.
pub(super) const fn validate_history(_messages: &[Message]) -> Checked<()> {
    Ok(())
}

/// Revalidation after mutation. No decoded argument tree exists yet.
pub(super) const fn revalidate(_messages: &[Message]) -> Result<(), SerializeError> {
    Ok(())
}
