//! Complete core inspection and validated request transformation (issue #19; ADR 0004,
//! 0007, 0015; core-completeness and field-classification contracts).
//!
//! Boundary-level tests drive the real `Inspection` service (the real bounded worker pool,
//! one pinned-core registry per worker) from a `ValidatedRequest` to a `SanitizedRequest`
//! and look at the sealed body. Nothing here constructs a transport or contacts an upstream.
//! Every secret is a synthetic revoked-style token and every PII value is invented.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod support;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redact_secret::Profile;
use redact_secret_gateway::admission::{Admission, CapacityPlan, RequestLimits};
use redact_secret_gateway::boundary::ProtocolRoute;
use redact_secret_gateway::boundary::{BoundaryError, Inspection, SanitizedRequest};
use redact_secret_gateway::config::{ContentPolicy, OnWarn, RouteId};
use redact_secret_gateway::core_bridge::CoreBridgeError;
use redact_secret_gateway::protocol::{self, Protocol, ProtocolError};
use redact_secret_gateway::telemetry::SafeCode;
use serde_json::{Value, json};
use support::leak::Markers;

/// Synthetic revoked-style token (the core's own documentation fixture shape).
fn token(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

const PEM: &str = "-----BEGIN PRIVATE KEY-----\\nU1lOVEhFVElDUkVWT0tFRFNZTlRIRVRJQ0tFWQ==\\n-----END PRIVATE KEY-----";

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

struct Lab {
    admission: Arc<Admission>,
    inspection: Inspection,
    limits: RequestLimits,
    memory_units: u32,
    inspection_permits: u32,
}

impl Lab {
    fn new(content: &ContentPolicy) -> Self {
        Self::with(content, RequestLimits::provisional(), 2)
    }

    fn with(content: &ContentPolicy, inspect_limits: RequestLimits, permits: u32) -> Self {
        let memory_units = 8192;
        let plan = CapacityPlan::new(nz(4), nz(memory_units), nz(permits), nz(1), nz(1));
        let admission = Arc::new(Admission::new(&plan));
        let inspection =
            Inspection::start(Arc::clone(&admission), content, &inspect_limits, &plan).unwrap();
        Self {
            admission,
            inspection,
            limits: RequestLimits::provisional(),
            memory_units,
            inspection_permits: permits,
        }
    }

    async fn validate(&self, body: &str) -> protocol::ValidatedRequest {
        let received = self
            .admission
            .begin_body_receipt(body.len(), &self.limits)
            .await
            .unwrap()
            .complete(body.as_bytes().to_vec())
            .unwrap();
        protocol::validate_with(received, Protocol::ChatCompletionsText, &self.limits).unwrap()
    }

    async fn run(&self, body: &str) -> Result<SanitizedRequest, BoundaryError> {
        self.inspection
            .inspect_and_approve(
                self.validate(body).await,
                ProtocolRoute::new(
                    Protocol::ChatCompletionsText,
                    RouteId::new("synthetic-route"),
                ),
            )
            .await
    }

    /// Wait until every memory unit and inspection permit is free again.
    async fn assert_all_free(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let memory = self.admission.try_reserve_memory(self.memory_units);
            let permits: Vec<_> = (0..self.inspection_permits)
                .map(|_| self.admission.try_inspection())
                .collect();
            let receipt = self.admission.try_receipt();
            if memory.is_ok() && permits.iter().all(Result::is_ok) && receipt.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "capacity was not returned");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn full() -> ContentPolicy {
    ContentPolicy::new(Profile::Full)
}

fn body_json(s: &SanitizedRequest) -> Value {
    serde_json::from_slice(s.body()).expect("sanitized body is valid JSON")
}

fn body_text(s: &SanitizedRequest) -> &str {
    std::str::from_utf8(s.body()).expect("sanitized body is UTF-8")
}

// ------------------------------------------------------------------------- field coverage

#[tokio::test]
async fn synthetic_secrets_in_every_supported_text_field_are_redacted_in_order() {
    let lab = Lab::new(&full());
    let body = json!({
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": format!("sys {}", token(1))},
            {"role": "user", "content": [
                {"type": "text", "text": format!("part-a {}", token(2))},
                {"type": "text", "text": "clean part"},
                {"type": "text", "text": format!("part-c {}", token(3))}
            ]},
            {"role": "assistant", "content": "clean reply"},
            {"role": "developer", "content": format!("dev {}", token(4))}
        ],
        "stop": [format!("stop-a {}", token(5)), "clean stop", format!("stop-c {}", token(6))],
        "user": format!("user {}", token(7))
    })
    .to_string();
    let sanitized = lab.run(&body).await.unwrap();
    let text = body_text(&sanitized);
    assert!(!text.contains("SYNTHETICREVOKED"), "a secret survived");
    let v = body_json(&sanitized);
    // Numbering is request-wide and follows the deterministic traversal order: messages in
    // order (parts in order), then stop, then user.
    assert_eq!(v["messages"][0]["content"], "sys <SECRET_1>");
    assert_eq!(v["messages"][1]["content"][0]["text"], "part-a <SECRET_2>");
    assert_eq!(v["messages"][1]["content"][1]["text"], "clean part");
    assert_eq!(v["messages"][1]["content"][2]["text"], "part-c <SECRET_3>");
    assert_eq!(v["messages"][2]["content"], "clean reply");
    assert_eq!(v["messages"][3]["content"], "dev <SECRET_4>");
    assert_eq!(v["stop"][0], "stop-a <SECRET_5>");
    assert_eq!(v["stop"][1], "clean stop");
    assert_eq!(v["stop"][2], "stop-c <SECRET_6>");
    assert_eq!(v["user"], "user <SECRET_7>");
    assert_eq!(sanitized.route().as_str(), "synthetic-route");
    drop(sanitized);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn single_string_stop_is_inspected() {
    let lab = Lab::new(&full());
    let body = json!({
        "model": "m", "messages": [{"role": "user", "content": "hi"}],
        "stop": format!("end {}", token(8))
    })
    .to_string();
    let v = body_json(&lab.run(&body).await.unwrap());
    assert_eq!(v["stop"], "end <SECRET_1>");
}

#[tokio::test]
async fn model_is_never_rewritten_and_any_finding_in_it_rejects() {
    let lab = Lab::new(&full());
    // A model-shaped identifier that the core recognizes as a token (the charset admits it).
    let body = json!({
        "model": token(9),
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string();
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::Blocked));
    let ok = lab
        .run(r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}"#)
        .await
        .unwrap();
    assert_eq!(body_json(&ok)["model"], "gpt-4o-mini");
}

#[tokio::test]
async fn optional_pii_selection_redacts_invented_pii() {
    let content =
        ContentPolicy::new(Profile::Full).with_pii(vec!["pii:family:global:email".into()]);
    let lab = Lab::new(&content);
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "email: jordan.public@mailhost.net"}]
    })
    .to_string();
    let v = body_json(&lab.run(&body).await.unwrap());
    assert_eq!(v["messages"][0]["content"], "email: <SECRET_1>");
}

