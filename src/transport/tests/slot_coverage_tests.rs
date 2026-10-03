//! Matrix-driven text-slot coverage (#55; ADR 0025 D9, contract
//! `docs/contracts/chat-completions-request.md`). Compiled only under `cfg(test)`.
//!
//! Each case is a request body that carries a distinct synthetic secret in **every** accepted
//! redactable string of one slot class (message text and parts, `stop`, `user`, metadata
//! values; the tool classes join this table when they land). The whole chat route runs
//! against the loopback fake provider, and the test asserts:
//!
//! 1. the parsed request really exposes every one of those strings as a redactable slot
//!    (the table cannot silently stop covering a class);
//! 2. no secret byte reaches the fake upstream, exactly one placeholder per secret does, and
//!    the forwarded document keeps its structure (parsed, not string-matched);
//! 3. a secret in a detect-only label (a metadata key) blocks the request with zero
//!    upstream bytes;
//! 4. unknown fields added at any depth of any accepted body are rejected with zero upstream
//!    bytes.
//!
//! Every secret is a synthetic revoked-style token; nothing leaves loopback.

use std::sync::Arc;

use redact_secret::Profile;
use serde_json::{Value, json};

use super::forward_tests::{Caps, KEY, Out, Rig, assert_gateway_error, request};
use super::leak::Markers;
use super::*;
use crate::boundary::Inspection;
use crate::chat_route::ChatRoute;
use crate::config::{ContentPolicy, OnWarn};
use crate::protocol::chat::SlotMode;
use crate::protocol::{self, Protocol};
use crate::telemetry::Metrics;

const MEM: u32 = 8192;

fn tok(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}

/// A rig with a chosen content policy and limits over a fresh loopback fake.
async fn rig_with(content: &ContentPolicy, limits: RequestLimits) -> Rig {
    let caps = Caps::ROOMY;
    let fake = FakeUpstream::start(Behavior::ok_json()).await;
    let upstream = http_upstream_with(fake.addr(), limits);
    let metrics = Arc::new(Metrics::new());
    let upstream = Upstream {
        metrics: Some(Arc::clone(&metrics)),
        ..upstream
    };
    let plan = CapacityPlan::new(
        nz(caps.receipt),
        nz(MEM),
        nz(caps.inspection),
        nz(caps.upstream),
        nz(caps.stream),
    );
    let admission = Arc::new(Admission::new(&plan));
    let inspection =
        Arc::new(Inspection::start(Arc::clone(&admission), content, &limits, &plan).unwrap());
    let route = Arc::new(
        ChatRoute::new(Arc::clone(&admission), limits, RouteId::new(ROUTE))
            .with_inspection(Arc::clone(&inspection))
            .with_upstream(Arc::new(upstream))
            .with_metrics(Arc::clone(&metrics)),
    );
    Rig {
        route,
        admission,
        inspection,
        fake,
        metrics,
        caps,
        limits,
    }
}

