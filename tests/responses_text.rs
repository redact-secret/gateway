//! Responses stateless text request: `instructions`, string and text-message `input`,
//! `store:false` and the structural controls (#84; contract
//! `docs/contracts/responses-request.md`, ADR 0031).
//!
//! The route is unrouted until #86, so these tests drive the protocol and boundary layers
//! directly: `protocol::validate_with` (one strict budgeted parse plus classification) and the
//! real `Inspection` service to a `SanitizedRequest`. A rejection at either layer means no
//! sanitized body exists, so nothing can reach a transport (zero forwarded bytes). Every
//! secret is a synthetic revoked-style token; all other data is invented.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redact_secret::Profile;
use redact_secret_gateway::admission::{Admission, CapacityPlan, RequestLimits};
use redact_secret_gateway::boundary::{BoundaryError, Inspection, ProtocolRoute, SanitizedRequest};
use redact_secret_gateway::config::{ContentPolicy, RouteId};
use redact_secret_gateway::core_bridge::CoreBridgeError;
use redact_secret_gateway::protocol::{self, Protocol, ProtocolError};
use serde_json::{Value, json};

fn token(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

struct Lab {
    admission: Arc<Admission>,
    inspection: Inspection,
    limits: RequestLimits,
    memory_units: u32,
}

impl Lab {
    fn new() -> Self {
        Self::with(RequestLimits::provisional(), 4)
    }

    fn with(limits: RequestLimits, permits: u32) -> Self {
        Self::with_policy(&ContentPolicy::new(Profile::Full), limits, permits)
    }

    fn with_policy(content: &ContentPolicy, limits: RequestLimits, permits: u32) -> Self {
        let memory_units = 8192;
        let plan = CapacityPlan::new(nz(8), nz(memory_units), nz(permits), nz(1), nz(1));
        let admission = Arc::new(Admission::new(&plan));
        let inspection =
            Inspection::start(Arc::clone(&admission), content, &limits, &plan).unwrap();
        Self {
            admission,
            inspection,
            limits,
            memory_units,
        }
    }

    async fn validate(
        &self,
        protocol: Protocol,
        body: &str,
    ) -> Result<protocol::ValidatedRequest, ProtocolError> {
        let received = self
            .admission
            .begin_body_receipt(body.len(), &self.limits)
            .await
            .unwrap()
            .complete(body.as_bytes().to_vec())
            .unwrap();
        protocol::validate_with(received, protocol, &self.limits)
    }

    async fn run_as(
        &self,
        protocol: Protocol,
        route: Protocol,
        body: &str,
    ) -> Result<SanitizedRequest, BoundaryError> {
        let validated = self.validate(protocol, body).await.expect("classified");
        self.inspection
            .inspect_and_approve(validated, ProtocolRoute::new(route, RouteId::new("r")))
            .await
    }

    async fn run(&self, body: &str) -> Result<SanitizedRequest, BoundaryError> {
        self.run_as(Protocol::ResponsesText, Protocol::ResponsesText, body)
            .await
    }

    async fn reject(&self, body: &str) -> ProtocolError {
        match self.validate(Protocol::ResponsesText, body).await {
            Ok(_) => panic!("accepted: {body}"),
            Err(e) => e,
        }
    }

    async fn assert_all_free(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let memory = self.admission.try_reserve_memory(self.memory_units);
            let receipt = self.admission.try_receipt();
            if memory.is_ok() && receipt.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "capacity was not returned");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn text(s: &SanitizedRequest) -> &str {
    std::str::from_utf8(s.body()).unwrap()
}

fn json_of(s: &SanitizedRequest) -> Value {
    serde_json::from_slice(s.body()).unwrap()
}

// ----------------------------------------------------------------------- positive forms

#[tokio::test]
async fn string_input_with_instructions_yields_the_expected_upstream_body() {
    let lab = Lab::new();
    let body = r#" { "input" : "서울의 날씨는?", "store":false, "instructions":"Be brief.", "model":"synthetic-model" } "#;
    let s = lab.run(body).await.unwrap();
    // A fresh document in canonical order, not the original bytes; `store:false` written.
    assert_eq!(
        text(&s),
        r#"{"model":"synthetic-model","instructions":"Be brief.","input":"서울의 날씨는?","store":false}"#
    );
    assert_eq!(s.protocol(), Protocol::ResponsesText);
    drop(s);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn message_items_keep_their_form_roles_and_phase() {
    let lab = Lab::new();
    let input = json!({
        "model": "m", "store": false,
        "input": [
            {"role": "system", "content": "s"},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "d"}]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "a"}, {"type": "input_text", "text": ""}]},
            {"role": "assistant", "content": "r", "phase": "final_answer"},
            {"type": "message", "role": "assistant", "content": "", "phase": "commentary"}
        ]
    });
    let s = lab.run(&input.to_string()).await.unwrap();
    // Nothing is redacted, so the fresh document equals the input as JSON (type present
    // stays present, absent stays absent, string stays string, array stays array).
    assert_eq!(json_of(&s), input);
}

