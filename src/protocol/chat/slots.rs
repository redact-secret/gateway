//! Text slots and deterministic traversal (ADR 0025). Owner: the #52 baseline; each
//! follow-up adds only the lines marked for it.
//!
//! A slot is one decoded string that the gateway hands to the core. Every slot has a
//! [`SlotMode`]: **redact** (the core may replace the text in place) or **detect-only**
//! (the text is a structural label such as an identifier, key, or enum value: any finding
//! rejects the request and the text is never rewritten). The mode is a property of the
//! slot class, never of the value or of anything the client says.
//!
//! Traversal order (fixed; [`ChatRequest::for_each_text`] and
//! [`ChatRequest::for_each_text_mut`] must stay identical):
//!
//! 1. `messages` in order; within a message: content (parts in order), then (#53) the
//!    message's tool-call slots in call order (`id`, `name`, argument keys and leaves in
//!    document order), or for a tool result its `tool_call_id` before its content;
//! 2. (#54) `tools` in order (name, description, schema labels and text in document order),
//!    then `tool_choice` function name;
//! 3. `stop`;
//! 4. `user`;
//! 5. `metadata` entries in input order, key then value (#55, implemented);
//! 6. (#54) `response_format.json_schema` (name, then schema labels and text in document
//!    order).
//!
//! Classes 1, 3, 4, 5 exist today. The reserved variants below are never produced until their
//! owning issue lands; their order inside the list above is already contractual.

use super::{
    ChatRequest, SerializeError, messages, metadata, response_format, stop_user, tool_calls,
    tool_defs,
};

/// Whether the core may rewrite a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotMode {
    /// The core's redaction replaces the text in place.
    Redact,
    /// Any finding rejects the request; the text is never rewritten.
    DetectOnly,
}

/// Where an inspected text lives, in deterministic traversal order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSlot {
    /// Message content text (also `role: tool` results once #53 lands).
    Message { index: usize, part: Option<usize> },
    /// `stop` sequence.
    Stop { index: usize },
    /// `user`.
    User,

    // ---- Reserved (planned, not produced yet). #53: tool history. ----
    /// Assistant `tool_calls[call].id`. Detect-only.
    ToolCallId { message: usize, call: usize },
    /// Assistant `tool_calls[call].function.name`. Detect-only.
    ToolCallName { message: usize, call: usize },
    /// Object key inside the decoded `function.arguments`. Detect-only.
    ToolCallArgumentKey {
        message: usize,
        call: usize,
        leaf: usize,
    },
    /// String leaf inside the decoded `function.arguments`. Redact.
    ToolCallArgumentText {
        message: usize,
        call: usize,
        leaf: usize,
    },
    /// Tool-result message `tool_call_id`. Detect-only.
    ToolResultId { message: usize },

    // ---- Reserved. #54: tool definitions, tool_choice, structured output. ----
    /// `tools[tool].function.name`. Detect-only.
    ToolDefName { tool: usize },
    /// `tools[tool].function.description`. Redact.
    ToolDefDescription { tool: usize },
    /// Schema label under `tools[tool]`: property key, `required` entry, enum or const
    /// string. Detect-only.
    ToolDefSchemaLabel { tool: usize, leaf: usize },
    /// Schema free text under `tools[tool]` (a keyword `description`). Redact.
    ToolDefSchemaText { tool: usize, leaf: usize },
    /// `tool_choice.function.name`. Detect-only.
    ToolChoiceName,
    /// `response_format.json_schema.name`. Detect-only.
    ResponseSchemaName,
    /// `response_format.json_schema.description`. Redact.
    ResponseSchemaDescription,
    /// Schema label under `response_format.json_schema.schema`. Detect-only.
    ResponseSchemaLabel { leaf: usize },
    /// Schema free text under `response_format.json_schema.schema`. Redact.
    ResponseSchemaText { leaf: usize },

    // ---- Metadata (#55, implemented). ----
    /// `metadata` key. Detect-only.
    MetadataKey { entry: usize },
    /// `metadata` value. Redact.
    MetadataValue { entry: usize },
}

