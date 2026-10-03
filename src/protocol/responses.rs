//! Responses typed request skeleton (#83; contract
//! `docs/contracts/responses-request.md`, ADR 0031).
//!
//! This module exists so the closed [`super::RequestBody`] dispatch has a real second
//! variant that goes through the same inspection, revalidation and bounded serialization
//! as Chat. It is deliberately minimal: the only fields it can hold are `model`,
//! `instructions` and a plain-string `input`. There is **no classifier**: the route is
//! unrouted and [`super::validate_with`] answers `unsupported_input` for
//! [`super::Protocol::ResponsesText`] until #84 adds the parser and #85 the remaining
//! slots. A value of this type cannot be built from client bytes in this change.
//!
//! Slot order is fixed and identical for reading and mutation: `instructions`, then
//! `input`. `model` is scanned detect-only by the boundary before the slots.

use std::fmt;
use std::io::{self, Write};

use super::SlotMode;
use super::chat::SerializeError;
use super::chat::serialize::{Bounded, json_str};

/// Longest `model` identifier, as in Chat.
const MAX_MODEL_BYTES: usize = 256;

/// The typed boundary representation of a supported Responses request.
pub struct ResponsesRequest {
    model: String,
    instructions: Option<String>,
    input: String,
}

/// Where an inspected Responses text lives, in traversal order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsesSlot {
    /// `instructions`. Redact.
    Instructions,
    /// String `input`. Redact.
    Input,
}

impl ResponsesSlot {
    /// The slot class's mode. Exhaustive on purpose: a new variant must choose.
    #[must_use]
    pub const fn mode(self) -> SlotMode {
        match self {
            Self::Instructions | Self::Input => SlotMode::Redact,
        }
    }
}

impl ResponsesRequest {
    /// Validated structural identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Visit every inspected text in traversal order.
    pub fn for_each_text(&self, mut f: impl FnMut(ResponsesSlot, &str)) {
        if let Some(text) = &self.instructions {
            f(ResponsesSlot::Instructions, text);
        }
        f(ResponsesSlot::Input, &self.input);
    }

    /// Visit every inspected text mutably, in the same order as [`Self::for_each_text`].
    pub fn for_each_text_mut(&mut self, mut f: impl FnMut(ResponsesSlot, &mut String)) {
        if let Some(text) = &mut self.instructions {
            f(ResponsesSlot::Instructions, text);
        }
        f(ResponsesSlot::Input, &mut self.input);
    }

    /// Re-check what a replacement can break before serialization.
    ///
    /// # Errors
    /// [`SerializeError::Invalid`] when the model identifier is no longer valid.
    pub const fn revalidate(&self) -> Result<(), SerializeError> {
        if self.model.is_empty() || self.model.len() > MAX_MODEL_BYTES {
            return Err(SerializeError::Invalid);
        }
        Ok(())
    }

    /// Serialize a fresh JSON document (canonical order `model`, `instructions`, `input`),
    /// bounded while it is produced.
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
        w.write_all(b",\"input\":")?;
        json_str(w, &self.input)?;
        w.write_all(b"}")
    }

    #[cfg(test)]
    pub(crate) fn for_test(model: &str, instructions: Option<&str>, input: &str) -> Self {
        Self {
            model: model.to_owned(),
            instructions: instructions.map(str::to_owned),
            input: input.to_owned(),
        }
    }
}

impl fmt::Debug for ResponsesRequest {
    /// Never prints request content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponsesRequest").finish_non_exhaustive()
    }
}
