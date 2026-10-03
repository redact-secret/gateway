//! App-submitted tool history through the whole chat route (#53; ADR 0025, field contract).
//! Compiled only under `cfg(test)`.
//!
//! Admission, strict parse, tool-history validation, real pinned-core inspection (redact
//! slots) and label scans (detect-only slots), revalidation, bounded serialization and
//! forwarding all run against the loopback fake provider. The fake records exactly the
//! bytes that reach it, so these tests assert on the provider's view: valid JSON, redacted
//! text, preserved structure, and zero bytes for every rejected shape. Every secret is a
//! synthetic revoked-style token.

use serde_json::{Value, json};

use super::forward_tests::{Caps, KEY, Rig, assert_gateway_error, request};
use super::*;

fn token(n: u32) -> String {
    format!("ghp_SYNTHETICREVOKED{n:020}")
}

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments}})
}

fn assistant(calls: &[Value]) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": calls})
}

fn tool(id: &str, content: &Value) -> Value {
    json!({"role": "tool", "tool_call_id": id, "content": content})
}

fn body(messages: &[Value]) -> String {
    json!({"model": "gpt-4o-mini", "messages": messages}).to_string()
}

/// The only upstream call, parsed. Fails if the provider saw anything but one valid JSON body.
fn forwarded(rig: &Rig) -> (Vec<u8>, Value) {
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 1, "exactly one upstream request");
    let raw = calls[0].body.clone();
    let parsed: Value = serde_json::from_slice(&raw).expect("provider JSON");
    (raw, parsed)
}

fn arguments_of(message: &Value, call: usize) -> Value {
    let text = message["tool_calls"][call]["function"]["arguments"]
        .as_str()
        .expect("arguments is a JSON string");
    serde_json::from_str(text).expect("arguments is valid JSON")
}

