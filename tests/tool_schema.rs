//! Tool definitions and structured-output schemas (issue #54; ADR 0025 D1 to D7, D11, D12;
//! `docs/contracts/chat-completions-request.md`).
//!
//! Boundary-level tests drive the real `Inspection` service from the typed request to the
//! sealed body and compare the forwarded structure with the input. A second group runs the
//! served router over loopback with a fake upstream that must see zero connections and zero
//! body bytes for every rejection. Every secret is a synthetic revoked-style token; all
//! other data is invented. Non-ASCII text appears both literally and as `\u` escapes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod support;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redact_secret::Profile;
use redact_secret_gateway::admission::{Admission, CapacityPlan, RequestLimits};
use redact_secret_gateway::boundary::{BoundaryError, Inspection, SanitizedRequest};
use redact_secret_gateway::chat_route::{CHAT_COMPLETIONS_PATH, ChatRoute};
use redact_secret_gateway::config::{self, ContentPolicy, OnWarn, RouteId};
use redact_secret_gateway::core_bridge::CoreBridgeError;
use redact_secret_gateway::protocol::{self, Protocol, ProtocolError};
use redact_secret_gateway::server::{self, Services, StartupError};
use serde_json::{Value, json};
use support::fake_upstream::{Behavior, FakeUpstream};
use support::leak::Markers;
use support::raw_http::{Response, parse_response, post};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

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
}

impl Lab {
    fn new(content: &ContentPolicy) -> Self {
        let plan = CapacityPlan::new(nz(4), nz(8192), nz(2), nz(1), nz(1));
        let admission = Arc::new(Admission::new(&plan));
        let limits = RequestLimits::provisional();
        let inspection =
            Inspection::start(Arc::clone(&admission), content, &limits, &plan).unwrap();
        Self {
            admission,
            inspection,
            limits,
        }
    }

    /// Parse and classify only; the typed error is the pre-inspection outcome.
    async fn validate(&self, body: &str) -> Result<protocol::ValidatedRequest, ProtocolError> {
        let received = self
            .admission
            .begin_body_receipt(body.len(), &self.limits)
            .await
            .unwrap()
            .complete(body.as_bytes().to_vec())
            .unwrap();
        protocol::validate_with(received, Protocol::ChatCompletionsText, &self.limits)
    }

    async fn run(&self, body: &str) -> Result<SanitizedRequest, BoundaryError> {
        let validated = self.validate(body).await.expect("classified");
        self.inspection
            .inspect_and_approve(validated, RouteId::new("synthetic-route"))
            .await
    }
}

fn full() -> ContentPolicy {
    ContentPolicy::new(Profile::Full)
}

fn out(s: &SanitizedRequest) -> Value {
    serde_json::from_slice(s.body()).expect("sanitized body is valid JSON")
}

fn shell(extra: Value) -> Value {
    let mut base = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "q"}]});
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

// ------------------------------------------------------------------ positive round trips