// ------------------------------------------------------- escapes, Unicode, structure

#[tokio::test]
async fn escaped_input_is_decoded_before_inspection_and_stays_valid_json() {
    let lab = Lab::new(&full());
    // Every character of the token's prefix is a JSON \u escape in the raw body, so a scan
    // over the raw bytes would miss it; the decoded string is what the core sees.
    let escaped_prefix = "\\u0067\\u0068\\u0070_SYNTHETICREVOKED00000000000000000010";
    let raw = format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"k={escaped_prefix} \"q\" \\ \n \u2028 \ud83d\ude00 \u00e9"}}]}}"#
    );
    assert!(
        !raw.contains("ghp_"),
        "the raw body must not contain the token"
    );
    let sanitized = lab.run(&raw).await.unwrap();
    assert!(!body_text(&sanitized).contains("SYNTHETICREVOKED"));
    let v = body_json(&sanitized);
    assert_eq!(
        v["messages"][0]["content"],
        "k=<SECRET_1> \"q\" \\ \n \u{2028} \u{1F600} \u{e9}"
    );
}

#[tokio::test]
async fn english_and_korean_text_survive_byte_for_byte_semantically() {
    let lab = Lab::new(&full());
    let korean = "안녕하세요, 김민수입니다. 이 문서는 비밀이 아닙니다 \u{1F600} café";
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": korean},
            {"role": "user", "content": [{"type": "text", "text": format!("{korean} {}", token(11))}]}
        ],
        "user": "사용자-001"
    })
    .to_string();
    let sanitized = lab.run(&body).await.unwrap();
    let v = body_json(&sanitized);
    assert_eq!(v["messages"][0]["content"], korean);
    assert_eq!(
        v["messages"][1]["content"][0]["text"],
        format!("{korean} <SECRET_1>")
    );
    assert_eq!(v["user"], "사용자-001");
    // Literal Unicode is emitted as UTF-8, not mangled.
    assert!(body_text(&sanitized).contains("안녕하세요"));
}