/// One accepted slot class with a secret in every redactable string.
fn classes() -> Vec<(&'static str, Value)> {
    let m = |extra: Value| {
        let mut base =
            json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "clean"}]});
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    };
    vec![
        (
            "message string content (every role)",
            json!({"model": "gpt-4o-mini", "messages": [
                {"role": "system", "content": format!("s {}", tok(1))},
                {"role": "developer", "content": format!("d {}", tok(2))},
                {"role": "user", "content": format!("u {}", tok(3))},
                {"role": "assistant", "content": format!("a {}", tok(4))}]}),
        ),
        (
            "message text parts",
            json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": [
                {"type": "text", "text": format!("p0 {}", tok(5))},
                {"type": "text", "text": format!("p1 {}", tok(6))}]}]}),
        ),
        ("stop (string)", m(json!({"stop": format!("e {}", tok(7))}))),
        (
            "stop (array)",
            m(
                json!({"stop": [format!("a {}", tok(8)), format!("b {}", tok(9)), format!("c {}", tok(10)), format!("d {}", tok(11))]}),
            ),
        ),
        ("user", m(json!({"user": format!("u {}", tok(12))}))),
        (
            "metadata values",
            m(json!({"metadata": {
                "trace_id": format!("v0 {}", tok(13)),
                "team": format!("한국어 \u{00e9} v1 {} \"q\" \\ \n", tok(14)),
                "empty": "",
                "k.1:a-b_c": tok(15)}})),
        ),
        (
            "metadata at the 16 entry and 512 byte limits",
            m(json!({"metadata": (0..16).map(|i| {
                let secret = tok(100 + i);
                let pad = "x".repeat(512 - secret.len() - 1);
                (format!("k{i}"), Value::String(format!("{secret} {pad}")))
            }).collect::<serde_json::Map<_, _>>()})),
        ),
        (
            "tool call arguments (string leaves) and tool results (string and parts)",
            json!({"model": "gpt-4o-mini", "messages": [
                {"role": "user", "content": "clean"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "lookup",
                        "arguments": format!(r#"{{"q":"{}","n":[1,"{}"],"o":{{"k":"{}"}}}}"#, tok(50), tok(51), tok(52))}},
                    {"id": "call_2", "type": "function", "function": {"name": "other", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": format!("r {}", tok(53))},
                {"role": "tool", "tool_call_id": "call_2",
                    "content": [{"type": "text", "text": format!("r {}", tok(54))}]}]}),
        ),
        (
            "tool and schema descriptions (tools, tool_choice, parameters)",
            json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "clean"}],
                "tools": [{"type": "function", "function": {"name": "lookup",
                    "description": format!("d {}", tok(60)),
                    "parameters": {"type": "object", "properties": {
                        "city": {"type": "string", "description": format!("c {}", tok(61))},
                        "mode": {"type": "string", "enum": ["fast", "slow"]}},
                        "required": ["city"]}}}],
                "tool_choice": {"type": "function", "function": {"name": "lookup"}}}),
        ),
        (
            "structured-output schema description and schema text",
            json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "clean"}],
                "response_format": {"type": "json_schema", "json_schema": {"name": "answer",
                    "description": format!("d {}", tok(62)),
                    "schema": {"type": "object", "properties": {
                        "ok": {"type": "string", "description": format!("s {}", tok(63))}},
                        "required": ["ok"]}}}}),
        ),
        (
            "every accepted class in one request",
            json!({"model": "gpt-4o-mini",
                "messages": [{"role": "user", "content": [{"type": "text", "text": format!("p {}", tok(20))}]},
                             {"role": "assistant", "content": format!("a {}", tok(21)), "tool_calls": [
                                 {"id": "c9", "type": "function", "function": {"name": "f",
                                     "arguments": format!(r#"{{"x":"{}"}}"#, tok(26))}}]},
                             {"role": "tool", "tool_call_id": "c9", "content": format!("t {}", tok(27))}],
                "stop": [format!("s {}", tok(22))],
                "user": format!("u {}", tok(23)),
                "metadata": {"a": format!("m {}", tok(24)), "b": format!("n {}", tok(25))}}),
        ),
    ]
}

/// Slots and secrets of a body as the typed contract sees them.
async fn redact_slot_count(
    rig: &Rig,
    body: &str,
    classes: &mut std::collections::BTreeSet<String>,
) -> usize {
    let received = rig
        .admission
        .begin_body_receipt(body.len(), &rig.limits)
        .await
        .unwrap()
        .complete(body.as_bytes().to_vec())
        .unwrap();
    let validated =
        protocol::validate_with(received, Protocol::ChatCompletionsText, &rig.limits).unwrap();
    let mut redact = 0_usize;
    let mut with_secret = 0_usize;
    validated.chat().for_each_text(|slot, text| {
        if slot.mode() == SlotMode::Redact {
            redact += 1;
            if text.contains("SYNTHETICREVOKED") {
                with_secret += 1;
                let shown = format!("{slot:?}");
                let name = shown.split([' ', '{']).next().unwrap_or_default();
                classes.insert(name.to_owned());
            }
        } else {
            assert!(
                !text.contains("SYNTHETICREVOKED"),
                "labels carry no secret here"
            );
        }
    });
    // Every redactable string of the class carries a secret, except deliberately clean ones.
    assert!(with_secret > 0 && with_secret <= redact);
    with_secret
}

/// Walk every JSON object (not into string contents) and yield a copy of `body` with an
/// unknown member added to that object.
fn with_unknown_member_everywhere(body: &Value) -> Vec<Value> {
    fn paths(v: &Value, here: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        match v {
            Value::Object(map) => {
                out.push(here.clone());
                for (k, child) in map {
                    here.push(k.clone());
                    paths(child, here, out);
                    here.pop();
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    here.push(i.to_string());
                    paths(child, here, out);
                    here.pop();
                }
            }
            _ => {}
        }
    }
    let mut all = Vec::new();
    paths(body, &mut Vec::new(), &mut all);
    all.into_iter()
        .map(|path| {
            let mut copy = body.clone();
            let mut node = &mut copy;
            for step in &path {
                node = match node {
                    Value::Array(items) => items.get_mut(step.parse::<usize>().unwrap()).unwrap(),
                    other => other.get_mut(step.as_str()).unwrap(),
                };
            }
            node.as_object_mut()
                .unwrap()
                .insert("SYNTHETIC_UNKNOWN".to_owned(), json!(1));
            copy
        })
        .collect()
}

#[tokio::test]
async fn a_secret_in_every_accepted_text_slot_class_never_reaches_the_upstream() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    let markers = Markers::standard();
    let mut expected_calls = 0_usize;
    let mut seen = std::collections::BTreeSet::new();
    for (name, body) in classes() {
        let text = body.to_string();
        let secrets = redact_slot_count(&rig, &text, &mut seen).await;
        let out = rig.post(request(&text, KEY, &[])).await;
        assert_eq!(out.status, 200, "{name}");
        markers.assert_clean(name, &out.body);
        expected_calls += 1;
        let calls = rig.fake.calls();
        assert_eq!(calls.len(), expected_calls, "{name}");
        let forwarded = &calls[expected_calls - 1].body;
        let forwarded_text = std::str::from_utf8(forwarded).unwrap();
        assert!(
            !forwarded_text.contains("SYNTHETICREVOKED"),
            "{name}: a secret reached upstream"
        );
        assert!(
            !forwarded_text.contains("ghp_"),
            "{name}: a secret prefix reached upstream"
        );
        assert_eq!(
            forwarded_text.matches("<SECRET_").count(),
            secrets,
            "{name}"
        );
        // Round trip: the forwarded document keeps the structure and the clean text.
        let parsed: Value = serde_json::from_slice(forwarded).unwrap();
        assert_eq!(parsed["model"], body["model"], "{name}");
        for key in ["stop", "user", "metadata"] {
            assert_eq!(
                parsed.get(key).is_some(),
                body.get(key).is_some(),
                "{name}: {key}"
            );
        }
        for key in ["tools", "tool_choice", "response_format"] {
            assert_eq!(
                parsed.get(key).is_some(),
                body.get(key).is_some(),
                "{name}: {key}"
            );
        }
        if let Some(meta) = body.get("metadata") {
            let sent: Vec<&String> = meta.as_object().unwrap().keys().collect();
            let got: Vec<&String> = parsed["metadata"].as_object().unwrap().keys().collect();
            assert_eq!(sent, got, "{name}: keys and their order are unchanged");
            assert!(
                parsed["metadata"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(Value::is_string)
            );
        }
    }
    // The table cannot silently stop covering a redactable slot class: every class of the
    // contract that carries free text has a secret placed in it by some case above.
    for class in [
        "Message",
        "Stop",
        "User",
        "ToolCallArgumentText",
        "ToolDefDescription",
        "ToolDefSchemaText",
        "ResponseSchemaDescription",
        "ResponseSchemaText",
        "MetadataValue",
    ] {
        assert!(seen.contains(class), "no case covers {class}: {seen:?}");
    }
    rig.settle().await;
}

#[tokio::test]
async fn placeholder_numbering_follows_the_traversal_order_with_metadata_after_user() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    // Raw text: the caller's entry order (`b` before `a`) must survive, which a sorted
    // `serde_json::Value` map would hide.
    let body = format!(
        r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"m {}"}}],"metadata":{{"b":"b {}","a":"a {}"}},"user":"u {}","stop":"s {}"}}"#,
        tok(1),
        tok(2),
        tok(3),
        tok(4),
        tok(5)
    );
    assert_eq!(rig.post(request(&body, KEY, &[])).await.status, 200);
    let sent: Value = serde_json::from_slice(&rig.fake.calls()[0].body).unwrap();
    assert_eq!(sent["messages"][0]["content"], "m <SECRET_1>");
    assert_eq!(sent["stop"], "s <SECRET_2>");
    assert_eq!(sent["user"], "u <SECRET_3>");
    // Entries keep the caller's order (`b` before `a`); numbering follows it.
    assert_eq!(sent["metadata"]["b"], "b <SECRET_4>");
    assert_eq!(sent["metadata"]["a"], "a <SECRET_5>");
    let wire = String::from_utf8(rig.fake.calls()[0].body.clone()).unwrap();
    assert!(wire.find(r#""b":"#).unwrap() < wire.find(r#""a":"#).unwrap());
    rig.settle().await;
}

#[tokio::test]
async fn a_secret_in_a_metadata_key_blocks_but_the_charset_already_limits_the_channel() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    // A token-shaped key is inside the LINK charset (letters, digits, `_`), so the label
    // scan is what stops it: the whole request is refused, nothing is rewritten.
    let key = tok(30);
    let body = format!(
        r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"clean"}}],"metadata":{{"{key}":"v"}}}}"#
    );
    let out = rig.post(request(&body, KEY, &[])).await;
    assert_gateway_error(&out, 422, "unsupported_input");
    assert!(!String::from_utf8_lossy(&out.body).contains("SYNTHETICREVOKED"));
    rig.fake.assert_nothing_sent();
    // A clean request with the same shape goes through unchanged, key included.
    let ok = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"clean"}],"metadata":{"trace_id":"v"}}"#;
    assert_eq!(rig.post(request(ok, KEY, &[])).await.status, 200);
    let sent: Value = serde_json::from_slice(&rig.fake.calls()[0].body).unwrap();
    assert_eq!(sent["metadata"], json!({"trace_id": "v"}));
    rig.settle().await;
}