/// Every approved keyword and value class survives inspection with its shape: the forwarded
/// document, parsed, equals the input structure.
#[tokio::test]
async fn every_keyword_class_round_trips_through_inspection() {
    let lab = Lab::new(&full());
    let parameters = json!({
        "type": "object",
        "description": "Search arguments. 검색 인자.",
        "properties": {
            "query": {"type": "string", "description": "Free text.", "minLength": 1, "maxLength": 200},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50},
            "ratio": {"type": "number", "minimum": -0.5, "maximum": 2.5},
            "unit": {"type": "string", "enum": ["celsius", "서울 city", "a.b:c/d+e-f"]},
            "mixed": {"enum": [1, 2.5, true, null, "x"]},
            "mode": {"const": "fast"},
            "flag": {"type": "boolean"},
            "maybe": {"type": ["string", "null"]},
            "tags": {"type": "array", "items": {"type": "string"}, "minItems": 0, "maxItems": 8},
            "either": {"anyOf": [{"type": "string"}, {"type": "integer"}, {"type": "null"}]},
            "nested": {"type": "object", "properties": {"inner": {"type": "string"}},
                       "required": ["inner"], "additionalProperties": false}
        },
        "required": ["query", "unit"],
        "additionalProperties": false
    });
    let schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "string", "description": "The answer."},
                       "score": {"type": "integer", "minimum": 0}},
        "required": ["answer", "score"],
        "additionalProperties": false
    });
    let request = shell(json!({
        "tools": [
            {"type": "function", "function": {
                "name": "search_docs", "description": "Search the documents.",
                "parameters": parameters, "strict": true}},
            {"type": "function", "function": {"name": "ping"}},
            {"type": "function", "function": {
                "name": "no_args", "parameters": {"type": "object", "properties": {}}}}
        ],
        "tool_choice": {"type": "function", "function": {"name": "search_docs"}},
        "parallel_tool_calls": false,
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "final_answer", "description": "Structured answer.", "strict": true, "schema": schema}}
    }));
    let sealed = lab.run(&request.to_string()).await.unwrap();
    assert_eq!(out(&sealed), request);

    // The same document written with `\u` escapes (decoded before every check) and the
    // keys in a different order forwards the same structure.
    let escaped = r#"{"response_format":{"json_schema":{"schema":{"required":["a"],"properties":{"a":{"enum":["서울","Ab"],"description":"한국어"}},"type":"object"},"name":"n"},"type":"json_schema"},
        "model":"m","messages":[{"content":"q","role":"user"}]}"#;
    let sealed = lab.run(escaped).await.unwrap();
    assert_eq!(
        out(&sealed),
        json!({"model": "m", "messages": [{"role": "user", "content": "q"}],
               "response_format": {"type": "json_schema", "json_schema": {"name": "n", "schema": {
                   "type": "object",
                   "properties": {"a": {"description": "한국어", "enum": ["서울", "Ab"]}},
                   "required": ["a"]}}}})
    );
}

/// `tool_choice` string forms and `parallel_tool_calls` true.
#[tokio::test]
async fn tool_choice_string_forms_and_parallel_flag_round_trip() {
    let lab = Lab::new(&full());
    for choice in ["none", "auto", "required"] {
        let request = shell(json!({
            "tools": [{"type": "function", "function": {"name": "f"}}],
            "tool_choice": choice, "parallel_tool_calls": true}));
        assert_eq!(out(&lab.run(&request.to_string()).await.unwrap()), request);
    }
}

/// Numbers keep their value; an exponent literal is written in the JSON writer's canonical
/// form (documented limit, ADR 0025 D10).
#[tokio::test]
async fn numeric_literals_keep_their_value_and_are_written_canonically() {
    let lab = Lab::new(&full());
    let body = r#"{"model":"m","messages":[{"role":"user","content":"q"}],
      "response_format":{"type":"json_schema","json_schema":{"name":"n","schema":{"type":"object",
      "properties":{"a":{"type":"number","minimum":-9223372036854775808,"maximum":1e3},
                    "b":{"type":"integer","minimum":0.25,"maxLength":9223372036854775807}}}}}}"#;
    let sealed = lab.run(body).await.unwrap();
    let text = std::str::from_utf8(sealed.body()).unwrap();
    assert!(
        text.contains(r#""minimum":-9223372036854775808,"maximum":1000.0"#),
        "{text}"
    );
    assert!(
        text.contains(r#""minimum":0.25,"maxLength":9223372036854775807"#),
        "{text}"
    );
}

// ------------------------------------------------------------ secret placement: text

/// Descriptions (tool, schema keyword, response format, response schema keyword) are free
/// text: redacted in place in traversal order, structure and flags untouched.
#[tokio::test]
async fn secrets_in_descriptions_are_redacted_in_place_in_order() {
    let lab = Lab::new(&full());
    let request = shell(json!({
        "tools": [{"type": "function", "function": {
            "name": "lookup",
            "description": format!("tool {}", token(1)),
            "strict": true,
            "parameters": {
                "type": "object",
                "description": format!("root 키 {}", token(2)),
                "properties": {"q": {"type": "string", "description": format!("leaf {}", token(3))}},
                "required": ["q"], "additionalProperties": false}}}],
        "tool_choice": "auto",
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "ans", "description": format!("resp {}", token(4)), "strict": true,
            "schema": {"type": "object",
                       "properties": {"a": {"type": "string", "description": format!("rleaf {}", token(5))}},
                       "required": ["a"]}}}
    }));
    let sealed = lab.run(&request.to_string()).await.unwrap();
    let wire = std::str::from_utf8(sealed.body()).unwrap();
    assert!(!wire.contains("SYNTHETICREVOKED"), "{wire}");
    let v = out(&sealed);
    let f = &v["tools"][0]["function"];
    assert_eq!(f["description"], "tool <SECRET_1>");
    assert_eq!(f["parameters"]["description"], "root 키 <SECRET_2>");
    assert_eq!(
        f["parameters"]["properties"]["q"]["description"],
        "leaf <SECRET_3>"
    );
    let rf = &v["response_format"]["json_schema"];
    assert_eq!(rf["description"], "resp <SECRET_4>");
    assert_eq!(
        rf["schema"]["properties"]["a"]["description"],
        "rleaf <SECRET_5>"
    );
    // Everything except those texts is identical to the input.
    let mut expected = request.clone();
    expected["tools"][0]["function"]["description"] = json!("tool <SECRET_1>");
    expected["tools"][0]["function"]["parameters"]["description"] = json!("root 키 <SECRET_2>");
    expected["tools"][0]["function"]["parameters"]["properties"]["q"]["description"] =
        json!("leaf <SECRET_3>");
    expected["response_format"]["json_schema"]["description"] = json!("resp <SECRET_4>");
    expected["response_format"]["json_schema"]["schema"]["properties"]["a"]["description"] =
        json!("rleaf <SECRET_5>");
    assert_eq!(v, expected);
}