#[tokio::test]
async fn keys_types_and_controls_are_preserved() {
    let lab = Lab::new(&full());
    let input = json!({
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
            {"role": "assistant", "content": ""}
        ],
        "stream": true,
        "stream_options": {"include_usage": false},
        "temperature": 0.7,
        "top_p": 1,
        "presence_penalty": -1.5,
        "frequency_penalty": 0,
        "max_tokens": 128,
        "max_completion_tokens": 256,
        "n": 1,
        "seed": -42,
        "stop": ["x", "y"],
        "user": "u",
        "response_format": {"type": "json_object"}
    });
    let sanitized = lab.run(&input.to_string()).await.unwrap();
    // Nothing is redacted here, so the fresh document must equal the input as JSON.
    assert_eq!(body_json(&sanitized), input);
    // The array form stays an array, the string form stays a string.
    let v = body_json(&sanitized);
    assert!(v["messages"][1]["content"].is_array());
    assert!(v["messages"][0]["content"].is_string());
}

#[tokio::test]
async fn serialization_is_a_fresh_document_not_the_original_bytes() {
    let lab = Lab::new(&full());
    // Whitespace, key order, and escapes in the input are not carried over.
    let raw = " {\n \"messages\" : [ {\"content\":\"\\u0068i\",\"role\":\"user\"} ],\n \"model\":\"m\" } ";
    let sanitized = lab.run(raw).await.unwrap();
    assert_eq!(
        body_text(&sanitized),
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#
    );
}

// ---------------------------------------------------------------- policy decisions

#[tokio::test]
async fn block_findings_reject_the_request() {
    let lab = Lab::new(&full());
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": format!("here: {PEM}")}]
    })
    .to_string()
    .replace("\\\\n", "\\n");
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::Blocked));
    assert_eq!(err.code(), SafeCode::UnsupportedInput);
    let markers = Markers::standard();
    markers.assert_clean("display", format!("{err} {err:?}").as_bytes());
    assert!(!format!("{err:?}").contains("PRIVATE"));
    lab.assert_all_free().await;
}

const WARN_BODY: &str =
    r#"{"model":"m","messages":[{"role":"user","content":"password=hunter2xyz"}]}"#;

#[tokio::test]
async fn warn_findings_reject_by_default() {
    assert_eq!(ContentPolicy::new(Profile::Full).on_warn(), OnWarn::Reject);
    let lab = Lab::new(&full());
    let err = lab.run(WARN_BODY).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::Warned));
    assert_eq!(err.code(), SafeCode::UnsupportedInput);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn warn_findings_forward_unchanged_only_when_the_operator_chose_forward() {
    let lab = Lab::new(&full().with_on_warn(OnWarn::Forward));
    let sanitized = lab.run(WARN_BODY).await.unwrap();
    // The default core policy leaves a `Warn` finding in the text; this is the documented,
    // deliberate operator choice, not a redaction.
    assert_eq!(
        body_json(&sanitized)["messages"][0]["content"],
        "password=hunter2xyz"
    );
}