#[tokio::test]
async fn a_finding_in_any_label_position_of_the_other_slot_classes_blocks_with_zero_bytes() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    let t = tok(70);
    let user = json!({"role": "user", "content": "clean"});
    let tool = |name: &str, params: Value| {
        json!({"model": "gpt-4o-mini", "messages": [user.clone()],
            "tools": [{"type": "function", "function": {"name": name, "parameters": params}}]})
    };
    let cases = [
        (
            "tool name",
            tool(&t, json!({"type": "object", "properties": {}})),
        ),
        (
            "schema property key",
            tool(
                "f",
                json!({"type": "object", "properties": {t.clone(): {"type": "string"}}}),
            ),
        ),
        (
            "schema required entry",
            tool(
                "f",
                json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": [t.clone()]}),
            ),
        ),
        (
            "schema enum string",
            tool(
                "f",
                json!({"type": "object", "properties": {"a": {"type": "string", "enum": [t.clone()]}}}),
            ),
        ),
        (
            "response schema name",
            json!({"model": "gpt-4o-mini", "messages": [user.clone()],
                "response_format": {"type": "json_schema", "json_schema": {"name": t.clone(),
                    "schema": {"type": "object", "properties": {}}}}}),
        ),
        (
            "tool call id",
            json!({"model": "gpt-4o-mini", "messages": [user.clone(),
                {"role": "assistant", "content": null, "tool_calls": [{"id": t.clone(), "type": "function",
                    "function": {"name": "f", "arguments": "{}"}}]}]}),
        ),
        (
            "tool call argument key",
            json!({"model": "gpt-4o-mini", "messages": [user.clone(),
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "f", "arguments": format!(r#"{{"{t}":1}}"#)}}]}]}),
        ),
    ];
    for (name, body) in cases {
        let out = rig.post(request(&body.to_string(), KEY, &[])).await;
        assert_gateway_error(&out, 422, "unsupported_input");
        assert!(
            !String::from_utf8_lossy(&out.body).contains("SYNTHETICREVOKED"),
            "{name}"
        );
        rig.fake.assert_nothing_sent();
    }
    rig.settle().await;
}

fn rejected_shapes() -> Vec<(&'static str, String)> {
    let shell = |metadata: &str| {
        format!(
            r#"{{"model":"gpt-4o-mini","messages":[{{"role":"user","content":"x"}}],"metadata":{metadata}}}"#
        )
    };
    let many = |n: usize| {
        let items: Vec<String> = (0..n).map(|i| format!(r#""k{i}":"v""#)).collect();
        shell(&format!("{{{}}}", items.join(",")))
    };
    vec![
        ("17th entry", many(17)),
        (
            "513 byte value",
            shell(&format!(r#"{{"k":"{}"}}"#, "v".repeat(513))),
        ),
        (
            "65 byte key",
            shell(&format!(r#"{{"{}":"v"}}"#, "k".repeat(65))),
        ),
        ("empty key", shell(r#"{"":"v"}"#)),
        ("space in key", shell(r#"{"a b":"v"}"#)),
        ("escaped control in key", shell(r#"{"a\u0000b":"v"}"#)),
        ("korean key", shell(r#"{"키":"v"}"#)),
        ("duplicate key", shell(r#"{"k":"a","k":"b"}"#)),
        ("duplicate key via escape", shell(r#"{"k":"a","k":"b"}"#)),
        ("number value", shell(r#"{"k":1}"#)),
        ("boolean value", shell(r#"{"k":true}"#)),
        ("null value", shell(r#"{"k":null}"#)),
        ("array value", shell(r#"{"k":["a"]}"#)),
        ("object value", shell(r#"{"k":{"a":"b"}}"#)),
        ("null metadata", shell("null")),
        ("array metadata", shell(r#"["k","v"]"#)),
        ("string metadata", shell(r#""k=v""#)),
        // A client claim is never read; it is just another unknown/oversized shape.
        (
            "claim as a nested object",
            shell(r#"{"redacted":{"claim":true}}"#),
        ),
    ]
}

#[tokio::test]
async fn unsupported_metadata_shapes_and_limits_forward_zero_bytes() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    for (name, body) in rejected_shapes() {
        let out: Out = rig.post(request(&body, KEY, &[])).await;
        assert!(
            matches!(out.status, 400 | 422),
            "{name}: status {}",
            out.status
        );
        let text = String::from_utf8_lossy(&out.body);
        assert!(text.starts_with(r#"{"error":{"code":""#), "{name}");
        rig.fake.assert_nothing_sent();
    }
    rig.settle().await;
}

#[tokio::test]
async fn unknown_fields_at_every_depth_of_every_accepted_body_are_rejected() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    let mut bodies: Vec<Value> = classes().into_iter().map(|(_, b)| b).collect();
    bodies.push(
        json!({"model": "gpt-4o-mini", "stream": true, "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": [{"type": "text", "text": "x"}]}],
        "response_format": {"type": "json_object"}, "temperature": 0.5}),
    );
    let mut tried = 0_usize;
    for body in &bodies {
        for variant in with_unknown_member_everywhere(body) {
            let out = rig.post(request(&variant.to_string(), KEY, &[])).await;
            // The `metadata` map has free keys by design, but its value `1` is not a string.
            assert!(
                matches!(out.status, 400 | 422),
                "status {} for {variant}",
                out.status
            );
            tried += 1;
        }
    }
    assert!(tried > 20, "the walk covered nested objects: {tried}");
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

#[tokio::test]
async fn a_client_redaction_token_is_plain_text_and_never_restored_or_trusted() {
    let rig = rig_with(
        &ContentPolicy::new(Profile::Full),
        RequestLimits::provisional(),
    )
    .await;
    let body = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "clean"}],
        "metadata": {"already": format!("<SECRET_7> {}", tok(40)), "claim": "claim=ok <SECRET_1>"}})
    .to_string();
    assert_eq!(rig.post(request(&body, KEY, &[])).await.status, 200);
    let sent: Value = serde_json::from_slice(&rig.fake.calls()[0].body).unwrap();
    // The literal is kept as ordinary text, the real secret still gets the next number from
    // the request-wide counter, and nothing is substituted back.
    assert_eq!(sent["metadata"]["already"], "<SECRET_7> <SECRET_1>");
    assert_eq!(sent["metadata"]["claim"], "claim=ok <SECRET_1>");
    rig.settle().await;
}

fn pii_policy() -> ContentPolicy {
    ContentPolicy::new(Profile::Full)
        .with_pii(vec!["pii:family:global:email".into()])
        .with_on_warn(OnWarn::Reject)
}

/// `n` context-gated (labelled) short addresses: the pinned core redacts a bare address
/// only behind a label, and each 6 byte address becomes a longer placeholder.
fn emails(n: usize) -> String {
    "email: a@b.io ".repeat(n)
}

#[tokio::test]
async fn growth_past_a_value_bound_or_the_output_bound_is_refused_with_zero_upstream_bytes() {
    // One value of 500 bytes made of short addresses: every address becomes a longer
    // placeholder, so the redacted value is over 512 bytes. It is never truncated.
    let rig = rig_with(&pii_policy(), RequestLimits::provisional()).await;
    let grows = emails(36);
    assert!(grows.len() <= 512, "the input itself is within the bound");
    let body = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "x"}],
        "metadata": {"k": grows}})
    .to_string();
    let out = rig.post(request(&body, KEY, &[])).await;
    assert_gateway_error(&out, 413, "limit_exceeded");
    rig.fake.assert_nothing_sent();

    // The same shape under the bound forwards, so the refusal above is the bound alone.
    let small = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "x"}],
        "metadata": {"k": emails(5)}})
    .to_string();
    assert_eq!(rig.post(request(&small, KEY, &[])).await.status, 200);
    let sent: Value = serde_json::from_slice(&rig.fake.calls()[0].body).unwrap();
    assert!(!sent["metadata"]["k"].as_str().unwrap().contains("a@b.io"));
    rig.settle().await;

    // Sixteen values that each stay under 512 after growth, in a body whose cap equals its
    // own size: the request-wide output bound refuses the grown document.
    let values: serde_json::Map<String, Value> = (0..16)
        .map(|i| (format!("k{i}"), Value::String(emails(25))))
        .collect();
    let wide = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "x"}],
        "metadata": values})
    .to_string();
    let mut tight = RequestLimits::provisional();
    tight.max_body_bytes = u32::try_from(wide.len()).unwrap();
    let rig = rig_with(&pii_policy(), tight).await;
    let out = rig.post(request(&wide, KEY, &[])).await;
    assert_gateway_error(&out, 413, "limit_exceeded");
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

#[tokio::test]
async fn a_user_value_that_outgrows_its_bound_after_redaction_is_refused() {
    let rig = rig_with(&pii_policy(), RequestLimits::provisional()).await;
    let user = emails(18);
    assert!(user.len() <= 256, "the input itself is within the bound");
    let body = json!({"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "x"}],
        "user": user})
    .to_string();
    let out = rig.post(request(&body, KEY, &[])).await;
    assert_gateway_error(&out, 413, "limit_exceeded");
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}