/// A description exactly at the 4096-byte bound that contains a secret is redacted in place
/// and stays within the bound (the revalidation unit tests cover growth past it).
#[tokio::test]
async fn description_at_the_bound_with_a_secret_is_redacted_within_it() {
    let lab = Lab::new(&full());
    let pad = "a".repeat(4096 - 41);
    let ok = shell(json!({"tools": [{"type": "function", "function": {
        "name": "f", "description": format!("{pad} {}", token(1))}}]}));
    let sealed = lab.run(&ok.to_string()).await.unwrap();
    let desc = out(&sealed)["tools"][0]["function"]["description"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(desc == format!("{pad} <SECRET_1>"), "unexpected redaction");
    assert!(desc.len() <= 4096);
}

/// `title` is free text like `description` (#57, ADR 0029): it round-trips in canonical
/// position (after `type`, before `description`), a secret in it is redacted in place, and
/// a property that is merely named `title` is still an ordinary label.
#[tokio::test]
async fn title_round_trips_and_a_secret_in_it_is_redacted_in_place() {
    let lab = Lab::new(&full());
    let request = shell(json!({
        "tools": [{"type": "function", "function": {
            "name": "lookup",
            "parameters": {
                "type": "object",
                "title": format!("루트 {}", token(1)),
                "description": "d",
                "properties": {
                    "title": {"type": "string", "title": "Title", "description": format!("x {}", token(2))},
                    "n": {"anyOf": [{"type": "string", "title": "S"}, {"type": "null"}], "title": "N"}},
                "required": ["title", "n"], "additionalProperties": false}}}],
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "ans", "strict": true,
            "schema": {"type": "object", "title": format!("resp {}", token(3)),
                       "properties": {"a": {"type": "string", "title": "A"}}, "required": ["a"]}}}
    }));
    let sealed = lab.run(&request.to_string()).await.unwrap();
    let wire = std::str::from_utf8(sealed.body()).unwrap();
    assert!(!wire.contains("SYNTHETICREVOKED"), "{wire}");
    // Canonical order: `title` is written right after `type`, before `description`.
    assert!(
        wire.contains(r#""type":"object","title":"루트 <SECRET_1>","description":"d""#),
        "{wire}"
    );
    let mut expected = request.clone();
    expected["tools"][0]["function"]["parameters"]["title"] = json!("루트 <SECRET_1>");
    expected["tools"][0]["function"]["parameters"]["properties"]["title"]["description"] =
        json!("x <SECRET_2>");
    expected["response_format"]["json_schema"]["schema"]["title"] = json!("resp <SECRET_3>");
    assert_eq!(out(&sealed), expected);
}