#[tokio::test]
async fn pre_redacted_input_is_ordinary_text_and_numbering_is_request_wide() {
    let lab = Lab::new(&full());
    // A client-supplied placeholder-shaped literal is not a finding and is kept as is; a real
    // token next to it is still redacted. The two strings can be equal, so numbering is
    // never a unique mapping back to values (documented; nothing trusts it).
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": format!("<SECRET_1> then {}", token(12))}]
    })
    .to_string();
    let v = body_json(&lab.run(&body).await.unwrap());
    assert_eq!(v["messages"][0]["content"], "<SECRET_1> then <SECRET_1>");
    // Already-sanitized content is stable under a second pass.
    let again = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "<SECRET_1> then <SECRET_2>"}]
    })
    .to_string();
    let v = body_json(&lab.run(&again).await.unwrap());
    assert_eq!(v["messages"][0]["content"], "<SECRET_1> then <SECRET_2>");
}

#[tokio::test]
async fn claims_that_input_was_already_scanned_have_no_effect() {
    // The matrix rejects unknown fields, so such a claim cannot even ride in the body, and no
    // code reads one from headers. Same text, same result, claim or not.
    let lab = Lab::new(&full());
    let with_claim = json!({
        "model": "m", "already_scanned": true,
        "messages": [{"role": "user", "content": token(13)}]
    })
    .to_string();
    let received = lab
        .admission
        .begin_body_receipt(with_claim.len(), &lab.limits)
        .await
        .unwrap()
        .complete(with_claim.into_bytes())
        .unwrap();
    let err =
        protocol::validate_with(received, Protocol::ChatCompletionsText, &lab.limits).unwrap_err();
    assert_eq!(err, ProtocolError::Unsupported);
}

// ------------------------------------------------------------ failure and limits

#[tokio::test]
async fn finding_limit_rejects_with_no_partial_result() {
    let lab = Lab::new(&full().with_max_findings(1));
    let body = json!({
        "model": "m",
        "messages": [
            {"role": "user", "content": token(14)},
            {"role": "user", "content": token(15)}
        ]
    })
    .to_string();
    // Each leaf has one finding (within the bound); the request has two (over it).
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::LimitExceeded));
    assert_eq!(err.code(), SafeCode::LimitExceeded);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn request_wide_input_limit_rejects_even_when_each_text_fits() {
    let mut tight = RequestLimits::provisional();
    tight.max_body_bytes = 100;
    let lab = Lab::with(&full(), tight, 2);
    let half = "a".repeat(60);
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": half}, {"role": "user", "content": half}]
    })
    .to_string();
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::LimitExceeded));
    lab.assert_all_free().await;
}

