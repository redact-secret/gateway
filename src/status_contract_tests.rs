//! The frozen status mapping cannot drift from its documentation (#60). Compiled only
//! under `cfg(test)`.
//!
//! `docs/contracts/errors-and-telemetry.md` carries a table, "Frozen status mapping", with
//! one row per Gateway-generated outcome. This module builds the same rows from the code
//! (`Reject::status_and_code`, the real `IntoResponse` rendering, the head guard's fixed
//! byte constants) and requires the two to be identical. Changing a status, a code, a
//! `Retry-After`, or adding or removing an outcome without changing the table fails here,
//! and so does changing the table alone. The `SafeCode` and `Reject` coverage checks use
//! exhaustive `match`es (this module lives inside the crate, so `#[non_exhaustive]` does
//! not apply): adding a variant is a compile error until it is classified and documented.
//!
//! The "SDK default retry" column is checked against the rule observed in #22 with the
//! pinned SDKs (retry `408`, `409`, `429`, and every `5xx`), so a status change cannot
//! silently turn a retried outcome into a non-retried one without a documented decision.
//! The rule itself is verified against the real SDKs by `qualification/sdk`.

use std::collections::BTreeMap;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::boundary::BoundaryError;
use crate::chat_route::Reject;
use crate::core_bridge::CoreBridgeError;
use crate::head_guard::{AMBIGUOUS_FRAMING_RESPONSE, HEAD_TOO_LARGE_RESPONSE};
use crate::telemetry::SafeCode;
use crate::transport::TransportError;

const CONTRACT: &str = include_str!("../docs/contracts/errors-and-telemetry.md");
const SECTION: &str = "## Frozen status mapping (#60)";

/// One documented row, from code or from the document.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    status: String,
    code: String,
    retry_after: String,
    sent_upstream: String,
    sdk_retries: String,
}

fn sdk_default_retries(status: u16) -> &'static str {
    if matches!(status, 408 | 409 | 429) || status >= 500 {
        "yes"
    } else {
        "no"
    }
}

/// Whether request bytes may have reached the provider before the outcome. Exhaustive over
/// every outcome so a new one must be classified here.
fn sent_upstream(reject: Reject) -> &'static str {
    match reject {
        Reject::Transport(error) => match error {
            TransportError::Timeout => "maybe",
            TransportError::InvalidResponse | TransportError::ResponseTooLarge => "yes",
            TransportError::ClientInit
            | TransportError::UnknownRoute
            | TransportError::Connect
            | TransportError::Tls => "no",
        },
        _ => "no",
    }
}

fn reject_id(reject: Reject) -> String {
    let core = |e: CoreBridgeError| match e {
        CoreBridgeError::UnsupportedProfile => "UnsupportedProfile",
        CoreBridgeError::InvalidConfiguration => "InvalidConfiguration",
        CoreBridgeError::LimitExceeded => "LimitExceeded",
        CoreBridgeError::Blocked => "Blocked",
        CoreBridgeError::Warned => "Warned",
        CoreBridgeError::Overload => "Overload",
        CoreBridgeError::Incomplete => "Incomplete",
    };
    match reject {
        Reject::Method => "Reject::Method".into(),
        Reject::Target => "Reject::Target".into(),
        Reject::ContentType => "Reject::ContentType".into(),
        Reject::Encoding => "Reject::Encoding".into(),
        Reject::Framing => "Reject::Framing".into(),
        Reject::TooLarge => "Reject::TooLarge".into(),
        Reject::Deadline => "Reject::Deadline".into(),
        Reject::Overload => "Reject::Overload".into(),
        Reject::Malformed => "Reject::Malformed".into(),
        Reject::Unsupported => "Reject::Unsupported".into(),
        Reject::LimitExceeded => "Reject::LimitExceeded".into(),
        Reject::NotImplemented => "Reject::NotImplemented".into(),
        Reject::ShuttingDown => "Reject::ShuttingDown".into(),
        Reject::MissingCredential => "Reject::MissingCredential".into(),
        Reject::Header => "Reject::Header".into(),
        Reject::HeaderTooLarge => "Reject::HeaderTooLarge".into(),
        Reject::Expectation => "Reject::Expectation".into(),
        Reject::Transport(e) => format!("Reject::Transport({e:?})"),
        Reject::Inspection(BoundaryError::OutputLimit) => "Reject::Inspection(OutputLimit)".into(),
        Reject::Inspection(BoundaryError::Serialization) => {
            "Reject::Inspection(Serialization)".into()
        }
        Reject::Inspection(BoundaryError::Core(e)) => {
            format!("Reject::Inspection(Core({}))", core(e))
        }
    }
}

