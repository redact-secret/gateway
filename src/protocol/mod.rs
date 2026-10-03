//! Endpoint-specific parsing and field classification (ADR 0005, ADR 0007).
//!
//! Protocol code classifies; `boundary` approves; `transport` sends. This module must not
//! create HTTP clients, import `transport`, or send requests. [`validate_with`] is the
//! only way to obtain a [`ValidatedRequest`]: one budgeted duplicate-key-rejecting parse,
//! then the endpoint matrix in [`chat`]. Everything else is rejected with a fixed safe
//! code (nothing falls through).

pub mod chat;
pub mod json;
pub mod responses;

pub use chat::{SerializeError, SlotMode};

use std::fmt;

use crate::admission::{MemoryReservation, ReceiptPermit, ReceivedRequest, RequestLimits};
use crate::telemetry::SafeCode;

/// Supported protocol contracts. Internal enum, no plugin mechanism. The first variant is
/// the Alpha 1 MVP target (see `docs/contracts/chat-completions-request.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Protocol {
    ChatCompletionsText,
    /// Responses stateless text subset (`docs/contracts/responses-request.md`).
    /// Parsed by [`validate_with`] (#84); unrouted until #86.
    ResponsesText,
}

impl Protocol {
    /// The reviewed route id this protocol is delivered on. A sanitized request is bound
    /// to a [`crate::boundary::ProtocolRoute`] that pairs a protocol with its route, so a
    /// payload of one protocol can never be sealed for the other's route.
    #[must_use]
    pub const fn route_name(self) -> &'static str {
        match self {
            Self::ChatCompletionsText => "openai.chat_completions",
            Self::ResponsesText => "openai.responses",
        }
    }
}

/// The closed set of typed requests, one variant per reviewed endpoint (ADR 0005). Not a
/// plugin point: adding a protocol means a new variant and an exhaustive `match` here.
/// Boundary code reaches inspection, slot traversal, revalidation and bounded
/// serialization only through these methods, so both endpoints share one orchestration.
pub enum RequestBody {
    Chat(Box<chat::ChatRequest>),
    Responses(Box<responses::ResponsesRequest>),
}

impl fmt::Debug for RequestBody {
    /// Never prints request content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestBody")
            .field("protocol", &self.protocol())
            .finish_non_exhaustive()
    }
}

impl RequestBody {
    /// The protocol this body was parsed under.
    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        match self {
            Self::Chat(_) => Protocol::ChatCompletionsText,
            Self::Responses(_) => Protocol::ResponsesText,
        }
    }

    /// Validated `model` identifier (never rewritten).
    #[must_use]
    pub fn model(&self) -> &str {
        match self {
            Self::Chat(r) => r.model(),
            Self::Responses(r) => r.model(),
        }
    }

    /// The request's `stream` flag, when the protocol has one admitted.
    #[must_use]
    pub const fn stream(&self) -> Option<bool> {
        match self {
            Self::Chat(r) => r.stream(),
            Self::Responses(r) => r.stream(),
        }
    }

    /// Visit every inspected text mutably in the protocol's fixed order, with the slot
    /// class's mode. Detect-only slots are handed over to be scanned, never rewritten.
    pub fn for_each_text_mut(&mut self, mut f: impl FnMut(SlotMode, &mut String)) {
        match self {
            Self::Chat(r) => r.for_each_text_mut(|slot, text| f(slot.mode(), text)),
            Self::Responses(r) => r.for_each_text_mut(|slot, text| f(slot.mode(), text)),
        }
    }

    fn for_each_mode(&self, mut f: impl FnMut(SlotMode)) {
        match self {
            Self::Chat(r) => r.for_each_text(|slot, _| f(slot.mode())),
            Self::Responses(r) => r.for_each_text(|slot, _| f(slot.mode())),
        }
    }

    /// Number of inspected texts, in all modes.
    #[must_use]
    pub fn text_count(&self) -> usize {
        let mut count = 0_usize;
        self.for_each_mode(|_| count = count.saturating_add(1));
        count
    }

    /// Number of texts the core may rewrite ([`SlotMode::Redact`]).
    #[must_use]
    pub fn redactable_count(&self) -> usize {
        let mut count = 0_usize;
        self.for_each_mode(|mode| {
            if mode == SlotMode::Redact {
                count = count.saturating_add(1);
            }
        });
        count
    }

    /// Revalidation after text mutation and before serialization.
    ///
    /// # Errors
    /// [`SerializeError`] when a replacement broke a bound or a structural contract.
    pub fn revalidate(&self) -> Result<(), SerializeError> {
        match self {
            Self::Chat(r) => r.revalidate(),
            Self::Responses(r) => r.revalidate(),
        }
    }

    /// Serialize a fresh document from the typed request under `max_bytes`.
    ///
    /// # Errors
    /// [`SerializeError`] when the output exceeds its bound or cannot be written.
    pub fn serialize_bounded(&self, max_bytes: usize) -> Result<Vec<u8>, SerializeError> {
        match self {
            Self::Chat(r) => r.serialize_bounded(max_bytes),
            Self::Responses(r) => r.serialize_bounded(max_bytes),
        }
    }
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
/// the route's protocol contract, held as the typed [`RequestBody`]. It keeps the
/// memory reservation alive for as long as the parsed data lives, and the original body
/// buffer is already gone. It has no path to a transport call; only `boundary` turns it
/// into a sanitized request after complete core inspection (#19).
pub struct ValidatedRequest {
    request: RequestBody,
    memory: MemoryReservation,
    _receipt: ReceiptPermit,
}