impl TextSlot {
    /// The slot class's mode. Exhaustive on purpose: a new variant must choose.
    #[must_use]
    pub const fn mode(self) -> SlotMode {
        match self {
            Self::Message { .. }
            | Self::Stop { .. }
            | Self::User
            | Self::ToolCallArgumentText { .. }
            | Self::ToolDefDescription { .. }
            | Self::ToolDefSchemaText { .. }
            | Self::ResponseSchemaDescription
            | Self::ResponseSchemaText { .. }
            | Self::MetadataValue { .. } => SlotMode::Redact,
            Self::ToolCallId { .. }
            | Self::ToolCallName { .. }
            | Self::ToolCallArgumentKey { .. }
            | Self::ToolResultId { .. }
            | Self::ToolDefName { .. }
            | Self::ToolDefSchemaLabel { .. }
            | Self::ToolChoiceName
            | Self::ResponseSchemaName
            | Self::ResponseSchemaLabel { .. }
            | Self::MetadataKey { .. } => SlotMode::DetectOnly,
        }
    }
}

impl ChatRequest {
    /// Visit every inspected text in traversal order.
    pub fn for_each_text(&self, mut f: impl FnMut(TextSlot, &str)) {
        messages::visit(&self.messages, &mut f);
        tool_defs::visit(&self.tool_defs, &mut f);
        stop_user::visit_stop(self.stop.as_ref(), &mut f);
        if let Some(text) = &self.user {
            f(TextSlot::User, text);
        }
        metadata::visit(&self.metadata, &mut f);
        response_format::visit(self.response_format.as_ref(), &mut f);
    }

    /// Visit every inspected text mutably, in the same order as [`Self::for_each_text`].
    /// The callback must not change the slot's mode semantics: detect-only slots are
    /// handed over so the caller can scan them, and a caller must not write to them.
    pub fn for_each_text_mut(&mut self, mut f: impl FnMut(TextSlot, &mut String)) {
        messages::visit_mut(&mut self.messages, &mut f);
        tool_defs::visit_mut(&mut self.tool_defs, &mut f);
        stop_user::visit_stop_mut(self.stop.as_mut(), &mut f);
        if let Some(text) = &mut self.user {
            f(TextSlot::User, text);
        }
        metadata::visit_mut(&mut self.metadata, &mut f);
        response_format::visit_mut(self.response_format.as_mut(), &mut f);
    }

    /// Number of inspected texts, in all modes.
    #[must_use]
    pub fn text_count(&self) -> usize {
        let mut count = 0_usize;
        self.for_each_text(|_, _| count = count.saturating_add(1));
        count
    }

    /// Number of texts the core may rewrite ([`SlotMode::Redact`]).
    #[must_use]
    pub fn redactable_count(&self) -> usize {
        let mut count = 0_usize;
        self.for_each_text(|slot, _| {
            if slot.mode() == SlotMode::Redact {
                count = count.saturating_add(1);
            }
        });
        count
    }

    /// Revalidation after text mutation and before serialization (ADR 0025). Each owning
    /// module re-checks the constraints that a replacement can break (per-string and
    /// aggregate length bounds, derived-structure shape such as decoded tool arguments)
    /// and fails closed: `user` and `metadata` bounds are checked here (#55); the output
    /// bound is enforced by the serializer.
    ///
    /// # Errors
    /// [`SerializeError::Limit`] when a replacement broke a size bound,
    /// [`SerializeError::Invalid`] when it broke a structural contract.
    pub fn revalidate(&self) -> Result<(), SerializeError> {
        tool_calls::revalidate(&self.messages)?;
        tool_defs::revalidate(&self.tool_defs)?;
        stop_user::revalidate_user(self.user.as_deref())?;
        metadata::revalidate(&self.metadata)?;
        response_format::revalidate(self.response_format.as_ref())
    }
}