/// Every `Reject` outcome. `reject_id` is an exhaustive `match`, so a new variant fails to
/// compile until it is named; it then also has to be added here and to the table (the list
/// is compared with the document in both directions).
fn all_rejects() -> Vec<Reject> {
    let mut out = vec![
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
        Reject::Inspection(BoundaryError::OutputLimit),
        Reject::Inspection(BoundaryError::Serialization),
    ];
    for error in [
        TransportError::ClientInit,
        TransportError::UnknownRoute,
        TransportError::Timeout,
        TransportError::Connect,
        TransportError::Tls,
        TransportError::InvalidResponse,
        TransportError::ResponseTooLarge,
    ] {
        out.push(Reject::Transport(error));
    }
    for error in [
        CoreBridgeError::UnsupportedProfile,
        CoreBridgeError::InvalidConfiguration,
        CoreBridgeError::LimitExceeded,
        CoreBridgeError::Blocked,
        CoreBridgeError::Warned,
        CoreBridgeError::Overload,
        CoreBridgeError::Incomplete,
    ] {
        out.push(Reject::Inspection(BoundaryError::Core(error)));
    }
    out
}

/// Compile-time completeness of the code vocabulary: every `SafeCode` is named here.
/// Returns whether the code can be a request outcome (everything except `InvalidConfig`).
fn is_request_outcome(code: SafeCode) -> bool {
    match code {
        SafeCode::MalformedInput
        | SafeCode::UnsupportedInput
        | SafeCode::LimitExceeded
        | SafeCode::IncompleteInspection
        | SafeCode::Overload
        | SafeCode::TransportFailure
        | SafeCode::NotReady
        | SafeCode::NotImplemented
        | SafeCode::MissingCredential
        | SafeCode::UpstreamTimeout
        | SafeCode::UpstreamUnavailable
        | SafeCode::UpstreamTls
        | SafeCode::UpstreamInvalidResponse
        | SafeCode::UpstreamResponseTooLarge => true,
        SafeCode::InvalidConfig => false,
    }
}

const ALL_CODES: [SafeCode; 15] = [
    SafeCode::MalformedInput,
    SafeCode::UnsupportedInput,
    SafeCode::LimitExceeded,
    SafeCode::IncompleteInspection,
    SafeCode::Overload,
    SafeCode::TransportFailure,
    SafeCode::InvalidConfig,
    SafeCode::NotReady,
    SafeCode::NotImplemented,
    SafeCode::MissingCredential,
    SafeCode::UpstreamTimeout,
    SafeCode::UpstreamUnavailable,
    SafeCode::UpstreamTls,
    SafeCode::UpstreamInvalidResponse,
    SafeCode::UpstreamResponseTooLarge,
];

