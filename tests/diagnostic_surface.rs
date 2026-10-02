//! Diagnostic and error object paths (#25): every error type's `Debug` and `Display`
//! output, and every fixed response body, is a short fixed vocabulary that cannot carry
//! request content, credentials, hosts, or addresses. All data is synthetic.
//!
//! Two layers:
//!
//! 1. Structural: every error type is `Copy`. A `Copy` type cannot own a `String`, `Vec`,
//!    or `Box`, so none can retain a body, header, or credential; this fails to compile if
//!    a variant ever gains an owning field.
//! 2. Rendering: every variant renders (`Debug`, and `Display` where implemented) using
//!    only a small safe alphabet, so even a `&'static str` field could not smuggle a URL,
//!    path, header, or JSON fragment through diagnostics.
//!
//! No error type implements `serde::Serialize`; that is pinned by the compile-fail case
//! `tests/ui/fail_serialize_error_types.rs`. Runtime leakage across the served stack is in
//! `src/transport/tests/attack_tests.rs` and `tests/attack_surface.rs`.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::fmt::{Debug, Display};

use redact_secret_gateway::admission::AdmissionError;
use redact_secret_gateway::boundary::BoundaryError;
use redact_secret_gateway::chat_route::Reject;
use redact_secret_gateway::config::ConfigErrorKind;
use redact_secret_gateway::core_bridge::CoreBridgeError;
use redact_secret_gateway::protocol::ProtocolError;
use redact_secret_gateway::protocol::json::ParseError;
use redact_secret_gateway::server::StartupError;
use redact_secret_gateway::telemetry::SafeCode;
use redact_secret_gateway::transport::TransportError;
use redact_secret_gateway::transport::destination::OriginError;
use redact_secret_gateway::transport::headers::{HeaderReject, ResponseHeaderError};
use redact_secret_gateway::transport::stream::StreamError;
use support::leak::Markers;

/// Fails to compile for a type that is not `Copy`.
const fn assert_copy<T: Copy>() {}

const _: () = {
    assert_copy::<SafeCode>();
    assert_copy::<TransportError>();
    assert_copy::<CoreBridgeError>();
    assert_copy::<BoundaryError>();
    assert_copy::<ProtocolError>();
    assert_copy::<ParseError>();
    assert_copy::<AdmissionError>();
    assert_copy::<OriginError>();
    assert_copy::<HeaderReject>();
    assert_copy::<ResponseHeaderError>();
    assert_copy::<StreamError>();
    assert_copy::<StartupError>();
    assert_copy::<Reject>();
    assert_copy::<ConfigErrorKind>();
};

fn safe(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 96
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | ':' | '(' | ')'))
}

fn check_debug<T: Debug>(label: &str, value: &T) {
    let text = format!("{value:?}");
    assert!(safe(&text), "{label}: Debug is outside the safe alphabet");
}

fn check_both<T: Debug + Display>(label: &str, value: &T) {
    check_debug(label, value);
    let text = value.to_string();
    assert!(safe(&text), "{label}: Display is outside the safe alphabet");
}