#[tokio::test]
async fn transformed_output_over_its_bound_is_rejected_not_truncated() {
    // Input text fits the inspection bound, but the fresh document (keys, quotes, role) does not.
    let mut tight = RequestLimits::provisional();
    tight.max_body_bytes = 100;
    let lab = Lab::with(&full(), tight, 2);
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "b".repeat(80)}]
    })
    .to_string();
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::OutputLimit);
    assert_eq!(err.code(), SafeCode::LimitExceeded);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn inspection_overload_is_a_safe_rejection_that_keeps_the_request_unprocessed() {
    let lab = Lab::with(&full(), RequestLimits::provisional(), 1);
    let hold = lab.admission.try_inspection().unwrap();
    let err = lab.run(WARN_BODY).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::Overload));
    assert_eq!(err.code(), SafeCode::Overload);
    drop(hold);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn request_state_does_not_leak_across_requests() {
    let lab = Lab::new(&full());
    let one = json!({"model":"m","messages":[{"role":"user","content":format!("{} {}", token(21), token(22))}]})
        .to_string();
    let two = json!({"model":"m","messages":[{"role":"user","content":token(23)}]}).to_string();
    let a = lab.run(&one).await.unwrap();
    let b = lab.run(&two).await.unwrap();
    assert_eq!(
        body_json(&a)["messages"][0]["content"],
        "<SECRET_1> <SECRET_2>"
    );
    // The second request restarts at 1 even though the first used 1 and 2.
    assert_eq!(body_json(&b)["messages"][0]["content"], "<SECRET_1>");
    // Concurrent requests are independent as well.
    let lab = Arc::new(Lab::with(&full(), RequestLimits::provisional(), 4));
    let mut tasks = Vec::new();
    for i in 0..4_u32 {
        let lab = Arc::clone(&lab);
        tasks.push(tokio::spawn(async move {
            let body = json!({"model":"m","messages":[{"role":"user","content":token(30 + i)}]})
                .to_string();
            let s = lab.run(&body).await.unwrap();
            body_json(&s)["messages"][0]["content"].clone()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), "<SECRET_1>");
    }
}

// ------------------------------------------- cancellation keeps capacity (ADR 0004)

#[tokio::test]
async fn dropping_the_awaiting_future_does_not_release_capacity_early() {
    use redact_secret_gateway::core_bridge::pool::JobHandle;
    // More permits than workers (the pool never has more than 16), so the request can take
    // a permit while a gated job keeps every worker busy. The real request then queues
    // behind the gates, and dropping its future cancels the wait only.
    let lab = Lab::with(&full(), RequestLimits::provisional(), 17);
    let workers = lab.inspection.workers();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let mut gates: Vec<JobHandle<()>> = Vec::new();
    for _ in 0..workers {
        let permit = lab.admission.try_inspection().unwrap();
        let memory = lab.admission.try_reserve_memory(1).unwrap();
        let release_rx = Arc::clone(&release_rx);
        let started_tx = started_tx.clone();
        gates.push(
            lab.inspection
                .pool()
                .submit_with(permit, memory, move |_| {
                    let _ = started_tx.send(());
                    let _ = release_rx.lock().unwrap().recv();
                })
                .unwrap(),
        );
    }
    for _ in 0..workers {
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    // Every worker is busy. Poll the request once so its job is submitted, then drop it.
    let validated = lab.validate(WARN_BODY).await;
    let mut fut = Box::pin(lab.inspection.inspect_and_approve(
        validated,
        ProtocolRoute::new(Protocol::ChatCompletionsText, RouteId::new("r")),
    ));
    let polled = tokio::time::timeout(Duration::from_millis(100), &mut fut).await;
    assert!(polled.is_err(), "the job is queued behind the gates");
    drop(fut);
    // The cancelled job still owns its permit and its memory reservation (the request's
    // reservation is still held, so the full budget cannot be taken).
    assert!(lab.admission.try_reserve_memory(lab.memory_units).is_err());
    let free: Vec<_> = (0..17)
        .filter_map(|_| lab.admission.try_inspection().ok())
        .collect();
    assert_eq!(
        free.len(),
        17 - workers - 1,
        "gates and the cancelled job hold permits"
    );
    drop(free);
    // Release the gates: a worker dequeues the cancelled job, skips it (it never runs), and
    // only then does capacity return. Nothing is ever delivered for it.
    for _ in 0..workers {
        release_tx.send(()).unwrap();
    }
    for gate in gates {
        gate.await.unwrap();
    }
    lab.assert_all_free().await;
}

#[tokio::test]
async fn repeated_cancelled_requests_stay_within_capacity_and_all_capacity_returns() {
    let lab = Arc::new(Lab::with(&full(), RequestLimits::provisional(), 2));
    let big = "x ".repeat(20_000);
    let body = json!({"model":"m","messages":[{"role":"user","content": big}]}).to_string();
    let mut accepted = 0_usize;
    let mut refused = 0_usize;
    for _ in 0..200 {
        let ticket = lab
            .admission
            .begin_body_receipt(body.len(), &lab.limits)
            .await;
        let Ok(ticket) = ticket else {
            refused += 1;
            continue;
        };
        let received = ticket.complete(body.as_bytes().to_vec()).unwrap();
        let validated =
            protocol::validate_with(received, Protocol::ChatCompletionsText, &lab.limits).unwrap();
        let lab2 = Arc::clone(&lab);
        let task = tokio::spawn(async move {
            lab2.inspection
                .inspect_and_approve(
                    validated,
                    ProtocolRoute::new(Protocol::ChatCompletionsText, RouteId::new("r")),
                )
                .await
        });
        tokio::task::yield_now().await;
        task.abort();
        accepted += 1;
    }
    assert!(accepted > 0);
    assert_eq!(accepted + refused, 200);
    // However many were cancelled mid-flight, every permit and reservation comes back, and
    // only because the jobs really finished.
    lab.assert_all_free().await;
}