// ------------------------------------------------------- secret placement: labels block

/// A finding in any label position blocks the request: nothing is rewritten and no sealed
/// request exists. Detect-only ignores the warning mode.
#[tokio::test]
async fn secrets_in_labels_block_and_are_never_rewritten() {
    let t = token(7);
    let tool = |name: &str, params: Value| {
        shell(
            json!({"tools": [{"type": "function", "function": {"name": name, "parameters": params}}]}),
        )
    };
    let obj = |props: Value| json!({"type": "object", "properties": props});
    let cases: Vec<(&str, Value)> = vec![
        ("tool name", tool(&t, obj(json!({})))),
        (
            "property key",
            tool("f", obj(json!({ t.clone(): {"type": "string"} }))),
        ),
        (
            "required entry",
            tool(
                "f",
                json!({"type": "object", "properties": {}, "required": [t.clone()]}),
            ),
        ),
        (
            "enum string",
            tool("f", obj(json!({"a": {"enum": [t.clone()]}}))),
        ),
        (
            "const string",
            tool("f", obj(json!({"a": {"const": t.clone()}}))),
        ),
        (
            "nested anyOf property key",
            tool(
                "f",
                obj(json!({"a": {"anyOf": [obj(json!({ t.clone(): {"type": "string"} }))]}})),
            ),
        ),
        (
            "response schema name",
            shell(
                json!({"response_format": {"type": "json_schema", "json_schema": {
                "name": t.clone(), "schema": {"type": "object"}}}}),
            ),
        ),
        (
            "response property key",
            shell(
                json!({"response_format": {"type": "json_schema", "json_schema": {
                "name": "n", "schema": obj(json!({ t.clone(): {"type": "string"} }))}}}),
            ),
        ),
        (
            "response required entry",
            shell(
                json!({"response_format": {"type": "json_schema", "json_schema": {
                "name": "n", "schema": {"type": "object", "required": [t.clone()]}}}}),
            ),
        ),
        (
            "response enum string",
            shell(
                json!({"response_format": {"type": "json_schema", "json_schema": {
                "name": "n", "schema": obj(json!({"a": {"enum": [t.clone()]}}))}}}),
            ),
        ),
        (
            "response const string",
            shell(
                json!({"response_format": {"type": "json_schema", "json_schema": {
                "name": "n", "schema": obj(json!({"a": {"const": t.clone()}}))}}}),
            ),
        ),
    ];
    for on_warn in [OnWarn::Reject, OnWarn::Forward] {
        let lab = Lab::new(&full().with_on_warn(on_warn));
        for (name, body) in &cases {
            let err = lab.run(&body.to_string()).await.unwrap_err();
            assert_eq!(err, BoundaryError::Core(CoreBridgeError::Blocked), "{name}");
        }
        // The clean twin of every case is accepted, so the block is the label scan.
        let clean = tool(
            "good_name",
            obj(json!({"good_key": {"enum": ["good value"], "const": null}})),
        );
        assert!(lab.run(&clean.to_string()).await.is_ok());
    }
}

/// `tool_choice` carries a label too; it must name a declared tool, so the declared name is
/// what the scan blocks. A clean pair passes.
#[tokio::test]
async fn tool_choice_name_is_scanned_through_the_declared_tool() {
    let lab = Lab::new(&full());
    let t = token(8);
    let request = shell(json!({
        "tools": [{"type": "function", "function": {"name": t.clone()}}],
        "tool_choice": {"type": "function", "function": {"name": t}}}));
    assert_eq!(
        lab.run(&request.to_string()).await.unwrap_err(),
        BoundaryError::Core(CoreBridgeError::Blocked)
    );
}

