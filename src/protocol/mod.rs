//! Endpoint-specific parsing and field classification (ADR 0005, ADR 0007).
//!
//! Protocol code classifies; `boundary` approves; `transport` sends. This module must not
//! create HTTP clients, import `transport`, or send requests. [`validate_with`] is the
//! only way to obtain a [`ValidatedRequest`]: one budgeted duplicate-key-rejecting parse,
//! then the endpoint matrix in [`chat`]. Everything else is rejected with a fixed safe
//! code (nothing falls through).

pub mod chat;
pub mod json;

use std::fmt;

use crate::admission::{MemoryReservation, ReceiptPermit, ReceivedRequest, RequestLimits};
use crate::telemetry::SafeCode;

/// Supported protocol contracts. Internal enum, no plugin mechanism. The first variant is
/// the Alpha 1 MVP target (see `docs/contracts/chat-completions-request.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Protocol {
    ChatCompletionsText,
}

/// Safe protocol failure. Carries no body, key, or offending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// Syntax, encoding, trailing bytes, or duplicate keys.
    Malformed,
    /// Well-formed but outside the supported subset or contract.
    Unsupported,
    /// A parse budget or a count limit was exceeded.
    LimitExceeded,
}

impl ProtocolError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::Malformed => SafeCode::MalformedInput,
            Self::Unsupported => SafeCode::UnsupportedInput,
            Self::LimitExceeded => SafeCode::LimitExceeded,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code().as_str())
    }
}

impl std::error::Error for ProtocolError {}

/// Parsed (one structure), duplicate-key and UTF-8 checked, and every field classified for
/// the route's protocol contract, held as the typed [`chat::ChatRequest`]. It keeps the
/// memory reservation alive for as long as the parsed data lives, and the original body
/// buffer is already gone. It has no path to a transport call; only `boundary` turns it
/// into a sanitized request after complete core inspection (#19).
pub struct ValidatedRequest {
    protocol: Protocol,
    request: chat::ChatRequest,
    memory: MemoryReservation,
    _receipt: ReceiptPermit,
}

impl ValidatedRequest {
    fn new(
        protocol: Protocol,
        request: chat::ChatRequest,
        memory: MemoryReservation,
        receipt: ReceiptPermit,
    ) -> Self {
        Self {
            protocol,
            request,
            memory,
            _receipt: receipt,
        }
    }

    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The typed request. Inspected text is read through
    /// [`chat::ChatRequest::for_each_text`].
    #[must_use]
    pub const fn chat(&self) -> &chat::ChatRequest {
        &self.request
    }

    /// Mutable access for text replacement only (#19): the type exposes no way to change
    /// structure.
    pub const fn chat_mut(&mut self) -> &mut chat::ChatRequest {
        &mut self.request
    }

    pub(crate) const fn memory(&self) -> &MemoryReservation {
        &self.memory
    }

    /// Hand the memory reservation to the next state; the typed request is dropped.
    pub(crate) fn into_memory(self) -> MemoryReservation {
        self.memory
    }

    #[cfg(test)]
    pub(crate) fn for_test(memory: MemoryReservation, receipt: ReceiptPermit) -> Self {
        Self::new(
            Protocol::ChatCompletionsText,
            chat::ChatRequest::for_test(),
            memory,
            receipt,
        )
    }
}

impl fmt::Debug for ValidatedRequest {
    /// Never prints request content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedRequest")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

/// Validate a received request against a protocol contract using the provisional
/// [`RequestLimits`]. The served route uses [`validate_with`] with the configured limits.
///
/// # Errors
/// See [`validate_with`].
pub fn validate(
    received: ReceivedRequest,
    protocol: Protocol,
) -> Result<ValidatedRequest, ProtocolError> {
    validate_with(received, protocol, &RequestLimits::provisional())
}

/// Validate a received request: one budgeted strict parse, then field classification.
/// The original body buffer is released as soon as the parse finishes; the memory
/// reservation moves into the result and is released when it drops (also on every error
/// path).
///
/// # Errors
/// [`ProtocolError::Malformed`] for syntax, encoding, trailing-byte, and duplicate-key
/// failures; [`ProtocolError::LimitExceeded`] for depth, node, string, and count budgets;
/// [`ProtocolError::Unsupported`] for anything outside the matrix.
pub fn validate_with(
    received: ReceivedRequest,
    protocol: Protocol,
    limits: &RequestLimits,
) -> Result<ValidatedRequest, ProtocolError> {
    let budget = json::Budget {
        max_depth: limits.max_depth,
        max_nodes: limits.max_nodes,
        max_string_bytes: usize::try_from(limits.max_string_bytes).unwrap_or(usize::MAX),
        // Decoded text never exceeds the wire bytes it came from.
        max_total_string_bytes: usize::try_from(limits.max_body_bytes).unwrap_or(usize::MAX),
    };
    let document = json::parse_budgeted(received.body(), &budget).map_err(|e| match e {
        json::ParseError::Malformed => ProtocolError::Malformed,
        json::ParseError::LimitExceeded => ProtocolError::LimitExceeded,
    })?;
    // Release the original buffer now; the reservation stays held.
    let (memory, receipt) = received.into_reservation();
    match protocol {
        Protocol::ChatCompletionsText => {
            let request = chat::classify(document, limits)?;
            Ok(ValidatedRequest::new(protocol, request, memory, receipt))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::admission::{Admission, CapacityPlan};

    fn admission() -> Admission {
        let one = NonZeroU32::new(1).expect("nonzero");
        let big = NonZeroU32::new(1024).expect("nonzero");
        Admission::new(&CapacityPlan::new(one, big, one, one, one))
    }

    fn received(admission: &Admission, body: &[u8]) -> ReceivedRequest {
        admission
            .begin_receipt(256)
            .expect("reserve")
            .complete(body.to_vec())
            .expect("fits")
    }

    #[test]
    fn duplicate_keys_are_malformed() {
        let a = admission();
        let r = received(&a, br#"{"a":1,"a":2}"#);
        assert_eq!(
            validate(r, Protocol::ChatCompletionsText).unwrap_err(),
            ProtocolError::Malformed
        );
    }

    #[test]
    fn well_formed_but_unclassified_body_is_unsupported() {
        let a = admission();
        let r = received(&a, br#"{"messages":[]}"#);
        assert_eq!(
            validate(r, Protocol::ChatCompletionsText).unwrap_err(),
            ProtocolError::Unsupported
        );
    }

    #[test]
    fn supported_body_yields_a_typed_request_and_keeps_the_reservation() {
        let a = admission();
        let r = received(
            &a,
            br#"{"model":"m","messages":[{"role":"user","content":"hello"}]}"#,
        );
        let v = validate(r, Protocol::ChatCompletionsText).expect("supported");
        assert_eq!(v.chat().messages().len(), 1);
        // The reservation (256 units of the 1024) is still held, the receipt permit too.
        assert!(a.try_reserve_memory(1024).is_err());
        assert!(a.try_receipt().is_err());
        drop(v);
        assert!(a.try_reserve_memory(1024).is_ok());
    }

    #[test]
    fn every_failure_releases_the_reservation() {
        let a = admission();
        for body in [
            &br#"{"a":1,"a":2}"#[..],
            br#"{"messages":[]}"#,
            b"[[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]]]",
        ] {
            let r = received(&a, body);
            assert!(validate(r, Protocol::ChatCompletionsText).is_err());
            assert!(a.try_reserve_memory(1024).is_ok());
            assert!(a.try_receipt().is_ok());
        }
    }
}