#[test]
fn every_error_variant_renders_only_fixed_safe_text() {
    use TransportError as T;
    for v in [
        T::ClientInit,
        T::UnknownRoute,
        T::Timeout,
        T::Connect,
        T::Tls,
        T::InvalidResponse,
        T::ResponseTooLarge,
    ] {
        check_both("TransportError", &v);
        check_debug("Reject::Transport", &Reject::Transport(v));
        assert!(safe(v.code().as_str()));
    }
    use CoreBridgeError as C;
    for v in [
        C::UnsupportedProfile,
        C::InvalidConfiguration,
        C::LimitExceeded,
        C::Blocked,
        C::Warned,
        C::Overload,
        C::Incomplete,
    ] {
        check_both("CoreBridgeError", &v);
        check_both("BoundaryError::Core", &BoundaryError::Core(v));
        check_debug(
            "Reject::Inspection",
            &Reject::Inspection(BoundaryError::Core(v)),
        );
    }
    for v in [BoundaryError::OutputLimit, BoundaryError::Serialization] {
        check_both("BoundaryError", &v);
    }
    for v in [
        ProtocolError::Malformed,
        ProtocolError::Unsupported,
        ProtocolError::LimitExceeded,
    ] {
        check_both("ProtocolError", &v);
    }
    for v in [ParseError::Malformed, ParseError::LimitExceeded] {
        check_both("ParseError", &v);
    }
    for v in [AdmissionError::Overload, AdmissionError::InvalidReservation] {
        check_both("AdmissionError", &v);
    }
    for v in [
        OriginError::Scheme,
        OriginError::Userinfo,
        OriginError::NotOriginForm,
        OriginError::Host,
        OriginError::Port,
        OriginError::NotReviewed,
        OriginError::ParserDifferential,
    ] {
        check_both("OriginError", &v);
    }
    for v in [
        HeaderReject::TooLarge,
        HeaderReject::MissingCredential,
        HeaderReject::Credential,
        HeaderReject::Metadata,
        HeaderReject::Connection,
        HeaderReject::Expectation,
    ] {
        check_both("HeaderReject", &v);
        check_debug("Reject::from", &Reject::from(v));
    }
    check_both("ResponseHeaderError", &ResponseHeaderError::ContentEncoding);
    for v in [
        StreamError::IdleTimeout,
        StreamError::LifetimeExceeded,
        StreamError::Upstream,
        StreamError::BufferExceeded,
        StreamError::Shutdown,
    ] {
        check_both("StreamError", &v);
    }
    for v in [
        StartupError::Init,
        StartupError::Signals,
        StartupError::Bind,
        StartupError::Serve,
    ] {
        check_both("StartupError", &v);
        assert!(safe(v.code().as_str()));
    }
    for v in [
        ConfigErrorKind::Unreadable,
        ConfigErrorKind::TooLarge,
        ConfigErrorKind::Malformed,
        ConfigErrorKind::UnsupportedSchemaVersion,
        ConfigErrorKind::MissingField,
        ConfigErrorKind::UnknownField,
        ConfigErrorKind::InvalidType,
        ConfigErrorKind::InvalidValue,
        ConfigErrorKind::InvalidCombination,
    ] {
        check_debug("ConfigErrorKind", &v);
        assert!(safe(v.as_str()));
    }
}

#[test]
fn every_gateway_response_body_is_the_fixed_error_envelope() {
    let rejects = [
        Reject::Method,
        Reject::Target,
        Reject::ContentType,
        Reject::Encoding,
        Reject::Framing,
        Reject::TooLarge,
        Reject::Deadline,
        Reject::Overload,
        Reject::Malformed,
        Reject::Unsupported,
        Reject::LimitExceeded,
        Reject::NotImplemented,
        Reject::ShuttingDown,
        Reject::MissingCredential,
        Reject::Header,
        Reject::HeaderTooLarge,
        Reject::Expectation,
        Reject::Transport(TransportError::Timeout),
        Reject::Transport(TransportError::Tls),
        Reject::Transport(TransportError::Connect),
        Reject::Inspection(BoundaryError::Serialization),
        Reject::Inspection(BoundaryError::Core(CoreBridgeError::Blocked)),
    ];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    for reject in rejects {
        use axum::response::IntoResponse;
        let response = reject.into_response();
        let body = runtime
            .block_on(axum::body::to_bytes(response.into_body(), 4096))
            .expect("body");
        let text = String::from_utf8(body.to_vec()).expect("utf8");
        let (_, code) = reject.status_and_code();
        assert_eq!(
            text,
            format!(r#"{{"error":{{"code":"{}"}}}}"#, code.as_str())
        );
    }
}

#[test]
fn error_text_never_contains_the_standard_markers() {
    let markers = Markers::standard();
    // The vocabulary above has no room for marker-shaped text; scan the full rendering of a
    // representative error from each layer anyway, so a future free-text field is caught.
    for text in [
        format!("{:?}", Reject::Transport(TransportError::Tls)),
        TransportError::Connect.to_string(),
        BoundaryError::Core(CoreBridgeError::Blocked).to_string(),
        HeaderReject::Credential.to_string(),
        StartupError::Bind.to_string(),
    ] {
        markers.assert_clean("error rendering", text.as_bytes());
    }
}
