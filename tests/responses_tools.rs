//! Responses function-call history, outputs, function tools, `tool_choice`, `text.format`
//! structured output and `metadata` (#85; contract `docs/contracts/responses-request.md`,
//! ADR 0031).
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

fn req(extra: Value) -> String {
    let mut base = json!({"model": "m", "store": false, "input": "hi"});
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base.to_string()
}

fn with_input(items: Value) -> Value {
    json!({"model": "m", "store": false, "input": items})
}

fn call(id: &str, args: &str) -> Value {
    json!({"type": "function_call", "call_id": id, "name": "get_weather", "arguments": args})
}

fn output(id: &str, out: Value) -> Value {
    json!({"type": "function_call_output", "call_id": id, "output": out})
}

fn weather_tool() -> Value {
    json!({"type": "function", "name": "get_weather", "description": "City weather.",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
            "required": ["city"], "additionalProperties": false},
        "strict": true})
}

fn blocked() -> BoundaryError {
    BoundaryError::Core(CoreBridgeError::Blocked)
}

// ------------------------------------------------------------------------ accepted forms

#[tokio::test]
async fn contract_example_round_trips_with_structure_retained() {
    let lab = Lab::new();
    let input = json!({
        "model": "synthetic-model", "store": false,
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "Weather in Seoul?"}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                "arguments": "{\"city\":\"서울\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "sunny"}
        ],
        "tools": [weather_tool()],
        "tool_choice": {"type": "function", "name": "get_weather"},
        "text": {"format": {"type": "json_schema", "name": "answer",
            "schema": {"type": "object", "properties": {"summary": {"type": "string"}},
                "required": ["summary"], "additionalProperties": false},
            "strict": true}},
        "metadata": {"trace_id": "abc"}
    });
    let s = lab.run(&input.to_string()).await.unwrap();
    assert_eq!(json_of(&s), input);
    drop(s);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn outbound_key_order_is_canonical_and_arguments_are_compact() {
    let lab = Lab::new();
    // Caller key order and whitespace differ; the document is fresh, in contract order.
    let body = r#"{"metadata":{"k":"v"},"text":{"verbosity":"low","format":{"type":"text"}},
        "parallel_tool_calls":false,"tool_choice":"auto",
        "tools":[{"strict":null,"parameters":null,"name":"f","type":"function"}],
        "input":[{"arguments":"{ \"a\" : [ 1 , 2.50 , null , true ] }","name":"f","call_id":"c1","type":"function_call"},
                 {"output":"x","call_id":"c1","type":"function_call_output"}],
        "store":false,"model":"m","stream":false}"#;
    let s = lab.run(body).await.unwrap();
    assert_eq!(
        text(&s),
        concat!(
            r#"{"model":"m","input":[{"type":"function_call","call_id":"c1","name":"f","#,
            r#""arguments":"{\"a\":[1,2.5,null,true]}"},"#,
            r#"{"type":"function_call_output","call_id":"c1","output":"x"}],"store":false,"#,
            r#""tools":[{"type":"function","name":"f","parameters":null,"strict":null}],"#,
            r#""tool_choice":"auto","parallel_tool_calls":false,"#,
            r#""text":{"format":{"type":"text"},"verbosity":"low"},"#,
            r#""metadata":{"k":"v"},"stream":false}"#
        )
    );
}

#[tokio::test]
async fn controls_and_empty_objects_keep_their_shape() {
    let lab = Lab::new();
    for extra in [
        json!({"text": {}}),
        json!({"metadata": {}}),
        json!({"text": {"verbosity": "high"}}),
        json!({"text": {"format": {"type": "json_object"}}}),
        json!({"tools": [weather_tool()], "tool_choice": "none"}),
        json!({"tools": [weather_tool()], "tool_choice": "required", "parallel_tool_calls": true}),
        json!({"tools": [{"type": "function", "name": "n", "parameters": null, "strict": false}]}),
    ] {
        let input: Value = serde_json::from_str(&req(extra)).unwrap();
        let s = lab.run(&input.to_string()).await.unwrap();
        assert_eq!(json_of(&s), input);
    }
}

#[tokio::test]
async fn parallel_calls_and_non_adjacent_outputs_are_accepted() {
    let lab = Lab::new();
    let input = with_input(json!([
        {"role": "user", "content": "q"},
        call("a", "{}"), call("b", "{\"x\":\"y\"}"),
        {"role": "assistant", "content": "thinking"},
        output("b", json!([{"type": "input_text", "text": "B"}])),
        output("a", json!("A"))
    ]));
    let s = lab.run(&input.to_string()).await.unwrap();
    assert_eq!(json_of(&s), input);
    // Unanswered calls are the provider's concern, not the gateway's.
    let open = with_input(json!([call("a", "{}")]));
    assert_eq!(json_of(&lab.run(&open.to_string()).await.unwrap()), open);
}