#[tokio::test]
async fn secrets_in_argument_values_and_tool_results_are_redacted_before_the_provider() {
    let rig = Rig::new(Behavior::ok_json()).await;
    // Call 1 hides the token behind a \u escape inside the argument document itself.
    let escaped = format!(
        r#"{{"note":"g{}","city":"서울","days":3,"opts":{{"flags":[true,null,1.5,"clean"]}}}}"#,
        &token(1)[1..]
    );
    let plain = format!(r#"{{"query":"비밀번호 {} 입니다","n":[1,2]}}"#, token(2));
    let messages = [
        json!({"role": "user", "content": "weather and lookup"}),
        assistant(&[
            call("call_1", "get_weather", &escaped),
            call("call_2", "lookup", &plain),
        ]),
        tool("call_2", &json!(format!("result with {}", token(3)))),
        tool(
            "call_1",
            &json!([{"type": "text", "text": format!("결과 {} 끝", token(4))}]),
        ),
    ];
    let out = rig.post(request(&body(&messages), KEY, &[])).await;
    assert_eq!(out.status, 200);
    let (raw, parsed) = forwarded(&rig);
    let text = String::from_utf8(raw).unwrap();
    assert!(!text.contains("ghp_"), "a secret reached the provider");
    assert!(!text.contains("SYNTHETICREVOKED"));
    assert!(!text.contains("u0067"), "no escaped form survives either");

    let msgs = parsed["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[1]["role"], "assistant");
    assert!(msgs[1]["content"].is_null());
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[1]["tool_calls"][0]["type"], "function");
    assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
    assert_eq!(msgs[1]["tool_calls"][1]["id"], "call_2");
    // Arguments are valid JSON with the same keys, types and structure, text redacted.
    let a0 = arguments_of(&msgs[1], 0);
    assert_eq!(a0["note"], "<SECRET_1>");
    assert_eq!(a0["city"], "서울");
    assert_eq!(a0["days"], 3);
    assert_eq!(a0["opts"]["flags"], json!([true, null, 1.5, "clean"]));
    let a1 = arguments_of(&msgs[1], 1);
    assert_eq!(a1["query"], "비밀번호 <SECRET_2> 입니다");
    assert_eq!(a1["n"], json!([1, 2]));
    // Tool results are ordinary text, in the order the caller sent them.
    assert_eq!(msgs[2]["tool_call_id"], "call_2");
    assert_eq!(msgs[2]["content"], "result with <SECRET_3>");
    assert_eq!(msgs[3]["tool_call_id"], "call_1");
    assert_eq!(msgs[3]["content"][0]["text"], "결과 <SECRET_4> 끝");
    rig.settle().await;
}

#[tokio::test]
async fn clean_tool_history_is_forwarded_with_compact_equivalent_arguments() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let messages = [
        assistant(&[call(
            "a",
            "f",
            r#" { "x" : 1 , "y" : [ "p" , { "z" : null } ] } "#,
        )]),
        // A result that merely looks like JSON (or like a secret-free document) stays text.
        tool("a", &json!(r#"{"not":"parsed","dup":1,"dup":2}"#)),
    ];
    let out = rig.post(request(&body(&messages), KEY, &[])).await;
    assert_eq!(out.status, 200);
    let (_, parsed) = forwarded(&rig);
    assert_eq!(
        parsed["messages"][0]["tool_calls"][0]["function"]["arguments"],
        r#"{"x":1,"y":["p",{"z":null}]}"#
    );
    assert_eq!(
        parsed["messages"][1]["content"],
        r#"{"not":"parsed","dup":1,"dup":2}"#
    );
    rig.settle().await;
}

#[tokio::test]
async fn a_finding_in_any_label_position_blocks_and_sends_nothing() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let t = token(7);
    let key_in_args = format!(r#"{{"{t}":"v"}}"#);
    let cases: Vec<(&str, String)> = vec![
        (
            "argument key",
            body(&[assistant(&[call("a", "f", &key_in_args)])]),
        ),
        ("call id", body(&[assistant(&[call(&t, "f", "{}")])])),
        ("function name", body(&[assistant(&[call("a", &t, "{}")])])),
        (
            "tool_call_id of a result",
            // The label scan walks every slot; the assistant id is clean here, the result
            // id is the secret, so the linkage error and the finding are both fatal.
            body(&[assistant(&[call("a", "f", "{}")]), tool(&t, &json!("r"))]),
        ),
    ];
    for (what, body) in cases {
        let out = rig.post(request(&body, KEY, &[])).await;
        assert_eq!(out.status, 422, "{what}");
        assert_gateway_error(&out, 422, "unsupported_input");
        assert!(!String::from_utf8_lossy(&out.body).contains("ghp_"));
        rig.fake.assert_nothing_sent();
    }
    // Control: the same shapes with clean labels are forwarded.
    let ok = body(&[
        assistant(&[call("a", "f", r#"{"k":"v"}"#)]),
        tool("a", &json!("r")),
    ]);
    assert_eq!(rig.post(request(&ok, KEY, &[])).await.status, 200);
    rig.settle().await;
}

#[tokio::test]
async fn a_label_scan_that_finds_something_never_rewrites_instead() {
    // Detect-only: even a request whose only finding is a clean-looking label that the core
    // flags is blocked, not redacted into a placeholder that would corrupt linkage.
    let rig = Rig::new(Behavior::ok_json()).await;
    let t = token(8);
    let b = body(&[
        assistant(&[call(&t, "f", "{}")]),
        tool(&t, &json!("clean result")),
    ]);
    assert_gateway_error(
        &rig.post(request(&b, KEY, &[])).await,
        422,
        "unsupported_input",
    );
    rig.fake.assert_nothing_sent();
    rig.settle().await;
}

#[tokio::test]
async fn malformed_unsupported_and_over_limit_tool_history_sends_zero_upstream_bytes() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let ok_call = call("a", "f", "{}");
    let deep = format!("{}1{}", r#"{"a":"#.repeat(9), "}".repeat(9));
    let many: Vec<Value> = (0..33).map(|i| call(&format!("c{i}"), "f", "{}")).collect();
    let big_int = r#"{"n":9223372036854775808}"#;
    let cases: Vec<(&str, String, u16, &str)> = vec![
        (
            "duplicate argument keys",
            body(&[assistant(&[call("a", "f", r#"{"k":1,"k":2}"#)])]),
            400,
            "malformed_input",
        ),
        (
            "escaped duplicate argument keys",
            body(&[assistant(&[call("a", "f", r#"{"k":1,"k":2}"#)])]),
            400,
            "malformed_input",
        ),
        (
            "malformed arguments",
            body(&[assistant(&[call("a", "f", r#"{"k":"#)])]),
            400,
            "malformed_input",
        ),
        (
            "empty arguments",
            body(&[assistant(&[call("a", "f", "")])]),
            400,
            "malformed_input",
        ),
        (
            "non-object arguments",
            body(&[assistant(&[call("a", "f", "[1,2]")])]),
            422,
            "unsupported_input",
        ),
        (
            "argument key outside NAME",
            body(&[assistant(&[call("a", "f", r#"{"a b":1}"#)])]),
            422,
            "unsupported_input",
        ),
        (
            "integer beyond i64",
            body(&[assistant(&[call("a", "f", big_int)])]),
            422,
            "unsupported_input",
        ),
        (
            "arguments too deep",
            body(&[assistant(&[call("a", "f", &deep)])]),
            413,
            "limit_exceeded",
        ),
        (
            "too many calls in one message",
            body(&[assistant(&many)]),
            413,
            "limit_exceeded",
        ),
        (
            "null content without calls",
            body(&[json!({"role": "assistant", "content": null})]),
            422,
            "unsupported_input",
        ),
        (
            "null content on a user message",
            body(&[json!({"role": "user", "content": null})]),
            422,
            "unsupported_input",
        ),
        (
            "tool message without tool_call_id",
            body(&[
                assistant(std::slice::from_ref(&ok_call)),
                json!({"role": "tool", "content": "r"}),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "tool message with null content",
            body(&[
                assistant(std::slice::from_ref(&ok_call)),
                json!({"role": "tool", "tool_call_id": "a", "content": null}),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "result for an unknown id",
            body(&[
                assistant(std::slice::from_ref(&ok_call)),
                tool("zz", &json!("r")),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "result with no assistant call",
            body(&[
                json!({"role": "user", "content": "q"}),
                tool("a", &json!("r")),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "result answered twice",
            body(&[
                assistant(std::slice::from_ref(&ok_call)),
                tool("a", &json!("r")),
                tool("a", &json!("r")),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "duplicate call ids across messages",
            body(&[
                assistant(std::slice::from_ref(&ok_call)),
                tool("a", &json!("r")),
                assistant(std::slice::from_ref(&ok_call)),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "unknown field in a call",
            body(&[assistant(&[
                json!({"id": "a", "type": "function", "index": 0,
                "function": {"name": "f", "arguments": "{}"}}),
            ])]),
            422,
            "unsupported_input",
        ),
        (
            "custom tool call",
            body(&[assistant(&[json!({"id": "a", "type": "custom",
                "custom": {"name": "f", "input": "x"}})])]),
            422,
            "unsupported_input",
        ),
        (
            "message name",
            body(&[
                assistant(std::slice::from_ref(&ok_call)),
                json!({"role": "tool", "tool_call_id": "a", "content": "r", "name": "f"}),
            ]),
            422,
            "unsupported_input",
        ),
        (
            "legacy function role",
            body(&[json!({"role": "function", "name": "f", "content": "r"})]),
            422,
            "unsupported_input",
        ),
        (
            "client redaction claim in a call",
            body(&[
                json!({"role": "assistant", "content": null, "tool_calls": [ok_call],
                "redacted": true}),
            ]),
            422,
            "unsupported_input",
        ),
    ];
    for (what, body, status, code) in cases {
        let out = rig.post(request(&body, KEY, &[])).await;
        assert_eq!(out.status, status, "{what}");
        assert_gateway_error(&out, status, code);
        rig.fake.assert_nothing_sent();
    }
    rig.settle().await;
}

#[tokio::test]
async fn forwarded_tool_history_is_deterministic_across_repeats() {
    let rig = Rig::new(Behavior::ok_json()).await;
    let messages = [
        assistant(&[call(
            "a",
            "f",
            &format!(r#"{{"s":"{}","k":[1,2.5,"x"]}}"#, token(9)),
        )]),
        tool("a", &json!(format!("r {}", token(9)))),
    ];
    let b = body(&messages);
    for _ in 0..3 {
        assert_eq!(rig.post(request(&b, KEY, &[])).await.status, 200);
    }
    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].body, calls[1].body);
    assert_eq!(calls[1].body, calls[2].body);
    rig.settle().await;
}

#[tokio::test]
async fn concurrent_and_cancelled_requests_share_no_numbering_or_state() {
    const N: usize = 6;
    let rig = Rig::with(
        Behavior::Slow {
            delay: Duration::from_millis(40),
            then: Box::new(Behavior::ok_json()),
        },
        RequestLimits::provisional(),
        Caps {
            receipt: 8,
            inspection: 8,
            upstream: 8,
            stream: 1,
        },
    )
    .await;
    let make = |i: usize| {
        body(&[
            assistant(&[call(
                "a",
                "f",
                &format!(
                    r#"{{"marker":"payload-{i}","s":"{}"}}"#,
                    token(i as u32 + 20)
                ),
            )]),
            tool("a", &json!(format!("r-{i} {}", token(i as u32 + 40)))),
        ])
    };
    // One request is cancelled mid-flight; it must leave nothing behind.
    let mut fut = Box::pin(rig.route.handle(request(&make(99), KEY, &[])));
    let finished = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(fut.as_mut(), cx).is_ready())
    })
    .await;
    assert!(!finished);
    drop(fut);

    let mut set = tokio::task::JoinSet::new();
    for i in 0..N {
        let route = Arc::clone(&rig.route);
        let req = request(&make(i), KEY, &[]);
        set.spawn(async move { route.handle(req).await.status().as_u16() });
    }
    while let Some(status) = set.join_next().await {
        assert_eq!(status.unwrap(), 200);
    }
    for call in rig.fake.calls() {
        let text = String::from_utf8(call.body.clone()).unwrap();
        if text.contains("payload-99") {
            continue; // the cancelled request may or may not have been forwarded.
        }
        // Per-request numbering: each request has its own <SECRET_1> and <SECRET_2> only.
        assert!(text.contains("<SECRET_1>") && text.contains("<SECRET_2>"));
        assert!(!text.contains("<SECRET_3>"), "numbering crossed requests");
        assert!(!text.contains("ghp_"));
    }
    rig.settle().await;
}