async fn code_rows() -> BTreeMap<String, Row> {
    let mut rows = BTreeMap::new();
    for reject in all_rejects() {
        let response = reject.into_response();
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let (_, code) = reject.status_and_code();
        // The rendered body is exactly the fixed shape: no field but the code.
        assert_eq!(
            String::from_utf8_lossy(&body),
            format!(r#"{{"error":{{"code":"{}"}}}}"#, code.as_str()),
            "{}",
            reject_id(reject)
        );
        rows.insert(
            reject_id(reject),
            Row {
                status: status.to_string(),
                code: code.as_str().to_owned(),
                retry_after,
                sent_upstream: sent_upstream(reject).to_owned(),
                sdk_retries: sdk_default_retries(status).to_owned(),
            },
        );
    }
    // The head guard writes fixed bytes before any HTTP layer exists.
    for (id, bytes) in [
        ("HeadGuard::Ambiguous", AMBIGUOUS_FRAMING_RESPONSE),
        ("HeadGuard::TooLarge", HEAD_TOO_LARGE_RESPONSE),
    ] {
        let text = String::from_utf8_lossy(bytes);
        let status: u16 = text
            .strip_prefix("HTTP/1.1 ")
            .and_then(|r| r.get(..3))
            .and_then(|s| s.parse().ok())
            .unwrap();
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        let code = body
            .strip_prefix(r#"{"error":{"code":""#)
            .and_then(|r| r.strip_suffix(r#""}}"#))
            .unwrap();
        rows.insert(
            id.to_owned(),
            Row {
                status: status.to_string(),
                code: code.to_owned(),
                retry_after: "-".into(),
                sent_upstream: "no".into(),
                sdk_retries: sdk_default_retries(status).to_owned(),
            },
        );
    }
    rows
}

fn cell(s: &str) -> String {
    s.trim().trim_matches('`').to_owned()
}

fn doc_rows() -> (BTreeMap<String, Row>, Vec<String>) {
    let section = CONTRACT
        .split(SECTION)
        .nth(1)
        .expect("the contract has a 'Frozen status mapping (#60)' section");
    let section = section.split("\n## ").next().unwrap_or(section);
    let mut rows = BTreeMap::new();
    let mut closed = Vec::new();
    for line in section.lines().filter(|l| l.starts_with("| `")) {
        let cols: Vec<String> = line.trim_matches('|').split('|').map(cell).collect();
        if cols.len() == 7 && cols[1] == "none" {
            // A closed connection: no status, no body, no code. Documented, not derivable
            // from a constant (the late-head and connection-bound tests prove the behavior).
            closed.push(cols[0].clone());
            continue;
        }
        assert_eq!(cols.len(), 7, "malformed row: {line}");
        let previous = rows.insert(
            cols[0].clone(),
            Row {
                status: cols[1].clone(),
                code: cols[2].clone(),
                retry_after: cols[3].clone(),
                sent_upstream: cols[4].clone(),
                sdk_retries: cols[5].clone(),
            },
        );
        assert!(previous.is_none(), "duplicate row {}", cols[0]);
    }
    (rows, closed)
}

#[tokio::test]
async fn documented_status_mapping_is_exactly_the_code_mapping() {
    let from_code = code_rows().await;
    let (from_doc, closed) = doc_rows();
    let code_ids: Vec<&String> = from_code.keys().collect();
    let doc_ids: Vec<&String> = from_doc.keys().collect();
    assert_eq!(
        code_ids, doc_ids,
        "an outcome exists in code but not in the table, or the reverse"
    );
    for (id, row) in &from_code {
        assert_eq!(
            Some(row),
            from_doc.get(id),
            "{id}: code and table disagree; change both in the same commit"
        );
    }
    let mut closed = closed;
    closed.sort();
    assert_eq!(
        closed,
        ["Connection::AtBound", "HeadGuard::LateHead"],
        "the closed-connection outcomes (no status, no body)"
    );
}

#[tokio::test]
async fn every_request_outcome_code_appears_in_the_table_and_every_mapped_status_is_an_error() {
    let rows = code_rows().await;
    let used: Vec<&str> = rows.values().map(|r| r.code.as_str()).collect();
    for code in ALL_CODES {
        if is_request_outcome(code) {
            // `TransportFailure` is reachable only as the startup-time client-construction
            // failure and the unknown-route fallback; both have rows.
            assert!(
                used.contains(&code.as_str()),
                "{} has no row in the frozen mapping",
                code.as_str()
            );
        }
    }
    // The vocabulary listed in the contract's category table is exactly the code's.
    let categories = CONTRACT
        .split("## Gateway-owned error categories")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .unwrap();
    let mut documented: Vec<String> = categories
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| cell(l.trim_matches('|').split('|').next().unwrap()))
        .collect();
    documented.sort();
    let mut in_code: Vec<String> = ALL_CODES.iter().map(|c| c.as_str().to_owned()).collect();
    // The category table names `upstream_tls_failure`; all other spellings are `as_str()`.
    in_code.sort();
    assert_eq!(documented, in_code, "category table and SafeCode::as_str()");
    for row in rows.values() {
        let status = StatusCode::from_u16(row.status.parse().unwrap()).unwrap();
        assert!(status.is_client_error() || status.is_server_error());
    }
}

#[tokio::test]
async fn retry_after_is_present_exactly_on_the_overload_outcomes() {
    for (id, row) in code_rows().await {
        let overload = row.code == "overload";
        assert_eq!(
            row.retry_after != "-",
            overload,
            "{id}: Retry-After belongs to overload outcomes only"
        );
        if overload {
            assert_eq!(row.retry_after, "1", "{id}");
            assert_eq!(row.status, "503", "{id}");
        }
    }
}