#[tokio::test]
async fn controls_are_preserved_and_store_is_always_false() {
    let lab = Lab::new();
    let input = json!({
        "model": "m", "store": false, "input": "hi", "stream": true,
        "stream_options": {"include_obfuscation": false},
        "temperature": 0.7, "top_p": 1, "max_output_tokens": 2_147_483_647
    });
    let s = lab.run(&input.to_string()).await.unwrap();
    assert_eq!(json_of(&s), input);
    assert!(text(&s).contains(r#""store":false"#));
}

#[tokio::test]
async fn secrets_in_every_text_slot_are_redacted_in_slot_order() {
    let lab = Lab::new();
    let body = json!({
        "model": "m", "store": false,
        "input": [
            {"role": "user", "content": format!("a {}", token(1))},
            {"role": "user", "content": [
                {"type": "input_text", "text": format!("b {}", token(2))},
                {"type": "input_text", "text": "clean"},
                {"type": "input_text", "text": format!("c {}", token(3))}]},
            {"role": "assistant", "content": format!("d {}", token(4))},
            {"role": "developer", "content": format!("e {}", token(5))}
        ],
        // Key order in the request does not change slot order: instructions first.
        "instructions": format!("sys {}", token(6))
    })
    .to_string();
    let s = lab.run(&body).await.unwrap();
    assert!(!text(&s).contains("SYNTHETICREVOKED"));
    let v = json_of(&s);
    assert_eq!(v["instructions"], "sys <SECRET_1>");
    assert_eq!(v["input"][0]["content"], "a <SECRET_2>");
    assert_eq!(v["input"][1]["content"][0]["text"], "b <SECRET_3>");
    assert_eq!(v["input"][1]["content"][1]["text"], "clean");
    assert_eq!(v["input"][1]["content"][2]["text"], "c <SECRET_4>");
    assert_eq!(v["input"][2]["content"], "d <SECRET_5>");
    assert_eq!(v["input"][3]["content"], "e <SECRET_6>");
}

#[tokio::test]
async fn secret_in_string_input_is_redacted() {
    let lab = Lab::new();
    let body = json!({"model":"m","store":false,"input":format!("k {}", token(7))}).to_string();
    let v = json_of(&lab.run(&body).await.unwrap());
    assert_eq!(v["input"], "k <SECRET_1>");
}

#[tokio::test]
async fn model_secret_rejects_and_structural_fields_cannot_carry_text() {
    let lab = Lab::new();
    let body = json!({"model": token(8), "store": false, "input": "hi"}).to_string();
    assert_eq!(
        lab.run(&body).await.unwrap_err(),
        BoundaryError::Core(CoreBridgeError::Blocked)
    );
    // A secret in `type`, `role`, `phase`, or a rejected key is refused by the matrix.
    for item in [
        json!({"type": token(9), "role": "user", "content": "x"}),
        json!({"role": token(9), "content": "x"}),
        json!({"role": "assistant", "content": "x", "phase": token(9)}),
        json!({"role": "user", "content": "x", token(9): 1}),
        json!({"role": "user", "content": [{"type": token(9), "text": "x"}]}),
    ] {
        let body = json!({"model": "m", "store": false, "input": [item]}).to_string();
        assert_eq!(lab.reject(&body).await, ProtocolError::Unsupported);
    }
    let body = json!({"model": "m", "store": false, "input": "x", token(9): 1}).to_string();
    assert_eq!(lab.reject(&body).await, ProtocolError::Unsupported);
    lab.assert_all_free().await;
}

// ------------------------------------------------------ escapes, Unicode, Korean, English

#[tokio::test]
async fn escaped_secret_is_decoded_before_inspection() {
    let lab = Lab::new();
    let escaped_prefix = "\\u0067\\u0068\\u0070_SYNTHETICREVOKED00000000000000000010";
    let raw = format!(
        r#"{{"model":"m","store":false,"instructions":"k={escaped_prefix} \"q\" \\ \n   😀 é","input":[{{"role":"user","content":"한국 {escaped_prefix}"}}]}}"#
    );
    assert!(!raw.contains("ghp_"));
    let s = lab.run(&raw).await.unwrap();
    assert!(!text(&s).contains("SYNTHETICREVOKED"));
    let v = json_of(&s);
    assert_eq!(
        v["instructions"],
        "k=<SECRET_1> \"q\" \\ \n \u{2028} \u{1F600} \u{e9}"
    );
    assert_eq!(v["input"][0]["content"], "한국 <SECRET_2>");
}

#[tokio::test]
async fn korean_and_english_text_survive_as_utf8() {
    let lab = Lab::new();
    let korean = "안녕하세요, 김민수입니다. café \u{1F600}";
    let body = json!({
        "model": "m", "store": false, "instructions": korean,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": format!("{korean} {}", token(11))}]}]
    })
    .to_string();
    let s = lab.run(&body).await.unwrap();
    let v = json_of(&s);
    assert_eq!(v["instructions"], korean);
    assert_eq!(
        v["input"][0]["content"][0]["text"],
        format!("{korean} <SECRET_1>")
    );
    assert!(text(&s).contains("안녕하세요"));
}

#[tokio::test]
async fn empty_strings_are_valid_text() {
    let lab = Lab::new();
    let body = r#"{"model":"m","store":false,"instructions":"","input":""}"#;
    assert_eq!(
        text(&lab.run(body).await.unwrap()),
        r#"{"model":"m","instructions":"","input":"","store":false}"#
    );
}

// ----------------------------------------------------------------- zero-forward negatives

#[tokio::test]
async fn unsupported_forms_are_rejected_before_inspection() {
    let lab = Lab::new();
    let part =
        |p: Value| json!({"model":"m","store":false,"input":[{"role":"user","content":[p]}]});
    let item = |i: Value| json!({"model":"m","store":false,"input":[i]});
    let cases: Vec<Value> = vec![
        // store
        json!({"model":"m","input":"hi"}),
        json!({"model":"m","store":true,"input":"hi"}),
        json!({"model":"m","store":null,"input":"hi"}),
        json!({"model":"m","store":"false","input":"hi"}),
        json!({"model":"m","store":0,"input":"hi"}),
        // missing / null / wrong-typed top level
        json!({"store":false,"input":"hi"}),
        json!({"model":"m","store":false}),
        json!({"model":"m","store":false,"input":null}),
        json!({"model":"m","store":false,"input":5}),
        json!({"model":"m","store":false,"input":{}}),
        json!({"model":"m","store":false,"input":"hi","instructions":null}),
        json!({"model":"m","store":false,"input":"hi","instructions":["x"]}),
        json!({"model":null,"store":false,"input":"hi"}),
        json!({"model":"","store":false,"input":"hi"}),
        json!({"model":"bad model","store":false,"input":"hi"}),
        // state and reference paths, reasoning, unreviewed fields (any value)
        json!({"model":"m","store":false,"input":"hi","previous_response_id":"resp_synthetic"}),
        json!({"model":"m","store":false,"input":"hi","previous_response_id":null}),
        json!({"model":"m","store":false,"input":"hi","conversation":null}),
        json!({"model":"m","store":false,"input":"hi","conversation":"conv_synthetic"}),
        json!({"model":"m","store":false,"input":"hi","prompt":{"id":"p"}}),
        json!({"model":"m","store":false,"input":"hi","background":false}),
        json!({"model":"m","store":false,"input":"hi","include":[]}),
        json!({"model":"m","store":false,"input":"hi","reasoning":{}}),
        json!({"model":"m","store":false,"input":"hi","truncation":"auto"}),
        json!({"model":"m","store":false,"input":"hi","user":"u"}),
        json!({"model":"m","store":false,"input":"hi","service_tier":"auto"}),
        json!({"model":"m","store":false,"input":"hi","future_field":1}),
        // owned by #85: rejected until its parser exists
        json!({"model":"m","store":false,"input":"hi","tools":[]}),
        json!({"model":"m","store":false,"input":"hi","tool_choice":"auto"}),
        json!({"model":"m","store":false,"input":"hi","parallel_tool_calls":true}),
        json!({"model":"m","store":false,"input":"hi","text":{}}),
        json!({"model":"m","store":false,"input":"hi","metadata":{}}),
        // controls
        json!({"model":"m","store":false,"input":"hi","stream":null}),
        json!({"model":"m","store":false,"input":"hi","stream":"true"}),
        json!({"model":"m","store":false,"input":"hi","stream_options":{"include_obfuscation":true}}),
        json!({"model":"m","store":false,"input":"hi","stream":false,"stream_options":{}}),
        json!({"model":"m","store":false,"input":"hi","stream":true,"stream_options":{"include_usage":true}}),
        json!({"model":"m","store":false,"input":"hi","stream":true,"stream_options":null}),
        json!({"model":"m","store":false,"input":"hi","temperature":2.5}),
        json!({"model":"m","store":false,"input":"hi","temperature":null}),
        json!({"model":"m","store":false,"input":"hi","top_p":-0.1}),
        json!({"model":"m","store":false,"input":"hi","max_output_tokens":0}),
        json!({"model":"m","store":false,"input":"hi","max_output_tokens":1.0}),
        json!({"model":"m","store":false,"input":"hi","max_output_tokens":2_147_483_648_i64}),
        // items
        json!({"model":"m","store":false,"input":[]}),
        json!({"model":"m","store":false,"input":["hi"]}),
        item(json!({"type":"item_reference","id":"msg_synthetic"})),
        item(json!({"type":"reasoning","id":"rs_synthetic","summary":[]})),
        item(json!({"type":"function_call","call_id":"c","name":"n","arguments":"{}"})),
        item(json!({"type":"function_call_output","call_id":"c","output":"x"})),
        item(json!({"type":"unknown_synthetic","role":"user","content":"x"})),
        item(json!({"type":null,"role":"user","content":"x"})),
        item(json!({"type":"message","id":"msg_synthetic","role":"user","content":"x"})),
        item(json!({"role":"user","content":"x","status":"completed"})),
        item(json!({"role":"user"})),
        item(json!({"content":"x"})),
        item(json!({"role":"tool","content":"x"})),
        item(json!({"role":"USER","content":"x"})),
        item(json!({"role":"user","content":null})),
        item(json!({"role":"user","content":5})),
        item(json!({"role":"user","content":{"type":"input_text","text":"x"}})),
        item(json!({"role":"user","content":[]})),
        item(json!({"role":"user","content":"x","phase":"final_answer"})),
        item(json!({"role":"assistant","content":"x","phase":null})),
        item(json!({"role":"assistant","content":"x","phase":"other"})),
        item(json!({"role":"assistant","content":[{"type":"input_text","text":"x"}]})),
        item(
            json!({"type":"message","id":"msg_synthetic","status":"completed","role":"assistant",
            "content":[{"type":"output_text","text":"hi","annotations":[]}]}),
        ),
        // parts
        part(json!({"type":"input_image","image_url":"https://example.invalid/a.png"})),
        part(json!({"type":"input_file","file_id":"file_synthetic"})),
        part(json!({"type":"input_audio","input_audio":{}})),
        part(json!({"type":"output_text","text":"x"})),
        part(json!({"type":"text","text":"x"})),
        part(json!({"text":"x"})),
        part(json!({"type":"input_text"})),
        part(json!({"type":"input_text","text":null})),
        part(json!({"type":"input_text","text":"x","annotations":[]})),
        part(json!({"type":"input_text","text":"x","prompt_cache_breakpoint":{}})),
        part(json!("x")),
    ];
    for case in &cases {
        let body = case.to_string();
        let err = lab.reject(&body).await;
        assert_eq!(err, ProtocolError::Unsupported, "{body}");
    }
    // The document root must be an object.
    for body in ["[]", "\"hi\"", "5", "null"] {
        assert_eq!(lab.reject(body).await, ProtocolError::Unsupported);
    }
    lab.assert_all_free().await;
}

#[tokio::test]
async fn malformed_bodies_and_duplicate_keys_are_malformed() {
    let lab = Lab::new();
    for body in [
        r#"{"model":"m","model":"n","store":false,"input":"hi"}"#,
        r#"{"model":"m","store":false,"store":false,"input":"hi"}"#,
        r#"{"model":"m","store":false,"input":"hi","input":"x"}"#,
        r#"{"model":"m","store":false,"input":[{"role":"user","role":"user","content":"x"}]}"#,
        r#"{"model":"m","store":false,"input":[{"role":"user","content":[{"type":"input_text","text":"x","text":"y"}]}]}"#,
        // Duplicate after decoding: an escaped spelling of the same key.
        r#"{"model":"m","store":false,"input":"hi","input":"x"}"#,
        r#"{"model":"m","store":false,"input":"hi""#,
        r#"{"model":"m","store":false,"input":"\ud800"}"#,
        r#"{"model":"m","store":false,"input":"hi"} x"#,
    ] {
        assert_eq!(lab.reject(body).await, ProtocolError::Malformed, "{body}");
    }
    lab.assert_all_free().await;
}

// --------------------------------------------------------------------- exact limit bounds

fn items(n: usize) -> String {
    let one = r#"{"role":"user","content":"x"}"#;
    format!(
        r#"{{"model":"m","store":false,"input":[{}]}}"#,
        vec![one; n].join(",")
    )
}

fn parts(n: usize) -> String {
    let one = r#"{"type":"input_text","text":"x"}"#;
    format!(
        r#"{{"model":"m","store":false,"input":[{{"role":"user","content":[{}]}}]}}"#,
        vec![one; n].join(",")
    )
}

#[tokio::test]
async fn item_and_part_counts_are_exact_at_the_boundary() {
    let mut limits = RequestLimits::provisional();
    limits.max_messages = 5;
    let lab = Lab::with(limits, 2);
    assert!(
        lab.validate(Protocol::ResponsesText, &items(5))
            .await
            .is_ok()
    );
    assert_eq!(lab.reject(&items(6)).await, ProtocolError::LimitExceeded);
    assert!(
        lab.validate(Protocol::ResponsesText, &parts(64))
            .await
            .is_ok()
    );
    assert_eq!(lab.reject(&parts(65)).await, ProtocolError::LimitExceeded);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn string_node_and_depth_budgets_are_exact_at_the_boundary() {
    let base = r#"{"model":"m","store":false,"input":"#;
    // String bytes: exactly the limit passes, one more is rejected.
    let mut limits = RequestLimits::provisional();
    limits.max_string_bytes = 16;
    let lab = Lab::with(limits, 2);
    let at = format!("{base}\"{}\"}}", "a".repeat(16));
    let over = format!("{base}\"{}\"}}", "a".repeat(17));
    assert!(lab.validate(Protocol::ResponsesText, &at).await.is_ok());
    assert_eq!(lab.reject(&over).await, ProtocolError::LimitExceeded);
    // Decoded, not raw, length: 16 escaped characters count as 16 bytes.
    let escaped = format!("{base}\"{}\"}}", "\\u0061".repeat(16));
    assert!(
        lab.validate(Protocol::ResponsesText, &escaped)
            .await
            .is_ok()
    );

    // Depth of the deepest accepted form (object, array, object, array, object) is 5.
    let deepest = parts(1);
    let mut limits = RequestLimits::provisional();
    limits.max_depth = 5;
    let lab = Lab::with(limits, 2);
    assert!(
        lab.validate(Protocol::ResponsesText, &deepest)
            .await
            .is_ok()
    );
    let mut limits = RequestLimits::provisional();
    limits.max_depth = 4;
    let lab = Lab::with(limits, 2);
    assert_eq!(lab.reject(&deepest).await, ProtocolError::LimitExceeded);

    // Nodes: values plus keys. Count once, then pin the exact boundary.
    let body = items(3);
    let mut found = None;
    for n in 1..200_u32 {
        let mut limits = RequestLimits::provisional();
        limits.max_nodes = n;
        let lab = Lab::with(limits, 2);
        if lab.validate(Protocol::ResponsesText, &body).await.is_ok() {
            found = Some(n);
            break;
        }
    }
    let n = found.expect("a node limit admits the body");
    let mut limits = RequestLimits::provisional();
    limits.max_nodes = n - 1;
    let lab = Lab::with(limits, 2);
    assert_eq!(
        lab.reject(&body).await,
        ProtocolError::LimitExceeded,
        "one node fewer must reject"
    );
}

#[tokio::test]
async fn many_small_slots_hit_the_request_wide_findings_limit() {
    // Each slot has one finding (within a per-slot view); the request has three (over the
    // request-wide bound). No partial result, nothing sealed.
    let lab = Lab::with_policy(
        &ContentPolicy::new(Profile::Full).with_max_findings(2),
        RequestLimits::provisional(),
        2,
    );
    let body = json!({
        "model": "m", "store": false,
        "instructions": token(41),
        "input": [
            {"role": "user", "content": token(42)},
            {"role": "user", "content": [{"type": "input_text", "text": token(43)}]}
        ]
    })
    .to_string();
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::LimitExceeded));
    lab.assert_all_free().await;
}

#[tokio::test]
async fn serialized_output_is_bounded_exactly_and_never_truncated() {
    let lab = Lab::new();
    let validated = lab
        .validate(
            Protocol::ResponsesText,
            r#"{"model":"m","store":false,"input":"hello"}"#,
        )
        .await
        .unwrap();
    let full = validated.body().serialize_bounded(1024).unwrap();
    assert_eq!(full, br#"{"model":"m","input":"hello","store":false}"#);
    // Exactly the output length fits; one byte less is refused outright (no partial body).
    assert_eq!(
        validated.body().serialize_bounded(full.len()).unwrap(),
        full
    );
    assert_eq!(
        validated.body().serialize_bounded(full.len() - 1),
        Err(protocol::SerializeError::Limit)
    );
    assert_eq!(
        validated.body().serialize_bounded(0),
        Err(protocol::SerializeError::Limit)
    );
    drop(validated);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn output_larger_than_the_reservation_is_refused_at_the_boundary() {
    // The reservation is sized from the wire body; the fresh document adds `"store":false`
    // when the caller wrote it compactly enough that nothing else shrinks. A body that fills
    // the whole body bound exactly leaves no room for any growth.
    let mut limits = RequestLimits::provisional();
    limits.max_body_bytes = 64;
    let lab = Lab::with(limits, 2);
    let prefix = r#"{"model":"m","store":false,"input":""#;
    let fill = 64 - prefix.len() - 2;
    let body = format!("{prefix}{}\"}}", "b".repeat(fill));
    assert_eq!(body.len(), 64);
    // Same length in and out: fits exactly or is refused; never truncated.
    match lab.run(&body).await {
        Ok(s) => assert!(s.body().len() <= 64),
        Err(e) => assert_eq!(e, BoundaryError::OutputLimit),
    }
    lab.assert_all_free().await;
}

// ------------------------------------------------------------------ isolation and routing

#[tokio::test]
async fn concurrent_chat_and_responses_requests_do_not_share_state() {
    let lab = Arc::new(Lab::with(RequestLimits::provisional(), 8));
    let mut tasks = Vec::new();
    for i in 0..8_u32 {
        let lab = Arc::clone(&lab);
        tasks.push(tokio::spawn(async move {
            if i % 2 == 0 {
                let body = json!({"model":"m","store":false,
                    "instructions": format!("a {}", token(100 + i)),
                    "input": format!("b {}", token(200 + i))})
                .to_string();
                let s = lab.run(&body).await.unwrap();
                assert_eq!(s.protocol(), Protocol::ResponsesText);
                let v = json_of(&s);
                assert_eq!(v["instructions"], "a <SECRET_1>");
                assert_eq!(v["input"], "b <SECRET_2>");
                assert!(v.get("messages").is_none());
            } else {
                let body = json!({"model":"m","messages":[{"role":"user",
                    "content": format!("c {}", token(300 + i))}]})
                .to_string();
                let s = lab
                    .run_as(
                        Protocol::ChatCompletionsText,
                        Protocol::ChatCompletionsText,
                        &body,
                    )
                    .await
                    .unwrap();
                assert_eq!(s.protocol(), Protocol::ChatCompletionsText);
                let v = json_of(&s);
                assert_eq!(v["messages"][0]["content"], "c <SECRET_1>");
                assert!(v.get("input").is_none() && v.get("store").is_none());
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    lab.assert_all_free().await;
}

#[tokio::test]
async fn a_responses_payload_cannot_be_sealed_for_the_chat_route() {
    let lab = Lab::new();
    let body = r#"{"model":"m","store":false,"input":"hi"}"#;
    let err = lab
        .run_as(Protocol::ResponsesText, Protocol::ChatCompletionsText, body)
        .await
        .unwrap_err();
    assert_eq!(err, BoundaryError::RouteMismatch);
    // And a Chat-shaped body is not a Responses request.
    assert_eq!(
        lab.reject(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
            .await,
        ProtocolError::Unsupported
    );
    lab.assert_all_free().await;
}

#[tokio::test]
async fn request_state_does_not_leak_across_requests() {
    let lab = Lab::new();
    let one = json!({"model":"m","store":false,"input":format!("{} {}", token(21), token(22))})
        .to_string();
    let two = json!({"model":"m","store":false,"input":token(23)}).to_string();
    let a = lab.run(&one).await.unwrap();
    let b = lab.run(&two).await.unwrap();
    assert_eq!(json_of(&a)["input"], "<SECRET_1> <SECRET_2>");
    assert_eq!(json_of(&b)["input"], "<SECRET_1>");
}