// -------------------------------------------------------------- redaction slot placement

#[tokio::test]
async fn every_redact_slot_replaces_a_secret_in_slot_order() {
    let lab = Lab::new();
    let args =
        json!({"a": format!("x {}", token(1)), "n": [format!("y {}", token(2)), 3]}).to_string();
    let body = json!({
        // Key order does not change slot order.
        "metadata": {"k": format!("m {}", token(8))},
        "text": {"format": {"type": "json_schema", "name": "ans",
            "description": format!("fd {}", token(6)),
            "schema": {"type": "object", "title": format!("ft {}", token(7)),
                "properties": {"p": {"type": "string", "description": format!("fs {}", token(9))}}}}},
        "tools": [{"type": "function", "name": "f", "description": format!("td {}", token(4)),
            "parameters": {"type": "object", "description": format!("ts {}", token(5)), "properties": {}},
            "strict": false}],
        "input": [
            call("c1", &args),
            output("c1", json!(format!("r {}", token(3)))),
            call("c2", "{}"),
            output("c2", json!([{"type": "input_text", "text": format!("p {}", token(10))}]))
        ],
        "model": "m", "store": false, "instructions": format!("i {}", token(0))
    })
    .to_string();
    let s = lab.run(&body).await.unwrap();
    assert!(!text(&s).contains("SYNTHETICREVOKED"));
    let v = json_of(&s);
    assert_eq!(v["instructions"], "i <SECRET_1>");
    let a: Value = serde_json::from_str(v["input"][0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(a, json!({"a": "x <SECRET_2>", "n": ["y <SECRET_3>", 3]}));
    assert_eq!(v["input"][1]["output"], "r <SECRET_4>");
    assert_eq!(v["input"][3]["output"][0]["text"], "p <SECRET_5>");
    assert_eq!(v["tools"][0]["description"], "td <SECRET_6>");
    assert_eq!(v["tools"][0]["parameters"]["description"], "ts <SECRET_7>");
    assert_eq!(v["text"]["format"]["description"], "fd <SECRET_8>");
    assert_eq!(v["text"]["format"]["schema"]["title"], "ft <SECRET_9>");
    assert_eq!(
        v["text"]["format"]["schema"]["properties"]["p"]["description"],
        "fs <SECRET_10>"
    );
    assert_eq!(v["metadata"]["k"], "m <SECRET_11>");
    // Structure is retained: same keys, same types, same order of items.
    assert_eq!(v["input"][2]["call_id"], "c2");
    assert_eq!(v["tools"][0]["strict"], false);
}

#[tokio::test]
async fn output_that_looks_like_json_is_text_and_never_parsed() {
    let lab = Lab::new();
    for out in [
        "{\"a\":",
        "{\"a\":1,\"a\":2}",
        "[1,2",
        "{\"deep\":[[[[[[[[[[[[1]]]]]]]]]]]]}",
    ] {
        let input = with_input(json!([call("c", "{}"), output("c", json!(out))]));
        let s = lab.run(&input.to_string()).await.unwrap();
        assert_eq!(json_of(&s)["input"][1]["output"], out);
    }
}

// ---------------------------------------------------------------- label slot placement

#[tokio::test]
async fn every_label_slot_with_a_secret_blocks_and_nothing_is_sealed() {
    let lab = Lab::new();
    let t = token(20);
    let cases: Vec<(&str, String)> = vec![
        ("call_id", req(json!({"input": [call(&t, "{}")]}))),
        (
            "call name",
            req(
                json!({"input": [{"type": "function_call", "call_id": "c", "name": t, "arguments": "{}"}]}),
            ),
        ),
        (
            "argument key",
            req(json!({"input": [call("c", &json!({t.clone(): 1}).to_string())]})),
        ),
        (
            "nested argument key",
            req(json!({"input": [call("c", &json!({"a": [{t.clone(): 1}]}).to_string())]})),
        ),
        (
            "output call_id",
            // The output must answer a declared call, so the call carries the same id.
            req(json!({"input": [call(&t, "{}"), output(&t, json!("x"))]})),
        ),
        (
            "tool name",
            req(
                json!({"tools": [{"type": "function", "name": t, "parameters": null, "strict": null}]}),
            ),
        ),
        (
            "tool_choice name",
            req(
                json!({"tools": [{"type": "function", "name": t, "parameters": null, "strict": null}],
                "tool_choice": {"type": "function", "name": t}}),
            ),
        ),
        (
            "tool schema property key",
            req(
                json!({"tools": [{"type": "function", "name": "f", "strict": null,
                "parameters": {"type": "object", "properties": {t.clone(): {"type": "string"}}}}]}),
            ),
        ),
        (
            "tool schema required",
            req(
                json!({"tools": [{"type": "function", "name": "f", "strict": null,
                "parameters": {"type": "object", "properties": {}, "required": [t]}}]}),
            ),
        ),
        (
            "tool schema enum",
            req(
                json!({"tools": [{"type": "function", "name": "f", "strict": null,
                "parameters": {"type": "object", "properties": {"p": {"type": "string", "enum": [t]}}}}]}),
            ),
        ),
        (
            "tool schema const",
            req(
                json!({"tools": [{"type": "function", "name": "f", "strict": null,
                "parameters": {"type": "object", "properties": {"p": {"const": t}}}}]}),
            ),
        ),
        (
            "format name",
            req(json!({"text": {"format": {"type": "json_schema", "name": t,
                "schema": {"type": "object", "properties": {}}}}})),
        ),
        (
            "format schema key",
            req(
                json!({"text": {"format": {"type": "json_schema", "name": "n",
                "schema": {"type": "object", "properties": {t.clone(): {"type": "string"}}}}}}),
            ),
        ),
        (
            "format schema enum",
            req(
                json!({"text": {"format": {"type": "json_schema", "name": "n",
                "schema": {"type": "object", "properties": {"p": {"enum": [t, "ok"]}}}}}}),
            ),
        ),
        ("metadata key", req(json!({"metadata": {t.clone(): "v"}}))),
    ];
    for (what, body) in &cases {
        assert_eq!(lab.run(body).await.unwrap_err(), blocked(), "{what}");
    }
    lab.assert_all_free().await;
}

#[tokio::test]
async fn escaped_secrets_are_decoded_before_inspection_in_every_decoded_position() {
    let lab = Lab::new();
    let esc = "\\u0067\\u0068\\u0070_SYNTHETICREVOKED00000000000000000030";
    // Escaped in the wire string AND inside the arguments string (two decoding layers).
    let args_inner = format!(r#"{{"k":"v {esc} \"q\" \\ \n 😀 é","z":"한국 {esc}"}}"#);
    let args_wire = args_inner.replace('\\', "\\\\").replace('"', "\\\"");
    let raw = format!(
        r#"{{"model":"m","store":false,"input":[{{"type":"function_call","call_id":"c","name":"f","arguments":"{args_wire}"}},{{"type":"function_call_output","call_id":"c","output":"o {esc}"}}],"metadata":{{"k":"m {esc}"}},"tools":[{{"type":"function","name":"f","description":"d {esc}","parameters":null,"strict":null}}]}}"#
    );
    assert!(!raw.contains("ghp_"));
    let s = lab.run(&raw).await.unwrap();
    assert!(!text(&s).contains("SYNTHETICREVOKED"));
    let v = json_of(&s);
    let a: Value = serde_json::from_str(v["input"][0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(a["k"], "v <SECRET_1> \"q\" \\ \n 😀 é");
    assert_eq!(a["z"], "한국 <SECRET_2>");
    assert_eq!(v["input"][1]["output"], "o <SECRET_3>");
    assert_eq!(v["tools"][0]["description"], "d <SECRET_4>");
    assert_eq!(v["metadata"]["k"], "m <SECRET_5>");
}

#[tokio::test]
async fn escaped_labels_are_decoded_before_the_label_scan() {
    let lab = Lab::new();
    let esc = "\\u0067\\u0068\\u0070_SYNTHETICREVOKED00000000000000000031";
    for raw in [
        format!(
            r#"{{"model":"m","store":false,"input":[{{"type":"function_call","call_id":"{esc}","name":"f","arguments":"{{}}"}}]}}"#
        ),
        format!(
            r#"{{"model":"m","store":false,"input":[{{"type":"function_call","call_id":"c","name":"f","arguments":"{{\"{esc}\":1}}"}}]}}"#
        ),
        format!(r#"{{"model":"m","store":false,"input":"x","metadata":{{"{esc}":"v"}}}}"#),
    ] {
        assert!(!raw.contains("ghp_"));
        // `\\u0067` inside the arguments string is a second layer; either layer decodes.
        assert!(lab.run(&raw).await.is_err());
    }
    lab.assert_all_free().await;
}

#[tokio::test]
async fn korean_unicode_and_escaping_survive_in_every_text_position() {
    let lab = Lab::new();
    let ko = "안녕하세요 김민수 café \u{1F600} \"q\" \\ \t";
    let body = json!({
        "model": "m", "store": false,
        "input": [
            call("c", &json!({"도시": ko}).to_string().replace("도시", "city")),
            output("c", json!([{"type": "input_text", "text": ko}]))
        ],
        "tools": [{"type": "function", "name": "f", "description": ko,
            "parameters": {"type": "object", "description": ko, "properties": {}}, "strict": null}],
        "text": {"format": {"type": "json_schema", "name": "n", "description": ko,
            "schema": {"type": "object", "title": ko, "properties": {}}}},
        "metadata": {"k": ko}
    });
    let s = lab.run(&body.to_string()).await.unwrap();
    let v = json_of(&s);
    let a: Value = serde_json::from_str(v["input"][0]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(a["city"], ko);
    assert_eq!(v["input"][1]["output"][0]["text"], ko);
    assert_eq!(v["tools"][0]["description"], ko);
    assert_eq!(v["tools"][0]["parameters"]["description"], ko);
    assert_eq!(v["text"]["format"]["description"], ko);
    assert_eq!(v["text"]["format"]["schema"]["title"], ko);
    assert_eq!(v["metadata"]["k"], ko);
    // Raw UTF-8 on the wire, not \u escapes (arguments are re-escaped once, as a string).
    assert!(text(&s).contains("안녕하세요"));
}

#[tokio::test]
async fn structural_fields_cannot_carry_free_text() {
    let lab = Lab::new();
    let t = token(40);
    let cases = [
        json!({"input": [{"type": t, "call_id": "c", "name": "f", "arguments": "{}"}]}),
        json!({"input": [{"type": "function_call", "call_id": "c", "name": "f", "arguments": "{}", t.clone(): 1}]}),
        json!({"input": [call("c", "{}"), {"type": "function_call_output", "call_id": "c", "output": "x", t.clone(): 1}]}),
        json!({"input": [call("c", "{}"), output("c", json!([{"type": t, "text": "x"}]))]}),
        json!({"tools": [{"type": t, "name": "f", "parameters": null, "strict": null}]}),
        json!({"tools": [{"type": "function", "name": "f", "parameters": null, "strict": t}]}),
        json!({"tools": [{"type": "function", "name": "f", "parameters": null, "strict": null, t.clone(): 1}]}),
        json!({"tools": [weather_tool()], "tool_choice": t}),
        json!({"tools": [weather_tool()], "tool_choice": {"type": t, "name": "get_weather"}}),
        json!({"tools": [weather_tool()], "parallel_tool_calls": t}),
        json!({"text": {"verbosity": t}}),
        json!({"text": {t.clone(): 1}}),
        json!({"text": {"format": {"type": t}}}),
        json!({"text": {"format": {"type": "json_object", t.clone(): 1}}}),
        json!({"text": {"format": {"type": "json_schema", "name": "n", "schema": {"type": "object"}, "strict": t}}}),
        json!({"metadata": {"k": 1}}),
    ];
    for extra in cases {
        let body = req(extra);
        assert_eq!(
            lab.reject(&body).await,
            ProtocolError::Unsupported,
            "{body}"
        );
    }
}

// -------------------------------------------------------------- zero-forward negatives

#[tokio::test]
async fn unsupported_and_malformed_forms_are_rejected_before_inspection() {
    let lab = Lab::new();
    let item = |i: Value| req(json!({"input": [i]}));
    let tool = |t: Value| req(json!({"tools": [t]}));
    let text_of = |t: Value| req(json!({"text": t}));
    let fmt = |f: Value| req(json!({"text": {"format": f}}));
    let schema_tool = |s: Value| {
        req(json!({"tools": [{"type": "function", "name": "f", "parameters": s, "strict": null}]}))
    };
    let cases: Vec<String> = vec![
        // function_call
        item(json!({"type": "function_call", "name": "f", "arguments": "{}"})),
        item(json!({"type": "function_call", "call_id": "c", "arguments": "{}"})),
        item(json!({"type": "function_call", "call_id": "c", "name": "f"})),
        item(json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": {}})),
        item(json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": null})),
        item(json!({"type": "function_call", "call_id": "bad id", "name": "f", "arguments": "{}"})),
        item(json!({"type": "function_call", "call_id": "", "name": "f", "arguments": "{}"})),
        item(
            json!({"type": "function_call", "call_id": "c", "name": "bad.name", "arguments": "{}"}),
        ),
        item(json!({"type": "function_call", "call_id": "c", "name": "", "arguments": "{}"})),
        item(json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": "[]"})),
        item(json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": "\"s\""})),
        item(
            json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": "{\"bad key\":1}"}),
        ),
        item(
            json!({"type": "function_call", "call_id": "c", "name": "f", "arguments": "{\"a\":99999999999999999999}"}),
        ),
        item(
            json!({"type": "function_call", "id": "fc_synthetic", "call_id": "c", "name": "f", "arguments": "{}"}),
        ),
        item(
            json!({"type": "function_call", "status": "completed", "call_id": "c", "name": "f", "arguments": "{}"}),
        ),
        item(
            json!({"type": "function_call", "caller": {}, "call_id": "c", "name": "f", "arguments": "{}"}),
        ),
        item(
            json!({"type": "function_call", "namespace": "n", "call_id": "c", "name": "f", "arguments": "{}"}),
        ),
        item(
            json!({"type": "function_call", "async": false, "call_id": "c", "name": "f", "arguments": "{}"}),
        ),
        // function_call_output
        item(json!({"type": "function_call_output", "output": "x"})),
        item(json!({"type": "function_call_output", "call_id": "c"})),
        req(json!({"input": [call("c", "{}"), output("c", Value::Null)]})),
        req(json!({"input": [call("c", "{}"), output("c", json!({"a": 1}))]})),
        req(json!({"input": [call("c", "{}"), output("c", json!(7))]})),
        req(json!({"input": [call("c", "{}"), output("c", json!([]))]})),
        req(
            json!({"input": [call("c", "{}"), output("c", json!([{"type": "input_image", "image_url": "https://example.invalid/a.png"}]))]}),
        ),
        req(
            json!({"input": [call("c", "{}"), output("c", json!([{"type": "input_file", "file_id": "f"}]))]}),
        ),
        req(
            json!({"input": [call("c", "{}"), output("c", json!([{"type": "input_text", "text": "x", "annotations": []}]))]}),
        ),
        req(
            json!({"input": [call("c", "{}"), {"type": "function_call_output", "call_id": "c", "output": "x", "name": "f"}]}),
        ),
        req(
            json!({"input": [call("c", "{}"), {"type": "function_call_output", "call_id": "c", "output": "x", "id": "o"}]}),
        ),
        req(
            json!({"input": [call("c", "{}"), {"type": "function_call_output", "call_id": "c", "output": "x", "status": "completed"}]}),
        ),
        // correlation
        item(output("c", json!("x"))),
        req(json!({"input": [output("c", json!("x")), call("c", "{}")]})),
        req(json!({"input": [call("c", "{}"), output("other", json!("x"))]})),
        req(json!({"input": [call("c", "{}"), output("c", json!("x")), output("c", json!("y"))]})),
        req(json!({"input": [call("c", "{}"), call("c", "{\"a\":1}")]})),
        req(json!({"input": [call("c", "{}"), output("c", json!("x")), call("c", "{}")]})),
        // other item types
        item(json!({"type": "custom_tool_call", "call_id": "c", "name": "n", "input": "x"})),
        item(json!({"type": "custom_tool_call_output", "call_id": "c", "output": "x"})),
        item(json!({"type": "web_search_call", "id": "w", "status": "completed"})),
        item(json!({"type": "file_search_call", "id": "w", "status": "completed", "queries": []})),
        item(json!({"type": "computer_call", "call_id": "c"})),
        item(json!({"type": "mcp_call", "id": "m"})),
        item(json!({"type": "shell_call", "call_id": "c"})),
        item(json!({"type": "apply_patch_call", "call_id": "c"})),
        item(json!({"type": "local_shell_call", "call_id": "c"})),
        item(json!({"type": "tool_search_call", "call_id": "c"})),
        item(json!({"type": "compaction", "encrypted_content": "x"})),
        item(json!({"type": "reasoning", "id": "rs", "encrypted_content": "x", "summary": []})),
        // tools
        req(json!({"tools": []})),
        req(json!({"tools": {}})),
        req(json!({"tools": null})),
        req(json!({"tools": ["f"]})),
        tool(json!({"type": "function", "parameters": null, "strict": null})),
        tool(json!({"type": "function", "name": "f", "strict": null})),
        tool(json!({"type": "function", "name": "f", "parameters": null})),
        tool(json!({"type": "function", "name": "f", "parameters": null, "strict": "yes"})),
        tool(
            json!({"type": "function", "name": "f", "description": null, "parameters": null, "strict": null}),
        ),
        tool(
            json!({"type": "function", "name": "f", "description": 5, "parameters": null, "strict": null}),
        ),
        tool(json!({"type": "function", "name": "bad name", "parameters": null, "strict": null})),
        tool(json!({"type": "function", "name": "", "parameters": null, "strict": null})),
        tool(
            json!({"type": "function", "name": "a".repeat(65), "parameters": null, "strict": null}),
        ),
        tool(
            json!({"type": "function", "name": "f", "parameters": null, "strict": null, "allowed_callers": ["x"]}),
        ),
        tool(
            json!({"type": "function", "name": "f", "parameters": null, "strict": null, "async": true}),
        ),
        tool(
            json!({"type": "function", "name": "f", "parameters": null, "strict": null, "defer_loading": true}),
        ),
        tool(
            json!({"type": "function", "name": "f", "parameters": null, "strict": null, "output_schema": {}}),
        ),
        tool(
            json!({"type": "function", "name": "f", "parameters": null, "strict": null, "namespace": "n"}),
        ),
        // Chat-shaped tool
        tool(
            json!({"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}),
        ),
        // hosted / remote tools
        tool(json!({"type": "web_search"})),
        tool(json!({"type": "web_search_preview"})),
        tool(json!({"type": "file_search", "vector_store_ids": ["v"]})),
        tool(json!({"type": "computer"})),
        tool(json!({"type": "computer_use_preview"})),
        tool(json!({"type": "mcp", "server_label": "s", "server_url": "https://example.invalid"})),
        tool(json!({"type": "code_interpreter", "container": "auto"})),
        tool(json!({"type": "image_generation"})),
        tool(json!({"type": "local_shell"})),
        tool(json!({"type": "shell"})),
        tool(json!({"type": "apply_patch"})),
        tool(json!({"type": "custom", "name": "c"})),
        tool(json!({"type": "namespace", "name": "n", "tools": []})),
        tool(json!({"type": "tool_search"})),
        tool(json!({"type": "programmatic_tool_calling"})),
        req(json!({"tools": [weather_tool(), weather_tool()]})),
        // schema subset
        schema_tool(json!({"type": "string"})),
        schema_tool(json!({"properties": {}})),
        schema_tool(json!({"type": "object", "$ref": "#/$defs/x"})),
        schema_tool(json!({"type": "object", "$defs": {}})),
        schema_tool(
            json!({"type": "object", "properties": {"p": {"type": "string", "default": "x"}}}),
        ),
        schema_tool(
            json!({"type": "object", "properties": {"p": {"type": "string", "pattern": "^a"}}}),
        ),
        schema_tool(
            json!({"type": "object", "properties": {"p": {"type": "string", "format": "email"}}}),
        ),
        schema_tool(
            json!({"type": "object", "properties": {"p": {"type": "string"}}, "additionalProperties": {}}),
        ),
        schema_tool(json!({"type": "object", "properties": {"p": {"allOf": []}}})),
        schema_tool(json!({"type": "object", "properties": {"bad key": {"type": "string"}}})),
        schema_tool(json!({"type": "object", "properties": {"p": {"enum": ["has\nnewline"]}}})),
        // tool_choice
        req(json!({"tool_choice": "auto"})),
        req(json!({"parallel_tool_calls": true})),
        req(json!({"tools": [weather_tool()], "tool_choice": "any"})),
        req(json!({"tools": [weather_tool()], "tool_choice": null})),
        req(
            json!({"tools": [weather_tool()], "tool_choice": {"type": "function", "name": "undeclared"}}),
        ),
        req(
            json!({"tools": [weather_tool()], "tool_choice": {"type": "function", "function": {"name": "get_weather"}}}),
        ),
        req(json!({"tools": [weather_tool()], "tool_choice": {"type": "function"}})),
        req(
            json!({"tools": [weather_tool()], "tool_choice": {"type": "allowed_tools", "mode": "auto", "tools": []}}),
        ),
        req(
            json!({"tools": [weather_tool()], "tool_choice": {"type": "custom", "name": "get_weather"}}),
        ),
        req(
            json!({"tools": [weather_tool()], "tool_choice": {"type": "mcp", "server_label": "s"}}),
        ),
        req(json!({"tools": [weather_tool()], "tool_choice": {"type": "web_search"}})),
        req(json!({"tools": [weather_tool()], "tool_choice": {"type": "shell"}})),
        req(
            json!({"tools": [weather_tool()], "tool_choice": {"type": "function", "name": "get_weather", "extra": 1}}),
        ),
        req(json!({"tools": [weather_tool()], "parallel_tool_calls": null})),
        // text
        text_of(Value::Null),
        text_of(json!([])),
        text_of(json!({"format": null})),
        text_of(json!({"verbosity": null})),
        text_of(json!({"verbosity": "max"})),
        text_of(json!({"stop": "x"})),
        fmt(json!({})),
        fmt(json!({"type": "text", "name": "n"})),
        fmt(json!({"type": "json_object", "schema": {}})),
        fmt(json!({"type": "grammar"})),
        // Chat-shaped format
        fmt(
            json!({"type": "json_schema", "json_schema": {"name": "answer", "schema": {"type": "object"}}}),
        ),
        fmt(json!({"type": "json_schema", "name": "n"})),
        fmt(json!({"type": "json_schema", "schema": {"type": "object"}})),
        fmt(json!({"type": "json_schema", "name": "bad name", "schema": {"type": "object"}})),
        fmt(
            json!({"type": "json_schema", "name": "n", "schema": {"type": "object"}, "strict": null}),
        ),
        fmt(
            json!({"type": "json_schema", "name": "n", "schema": {"type": "object"}, "description": null}),
        ),
        fmt(json!({"type": "json_schema", "name": "n", "schema": {"type": "array"}})),
        fmt(json!({"type": "json_schema", "name": "n", "schema": {"type": "object", "$ref": "#"}})),
        fmt(json!({"type": "json_schema", "name": "n", "schema": null})),
        // metadata
        req(json!({"metadata": null})),
        req(json!({"metadata": []})),
        req(json!({"metadata": {"k": null}})),
        req(json!({"metadata": {"k": true}})),
        req(json!({"metadata": {"k": {"a": "b"}}})),
        req(json!({"metadata": {"k": ["a"]}})),
        req(json!({"metadata": {"bad key": "v"}})),
        req(json!({"metadata": {"": "v"}})),
        req(json!({"metadata": {"k": "a".repeat(513)}})),
    ];
    for body in &cases {
        let err = lab.reject(body).await;
        assert!(
            matches!(
                err,
                ProtocolError::Unsupported | ProtocolError::LimitExceeded
            ),
            "{err:?}: {body}"
        );
    }
    // Over-long fields are limits (413), the rest of the matrix is unsupported (422).
    assert_eq!(
        lab.reject(&req(json!({"metadata": {"k": "a".repeat(513)}})))
            .await,
        ProtocolError::Unsupported
    );
    lab.assert_all_free().await;
}

#[tokio::test]
async fn malformed_and_duplicate_key_arguments_are_malformed() {
    let lab = Lab::new();
    for args in [
        "",
        "{",
        "{\"a\":1,}",
        "{\"a\":1} x",
        "{\"a\":1,\"a\":2}",
        "{\"a\":{\"b\":1,\"\\u0062\":2}}",
        "{\"a\":\"\\ud800\"}",
        "nul",
    ] {
        let body = req(json!({"input": [call("c", args)]}));
        assert_eq!(
            lab.reject(&body).await,
            ProtocolError::Malformed,
            "{args:?}"
        );
    }
    lab.assert_all_free().await;
}

#[tokio::test]
async fn counts_depth_and_nested_schema_limits_are_limit_exceeded() {
    let lab = Lab::new();
    // Eight nested argument objects (root included) pass, nine are a limit.
    let nested = |depth: usize| {
        let mut s = String::from("1");
        for _ in 1..depth {
            s = format!("{{\"a\":{s}}}");
        }
        s
    };
    let ok = req(json!({"input": [call("c", &nested(9))]}));
    assert!(lab.validate(Protocol::ResponsesText, &ok).await.is_ok());
    let deep = req(json!({"input": [call("c", &nested(10))]}));
    assert_eq!(lab.reject(&deep).await, ProtocolError::LimitExceeded);
    // 65 tools, 65 output parts.
    let tools: Vec<Value> = (0..65)
        .map(|i| json!({"type": "function", "name": format!("t{i}"), "parameters": null, "strict": null}))
        .collect();
    assert_eq!(
        lab.reject(&req(json!({"tools": tools}))).await,
        ProtocolError::LimitExceeded
    );
    let parts: Vec<Value> = (0..65)
        .map(|_| json!({"type": "input_text", "text": "x"}))
        .collect();
    assert_eq!(
        lab.reject(&req(
            json!({"input": [call("c", "{}"), output("c", json!(parts))]})
        ))
        .await,
        ProtocolError::LimitExceeded
    );
    // Schema nesting: 8 nested schema objects pass, 9 are a limit; over-long description.
    let schema = |depth: usize| {
        let mut s = json!({"type": "string"});
        for _ in 1..depth {
            s = json!({"type": "object", "properties": {"p": s}});
        }
        s
    };
    let mut limits = RequestLimits::provisional();
    limits.max_depth = 64;
    let deep_lab = Lab::with(limits, 2);
    let tool = |s: Value| {
        req(json!({"tools": [{"type": "function", "name": "f", "parameters": s, "strict": null}]}))
    };
    assert!(
        deep_lab
            .validate(Protocol::ResponsesText, &tool(schema(8)))
            .await
            .is_ok()
    );
    assert_eq!(
        deep_lab.reject(&tool(schema(9))).await,
        ProtocolError::LimitExceeded
    );
    let long = req(json!({"tools": [{"type": "function", "name": "f",
        "description": "d".repeat(4097), "parameters": null, "strict": null}]}));
    assert_eq!(lab.reject(&long).await, ProtocolError::LimitExceeded);
    lab.assert_all_free().await;
}

// ------------------------------------------------------------ request-wide shared budgets

#[tokio::test]
async fn derived_budgets_are_shared_by_calls_tools_schemas_and_metadata() {
    // Each decoded argument tree costs 1 + 2 * 12 = 25 derived nodes but a single wire node,
    // so the derived counter, not the wire parse, is what these limits exercise.
    let args = json!({"a": (0..24).collect::<Vec<_>>()}).to_string();
    let one = json!({"input": [call("c1", &args)]});
    let two = json!({"input": [call("c1", &args), call("c2", &args)]});
    let mut found = None;
    for n in 20..400_u32 {
        let mut limits = RequestLimits::provisional();
        limits.max_nodes = n;
        let lab = Lab::with(limits, 2);
        if lab
            .validate(Protocol::ResponsesText, &req(one.clone()))
            .await
            .is_ok()
        {
            found = Some(n);
            break;
        }
    }
    let n = found.expect("one call fits some limit");
    let mut limits = RequestLimits::provisional();
    limits.max_nodes = n;
    let lab = Lab::with(limits, 2);
    // The same limit rejects two calls: the budget is request-wide, not per call.
    assert_eq!(lab.reject(&req(two)).await, ProtocolError::LimitExceeded);
    // A call plus a schema, or a call plus metadata, also shares it (key order is free).
    let many_props: serde_json::Map<String, Value> = (0..n)
        .map(|i| (format!("p{i}"), json!({"type": "string"})))
        .take(30)
        .collect();
    let with_schema = json!({"input": [call("c1", &args)],
        "text": {"format": {"type": "json_schema", "name": "n",
            "schema": {"type": "object", "properties": many_props}}}});
    let err = lab.reject(&req(with_schema)).await;
    assert_eq!(err, ProtocolError::LimitExceeded);
    lab.assert_all_free().await;
}

#[tokio::test]
async fn one_findings_budget_spans_messages_tools_schemas_and_metadata() {
    let lab = Lab::with_policy(
        &ContentPolicy::new(Profile::Full).with_max_findings(3),
        RequestLimits::provisional(),
        2,
    );
    let body = json!({
        "model": "m", "store": false,
        "input": [
            call("c", &json!({"a": token(50)}).to_string()),
            output("c", json!(token(51)))
        ],
        "tools": [{"type": "function", "name": "f", "description": token(52),
            "parameters": null, "strict": null}],
        "metadata": {"k": token(53)}
    })
    .to_string();
    let err = lab.run(&body).await.unwrap_err();
    assert_eq!(err, BoundaryError::Core(CoreBridgeError::LimitExceeded));
    // The same request with one fewer secret is within the budget.
    let ok = json!({
        "model": "m", "store": false,
        "input": [call("c", &json!({"a": token(50)}).to_string()), output("c", json!(token(51)))],
        "tools": [{"type": "function", "name": "f", "description": token(52),
            "parameters": null, "strict": null}]
    })
    .to_string();
    assert!(lab.run(&ok).await.is_ok());
    lab.assert_all_free().await;
}

#[tokio::test]
async fn a_chat_shaped_request_is_not_a_responses_request() {
    let lab = Lab::new();
    for body in [
        req(
            json!({"tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}]}),
        ),
        req(json!({"response_format": {"type": "json_object"}})),
        json!({"model": "m", "store": false, "messages": [{"role": "user", "content": "x"}]})
            .to_string(),
        req(json!({"input": [{"role": "assistant", "content": null, "tool_calls": []}]})),
        req(json!({"input": [{"role": "tool", "tool_call_id": "c", "content": "x"}]})),
    ] {
        assert_eq!(
            lab.reject(&body).await,
            ProtocolError::Unsupported,
            "{body}"
        );
    }
}

#[tokio::test]
async fn every_synthetic_example_of_the_contract_behaves_as_labelled() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/contracts/responses-request.md"),
    )
    .unwrap();
    let lab = Lab::new();
    let mut lines = doc.lines();
    let (mut accepted, mut rejected) = (0, 0);
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("```json ") else {
            continue;
        };
        let kind = rest.split_whitespace().next().unwrap_or("");
        let mut body = String::new();
        for l in lines.by_ref() {
            if l.starts_with("```") {
                break;
            }
            body.push_str(l);
        }
        match kind {
            "accepted" => {
                accepted += 1;
                lab.run(&body).await.unwrap();
            }
            "rejected" => {
                rejected += 1;
                lab.reject(&body).await;
            }
            other => panic!("unknown kind {other}"),
        }
    }
    assert!(accepted >= 2 && rejected >= 10);
    lab.assert_all_free().await;
}