impl ValidatedRequest {
    fn new(request: RequestBody, memory: MemoryReservation, receipt: ReceiptPermit) -> Self {
        Self {
            request,
            memory,
            _receipt: receipt,
        }
    }

    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.request.protocol()
    }

    /// The typed request, whichever protocol it was parsed under.
    #[must_use]
    pub const fn body(&self) -> &RequestBody {
        &self.request
    }

    /// Mutable access for text replacement only (#19): [`RequestBody`] exposes no way to
    /// change structure.
    pub const fn body_mut(&mut self) -> &mut RequestBody {
        &mut self.request
    }

    /// The Chat Completions request, or `None` for another protocol.
    #[must_use]
    pub const fn chat(&self) -> Option<&chat::ChatRequest> {
        match &self.request {
            RequestBody::Chat(r) => Some(r),
            RequestBody::Responses(_) => None,
        }
    }

    /// The Responses request, or `None` for another protocol.
    #[must_use]
    pub const fn responses(&self) -> Option<&responses::ResponsesRequest> {
        match &self.request {
            RequestBody::Responses(r) => Some(r),
            RequestBody::Chat(_) => None,
        }
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
            RequestBody::Chat(Box::new(chat::ChatRequest::for_test())),
            memory,
            receipt,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_test_responses(memory: MemoryReservation, receipt: ReceiptPermit) -> Self {
        let request = responses::ResponsesRequest::for_test("synthetic-model", None, "hello");
        Self::new(RequestBody::Responses(Box::new(request)), memory, receipt)
    }

    #[cfg(test)]
    pub(crate) fn for_test_with(
        request: RequestBody,
        memory: MemoryReservation,
        receipt: ReceiptPermit,
    ) -> Self {
        Self::new(request, memory, receipt)
    }
}

impl fmt::Debug for ValidatedRequest {
    /// Never prints request content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValidatedRequest")
            .field("protocol", &self.protocol())
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
            Ok(ValidatedRequest::new(
                RequestBody::Chat(Box::new(request)),
                memory,
                receipt,
            ))
        }
        // On every error the memory reservation and receipt drop with this frame, so a
        // refusal holds no capacity.
        Protocol::ResponsesText => {
            let request = responses::classify(document, limits)?;
            Ok(ValidatedRequest::new(
                RequestBody::Responses(Box::new(request)),
                memory,
                receipt,
            ))
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
        assert_eq!(v.chat().expect("chat").messages().len(), 1);
        // The reservation (256 units of the 1024) is still held, the receipt permit too.
        assert!(a.try_reserve_memory(1024).is_err());
        assert!(a.try_receipt().is_err());
        drop(v);
        assert!(a.try_reserve_memory(1024).is_ok());
    }

    #[test]
    fn responses_refusal_releases_the_reservation() {
        let a = admission();
        let r = received(&a, br#"{"model":"m","input":"hi"}"#);
        assert_eq!(
            validate(r, Protocol::ResponsesText).unwrap_err(),
            ProtocolError::Unsupported
        );
        assert!(a.try_reserve_memory(1024).is_ok());
        assert!(a.try_receipt().is_ok());
    }

    #[test]
    fn request_body_dispatch_covers_both_variants() {
        let mut chat = RequestBody::Chat(Box::new(chat::ChatRequest::for_test()));
        let mut resp = RequestBody::Responses(Box::new(responses::ResponsesRequest::for_test(
            "m",
            Some("i"),
            "x",
        )));
        assert_eq!((chat.text_count(), chat.redactable_count()), (0, 0));
        assert_eq!((resp.text_count(), resp.redactable_count()), (2, 2));
        let mut seen = Vec::new();
        resp.for_each_text_mut(|mode, text| {
            seen.push(mode);
            text.push('!');
        });
        assert_eq!(seen, [SlotMode::Redact, SlotMode::Redact]);
        assert_eq!(
            resp.serialize_bounded(1024).expect("fits"),
            br#"{"model":"m","instructions":"i!","input":"x!","store":false}"#
        );
        assert_eq!(resp.serialize_bounded(8), Err(SerializeError::Limit));
        assert!(chat.revalidate().is_ok() && resp.revalidate().is_ok());
        assert_eq!(chat.protocol(), Protocol::ChatCompletionsText);
        assert_eq!(resp.protocol().route_name(), "openai.responses");
        assert!(!format!("{resp:?}").contains('!'));
        chat.for_each_text_mut(|_, _| unreachable!());
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
