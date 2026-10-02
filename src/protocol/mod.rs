//! Endpoint-specific parsing and field classification (ADR 0005, ADR 0007).
//!
//! Protocol code classifies; `boundary` approves; `transport` sends. This module must not
//! create HTTP clients, import `transport`, or send requests. Scaffold: no protocol is
//! implemented, so every request is rejected as unsupported (nothing falls through).

pub mod json;

use std::fmt;

use crate::admission::{MemoryReservation, ReceiptPermit, ReceivedRequest};
use crate::telemetry::SafeCode;

/// Supported protocol contracts. Internal enum, no plugin mechanism. The first variant is
/// the Alpha 1 MVP target; its classification rules arrive in #18/#19.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Protocol {
    ChatCompletionsText,
}

/// Safe protocol failure. Carries no body, key, or offending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    Malformed,
    Unsupported,
}

impl ProtocolError {
    #[must_use]
    pub const fn code(self) -> SafeCode {
        match self {
            Self::Malformed => SafeCode::MalformedInput,
            Self::Unsupported => SafeCode::UnsupportedInput,
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
/// the route's protocol contract. It keeps the memory reservation alive for as long as the
/// parsed structure lives. It has no path to a transport call.
pub struct ValidatedRequest {
    protocol: Protocol,
    #[expect(dead_code, reason = "read by #18/#19 field classification and output")]
    document: json::Json,
    memory: MemoryReservation,
    _receipt: ReceiptPermit,
}

impl ValidatedRequest {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "constructed by #18/#19 once field classification exists"
        )
    )]
    fn new(
        protocol: Protocol,
        document: json::Json,
        memory: MemoryReservation,
        receipt: ReceiptPermit,
    ) -> Self {
        Self {
            protocol,
            document,
            memory,
            _receipt: receipt,
        }
    }

    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub(crate) const fn memory(&self) -> &MemoryReservation {
        &self.memory
    }

    /// Hand the memory reservation to the next state; the parsed document is dropped.
    pub(crate) fn into_memory(self) -> MemoryReservation {
        self.memory
    }

    #[cfg(test)]
    pub(crate) fn for_test(memory: MemoryReservation, receipt: ReceiptPermit) -> Self {
        Self::new(
            Protocol::ChatCompletionsText,
            json::Json::Null,
            memory,
            receipt,
        )
    }
}

impl fmt::Debug for ValidatedRequest {
    /// Never prints document content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedRequest")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

/// Validate a received request against a protocol contract.
///
/// Scaffold behavior: the body is parsed strictly (duplicate keys, malformed JSON,
/// invalid UTF-8 are rejected), then the request is rejected as unsupported because no
/// field classification rules exist yet.
///
/// # Errors
/// [`ProtocolError::Malformed`] for parse failures; [`ProtocolError::Unsupported`] always
/// otherwise until #18/#19.
pub fn validate(
    received: ReceivedRequest,
    protocol: Protocol,
) -> Result<ValidatedRequest, ProtocolError> {
    let _document = json::parse_strict(received.body()).map_err(|_| ProtocolError::Malformed)?;
    match protocol {
        Protocol::ChatCompletionsText => Err(ProtocolError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::admission::{Admission, CapacityPlan};

    fn received(body: &[u8]) -> ReceivedRequest {
        let one = NonZeroU32::new(1).expect("nonzero");
        let big = NonZeroU32::new(1024).expect("nonzero");
        let admission = Admission::new(&CapacityPlan::new(one, big, one, one, one));
        admission
            .begin_receipt(256)
            .expect("reserve")
            .complete(body.to_vec())
            .expect("fits")
    }

    #[test]
    fn duplicate_keys_are_malformed() {
        let r = received(br#"{"a":1,"a":2}"#);
        assert_eq!(
            validate(r, Protocol::ChatCompletionsText).unwrap_err(),
            ProtocolError::Malformed
        );
    }

    #[test]
    fn well_formed_body_is_unsupported_until_classification_exists() {
        let r = received(br#"{"messages":[]}"#);
        assert_eq!(
            validate(r, Protocol::ChatCompletionsText).unwrap_err(),
            ProtocolError::Unsupported
        );
    }
}