/// Labels outside their charset never reach the core: they are classification failures.
#[tokio::test]
async fn disguised_secrets_in_labels_fail_the_charset_before_inspection() {
    let lab = Lab::new(&full());
    let bearer = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789";
    let tool = |params: Value| {
        shell(
            json!({"tools": [{"type": "function", "function": {"name": "f", "parameters": params}}]}),
        )
    };
    for body in [
        tool(json!({"type": "object", "properties": {bearer: {"type": "string"}}})),
        tool(json!({"type": "object", "required": [bearer]})),
        tool(json!({"type": "object", "properties": {"a": {"enum": [format!("<{}>", token(9))]}}})),
        tool(json!({"type": "object", "properties": {"a": {"const": "line\nbreak"}}})),
        shell(json!({"tools": [{"type": "function", "function": {"name": bearer}}]})),
    ] {
        assert_eq!(
            lab.validate(&body.to_string()).await.err(),
            Some(ProtocolError::Unsupported)
        );
    }
}

/// Redaction numbering is request-wide and label scans consume none.
#[tokio::test]
async fn labels_do_not_consume_placeholder_numbers() {
    let lab = Lab::new(&full());
    let request = shell(json!({
        "messages": [{"role": "user", "content": format!("first {}", token(1))}],
        "tools": [{"type": "function", "function": {
            "name": "clean_name", "description": format!("second {}", token(2)),
            "parameters": {"type": "object", "properties": {"k": {"type": "string"}}}}}]}));
    let v = out(&lab.run(&request.to_string()).await.unwrap());
    assert_eq!(v["messages"][0]["content"], "first <SECRET_1>");
    assert_eq!(
        v["tools"][0]["function"]["description"],
        "second <SECRET_2>"
    );
    assert_eq!(v["tools"][0]["function"]["name"], "clean_name");
}

// ------------------------------------------------------------------ rejection table

fn tool_body(params: &str) -> String {
    format!(
        r#"{{"model":"m","messages":[{{"role":"user","content":"q"}}],"tools":[{{"type":"function","function":{{"name":"f","parameters":{params}}}}}]}}"#
    )
}

