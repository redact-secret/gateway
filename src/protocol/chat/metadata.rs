//! `metadata`: provider-compatible string map. Owner: #55. Contract:
//! `docs/contracts/chat-completions-request.md`, ADR 0025.
//!
//! Today `metadata` is rejected. `classify` already routes the key to [`parse`], so #55
//! edits only this file.

use std::io;

use super::serialize::Bounded;
use super::{Checked, RequestLimits, SerializeError, TextSlot, unsupported};
use crate::protocol::json::Json;

/// Parsed metadata entries. Fields are added by #55.
#[derive(Debug, Default)]
pub struct Metadata {}

/// The `metadata` value. Rejected until #55.
pub(super) fn parse(_value: Json, _limits: &RequestLimits) -> Checked<Metadata> {
    Err(unsupported())
}

/// Visit metadata texts (traversal position 5 in `slots.rs`).
pub(super) fn visit(_metadata: &Metadata, _f: &mut impl FnMut(TextSlot, &str)) {}

/// Mutable twin of [`visit`].
pub(super) fn visit_mut(_metadata: &mut Metadata, _f: &mut impl FnMut(TextSlot, &mut String)) {}

/// Write `,"metadata":{...}` when present.
pub(super) const fn write(_metadata: &Metadata, _w: &mut Bounded) -> io::Result<()> {
    Ok(())
}

/// Revalidation after mutation (value length bounds after replacement).
pub(super) const fn revalidate(_metadata: &Metadata) -> Result<(), SerializeError> {
    Ok(())
}