fn nested_items(levels: usize) -> String {
    let mut s = r#"{"type":"string"}"#.to_owned();
    for _ in 0..levels {
        s = format!(r#"{{"type":"array","items":{s}}}"#);
    }
    s
}

/// Rejected bodies with their typed outcome, shared by the lab and the HTTP test.
fn rejected_cases() -> Vec<(&'static str, String, ProtocolError)> {
    use ProtocolError::{LimitExceeded as L, Malformed as M, Unsupported as U};
    let many_objects = {
        let group = format!(r#"{{"anyOf":[{}]}}"#, ["{}"; 8].join(","));
        let props: Vec<String> = (0..30).map(|i| format!(r#""p{i}":{group}"#)).collect();
        tool_body(&format!(
            r#"{{"type":"object","properties":{{{}}}}}"#,
            props.join(",")
        ))
    };
    let oversized = format!(
        r#"{{"type":"object","description":"{}"}}"#,
        "a".repeat(4097)
    );
    vec![
        ("external $ref", tool_body(r#"{"type":"object","properties":{"a":{"$ref":"https://example.invalid/s.json"}}}"#), U),
        ("local $ref and $defs", tool_body(r##"{"type":"object","$defs":{"x":{"type":"string"}},"properties":{"a":{"$ref":"#/$defs/x"}}}"##), U),
        ("$id and $schema", tool_body(r#"{"type":"object","$id":"https://example.invalid/s","$schema":"https://json-schema.org/draft/2020-12/schema"}"#), U),
        ("allOf", tool_body(r#"{"type":"object","allOf":[{"type":"object"}]}"#), U),
        ("oneOf", tool_body(r#"{"type":"object","oneOf":[{"type":"object"}]}"#), U),
        ("not", tool_body(r#"{"type":"object","not":{"type":"object"}}"#), U),
        ("if", tool_body(r#"{"type":"object","if":{"type":"object"},"then":{"type":"object"}}"#), U),
        ("pattern", tool_body(r#"{"type":"object","properties":{"a":{"type":"string","pattern":"^a+$"}}}"#), U),
        ("format", tool_body(r#"{"type":"object","properties":{"a":{"type":"string","format":"email"}}}"#), U),
        ("default", tool_body(r#"{"type":"object","properties":{"a":{"type":"string","default":"SYNTHETIC"}}}"#), U),
        ("examples", tool_body(r#"{"type":"object","properties":{"a":{"type":"string","examples":["SYNTHETIC"]}}}"#), U),
        ("title of the wrong type", tool_body(r#"{"type":"object","title":5}"#), U),
        ("title null", tool_body(r#"{"type":"object","properties":{"a":{"type":"string","title":null}}}"#), U),
        ("oversized title", tool_body(&format!(r#"{{"type":"object","title":"{}"}}"#, "a".repeat(4097))), L),
        ("duplicate title", tool_body(r#"{"type":"object","title":"A","title":"B"}"#), M),
        ("title on a tool function (not a schema keyword)", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f","title":"T"}}]}"#.to_owned(), U),
        ("title beside json_schema name (not a schema keyword)", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"response_format":{"type":"json_schema","json_schema":{"name":"n","title":"T","schema":{"type":"object"}}}}"#.to_owned(), U),
        ("unknown nested keyword", tool_body(r#"{"type":"object","properties":{"a":{"type":"string","x-vendor":true}}}"#), U),
        ("additionalProperties schema", tool_body(r#"{"type":"object","additionalProperties":{"type":"string"}}"#), U),
        ("root not an object schema", tool_body(r#"{"type":"string"}"#), U),
        ("unknown nested field in a tool", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f"},"extra":1}]}"#.to_owned(), U),
        ("unknown nested field in function", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f","extra":1}}]}"#.to_owned(), U),
        ("unknown field in json_schema", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"response_format":{"type":"json_schema","json_schema":{"name":"n","schema":{"type":"object"},"extra":1}}}"#.to_owned(), U),
        ("tool_choice naming an undeclared tool", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"function","function":{"name":"g"}}}"#.to_owned(), U),
        ("tool_choice without tools", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tool_choice":"auto"}"#.to_owned(), U),
        ("parallel_tool_calls without tools", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"parallel_tool_calls":true}"#.to_owned(), U),
        ("duplicate tool names", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f"}},{"type":"function","function":{"name":"f"}}]}"#.to_owned(), U),
        ("json_schema type with no body", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"response_format":{"type":"json_schema"}}"#.to_owned(), U),
        ("json_object type with a body", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"response_format":{"type":"json_object","json_schema":{"name":"n","schema":{"type":"object"}}}}"#.to_owned(), U),
        ("duplicate key in a schema object", tool_body(r#"{"type":"object","type":"string"}"#), M),
        ("duplicate property name", tool_body(r#"{"type":"object","properties":{"a":{"type":"string"},"a":{"type":"integer"}}}"#), M),
        ("escaped duplicate property name", tool_body(r#"{"type":"object","properties":{"a":{"type":"string"},"a":{"type":"integer"}}}"#), M),
        ("duplicate key in a tool", r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","type":"function","function":{"name":"f"}}]}"#.to_owned(), M),
        ("schema too deep", tool_body(&format!(r#"{{"type":"object","properties":{{"a":{}}}}}"#, nested_items(7))), L),
        ("too many schema objects", many_objects, L),
        ("oversized description", tool_body(&oversized), L),
    ]
}

#[tokio::test]
async fn unsupported_malformed_and_oversized_forms_are_rejected_before_inspection() {
    let lab = Lab::new(&full());
    for (name, body, want) in rejected_cases() {
        assert_eq!(lab.validate(&body).await.err(), Some(want), "{name}");
    }
}

// ------------------------------------------------------- SDK-generated schema shapes

/// Keywords from `rejected` that appear anywhere in `value`, as object keys.
fn present_keys(value: &Value, rejected: &[&'static str], found: &mut BTreeSet<&'static str>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if let Some(hit) = rejected.iter().find(|r| **r == k) {
                    found.insert(hit);
                }
                present_keys(v, rejected, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| present_keys(v, rejected, found)),
        _ => {}
    }
}

fn strip(value: &mut Value, rejected: &[&str]) {
    match value {
        Value::Object(map) => {
            map.retain(|k, _| !rejected.contains(&k.as_str()));
            map.values_mut().for_each(|v| strip(v, rejected));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| strip(v, rejected)),
        _ => {}
    }
}

/// Documents what real SDK helpers emit and which of those keywords the contract rejects.
/// #57 relaxed only `title` (ADR 0029): the assertions pin that the strict-converted Python
/// shape now passes as emitted and that every other fixture is rejected only because of the
/// listed keywords.
///
/// The fixtures are written by hand to match the helpers' output shapes: Pydantic's
/// `model_json_schema()` (titles, defaults, `$defs`/`$ref` for nested models), the OpenAI
/// Python SDK's strict conversion (`to_strict_json_schema`: titles kept, `$ref` inlined,
/// `additionalProperties: false`, every property required, optional fields as `anyOf` with
/// `null`), and `zod-to-json-schema` as used by `zodResponseFormat` (draft-07 `$schema`,
/// `format`, `pattern`, `additionalProperties: false`).
#[tokio::test]
async fn sdk_generated_schema_shapes_and_the_keywords_rejected_today() {
    const CANDIDATES: [&str; 7] = [
        "default", "$schema", "$defs", "$ref", "format", "pattern", "examples",
    ];
    let pydantic_raw = json!({
        "$defs": {"Address": {"properties": {"city": {"title": "City", "type": "string"}},
                              "required": ["city"], "title": "Address", "type": "object"}},
        "properties": {
            "name": {"title": "Name", "type": "string"},
            "unit": {"default": "c", "enum": ["c", "f"], "title": "Unit", "type": "string"},
            "days": {"default": 3, "title": "Days", "type": "integer"},
            "address": {"$ref": "#/$defs/Address"},
            "nick": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "Nick"}
        },
        "required": ["name", "address"],
        "title": "Weather",
        "type": "object"
    });
    let openai_strict = json!({
        "properties": {
            "name": {"title": "Name", "type": "string"},
            "unit": {"enum": ["c", "f"], "title": "Unit", "type": "string"},
            "nick": {"anyOf": [{"type": "string"}, {"type": "null"}], "title": "Nick"}
        },
        "required": ["name", "unit", "nick"],
        "title": "Weather",
        "type": "object",
        "additionalProperties": false
    });
    let zod = json!({
        "type": "object",
        "properties": {
            "email": {"type": "string", "format": "email"},
            "code": {"type": "string", "pattern": "^[A-Z]{3}$", "minLength": 3, "maxLength": 3},
            "age": {"type": "integer", "minimum": 0, "maximum": 150, "description": "Age."},
            "tags": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["email", "code", "age", "tags"],
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema#"
    });
    // A hand-written schema using only the approved keywords for contrast: accepted as is.
    let approved = json!({
        "type": "object",
        "properties": {"name": {"type": "string", "description": "Name."},
                       "nick": {"anyOf": [{"type": "string"}, {"type": "null"}]}},
        "required": ["name", "nick"], "additionalProperties": false
    });
    let lab = Lab::new(&full());
    let body = |schema: &Value| {
        shell(json!({"tools": [{"type": "function", "function": {
            "name": "f", "parameters": schema, "strict": true}}]}))
        .to_string()
    };
    let expected: [(&str, &Value, &[&str]); 3] = [
        (
            "pydantic model_json_schema",
            &pydantic_raw,
            &["$defs", "$ref", "default"],
        ),
        ("openai python to_strict_json_schema", &openai_strict, &[]),
        (
            "zod-to-json-schema",
            &zod,
            &["$schema", "format", "pattern"],
        ),
    ];
    for (name, fixture, keywords) in expected {
        // Since #57 `title` is accepted text, so the strict conversion passes as emitted.
        let blocked = !keywords.is_empty();
        assert_eq!(
            lab.validate(&body(fixture)).await.err(),
            blocked.then_some(ProtocolError::Unsupported),
            "{name}"
        );
        let mut found = BTreeSet::new();
        present_keys(fixture, &CANDIDATES, &mut found);
        let found: Vec<&str> = found.into_iter().collect();
        let mut want = keywords.to_vec();
        want.sort_unstable();
        assert_eq!(found, want, "{name}");
        // Only those keywords stand in the way (title, default, ... removed: accepted).
        let mut stripped = fixture.clone();
        strip(&mut stripped, &CANDIDATES);
        assert!(lab.validate(&body(&stripped)).await.is_ok(), "{name}");
    }
    assert!(lab.validate(&body(&approved)).await.is_ok());
}

// ------------------------------------------------------------ over HTTP: zero upstream

struct Gateway {
    addr: SocketAddr,
    chat: Arc<ChatRoute>,
    upstream: FakeUpstream,
    markers: Markers,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), StartupError>>,
}

impl Gateway {
    async fn start() -> Self {
        let json = r#"{"schema_version":1,
            "deployment":{"listener":{"address":"127.0.0.1:0"}},
            "content":{"profile":"full"},
            "resources":{"capacity":{"receipt":4,"memory_units":8192,
                "inspection":2,"upstream":1,"stream":1}}}"#;
        let plan = Arc::new(config::parse(json.as_bytes()).unwrap());
        let bound = server::bind(plan, Services::init).await.unwrap();
        let addr = bound.local_addr().unwrap();
        let chat = bound.chat();
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(bound.serve(async move {
            let _ = rx.await;
        }));
        Self {
            addr,
            chat,
            upstream: FakeUpstream::start(Behavior::ok_json()).await,
            markers: Markers::standard(),
            stop: Some(stop),
            task,
        }
    }

    async fn post_json(&self, body: &str) -> Response {
        let request = post(
            CHAT_COMPLETIONS_PATH,
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        );
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        stream.write_all(&request).await.unwrap();
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0) | Err(_)) => break,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                Err(_) => panic!("timed out waiting for a response"),
            }
        }
        self.markers.assert_clean("response", &out);
        assert!(!String::from_utf8_lossy(&out).contains("SYNTHETICREVOKED"));
        parse_response(&out).expect("a complete HTTP response")
    }

    async fn assert_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let adm = self.chat.admission();
            let memory = adm.try_reserve_memory(8192);
            let permits: Vec<_> = (0..2).map(|_| adm.try_inspection()).collect();
            let receipts: Vec<_> = (0..4).map(|_| adm.try_receipt()).collect();
            if memory.is_ok()
                && permits.iter().all(Result::is_ok)
                && receipts.iter().all(Result::is_ok)
            {
                break;
            }
            assert!(Instant::now() < deadline, "capacity was not returned");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // No request reaches the upstream, whatever its outcome.
        self.upstream.assert_nothing_sent();
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.task).await;
    }
}

fn expect(r: &Response, status: u16, code: &str) {
    let body = String::from_utf8(r.body.clone()).unwrap();
    assert_eq!(r.status, status, "{body}");
    assert_eq!(body, format!(r#"{{"error":{{"code":"{code}"}}}}"#));
}

#[tokio::test]
async fn every_rejection_sends_zero_upstream_bytes_with_a_fixed_safe_code() {
    let gw = Gateway::start().await;
    for (name, body, want) in rejected_cases() {
        let r = gw.post_json(&body).await;
        match want {
            ProtocolError::Unsupported => expect(&r, 422, "unsupported_input"),
            ProtocolError::Malformed => expect(&r, 400, "malformed_input"),
            ProtocolError::LimitExceeded => expect(&r, 413, "limit_exceeded"),
            other => panic!("unexpected {other:?}"),
        }
        gw.assert_idle().await;
        let _ = name;
    }
    // A secret in a label blocks with the same fixed code and still sends nothing.
    let blocked = shell(json!({"tools": [{"type": "function", "function": {
        "name": "f", "parameters": {"type": "object",
            "properties": {token(3): {"type": "string"}}}}}]}));
    expect(
        &gw.post_json(&blocked.to_string()).await,
        422,
        "unsupported_input",
    );
    gw.assert_idle().await;
    // A secret in a description is redacted and the request is approved (the sealed body is
    // dropped locally in this build, so the caller sees the fixed local answer).
    let redacted = shell(json!({"tools": [{"type": "function", "function": {
        "name": "f", "description": format!("d {}", token(4))}}]}));
    expect(
        &gw.post_json(&redacted.to_string()).await,
        501,
        "not_implemented",
    );
    gw.assert_idle().await;
    gw.shutdown().await;
}
