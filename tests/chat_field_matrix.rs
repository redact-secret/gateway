//! Matrix-driven contract tests for the Chat Completions request field matrix (#52).
//!
//! One table, one row per field class: a synthetic example, the class it belongs to, the
//! outcome today, and (for planned classes) the issue that flips the row. The normative
//! tables are `docs/contracts/chat-completions-request.md` and
//! `docs/contracts/field-classification.md`; ADR 0025 assigns each planned row to an owner.
//!
//! How a follow-up flips its rows (#53 tool history, #54 tools and schemas, #55 metadata):
//!
//! 1. Set the row's `implemented` to `true` (the row then asserts `target`, which was
//!    written from the contract) and fix `target` if the contract's slot layout was refined.
//! 2. Delete the `#[ignore]` from your own `target_*` test, or delete the test: once every
//!    row of an owner is implemented, `planned_rows_are_rejected_until_their_issue_lands`
//!    no longer constrains it.
//!
//! Every planned row is asserted rejected until it is flipped, so a newly contracted field
//! can never be accepted by accident, and an unflipped row fails loudly the moment an
//! owner makes it accepted. Rejection happens in `ChatRoute::admit`, before any
//! `SanitizedRequest` can exist, so a rejected row reaches no upstream by construction; the
//! over-HTTP zero-upstream-bytes evidence is `tests/chat_admission.rs`. All data here is
//! synthetic.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::num::NonZeroU32;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use redact_secret_gateway::admission::{Admission, CapacityPlan, RequestLimits};
use redact_secret_gateway::chat_route::{CHAT_COMPLETIONS_PATH, ChatRoute, Reject};
use redact_secret_gateway::config::RouteId;
use redact_secret_gateway::protocol::chat::{SlotMode, TextSlot};
use redact_secret_gateway::transport::destination::OPENAI_CHAT_COMPLETIONS_ROUTE;

/// How a field is classified by the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// Decoded string sent through the core (redact).
    Text,
    /// Structural label: identifier, key, or enum value; any finding blocks (detect-only).
    Label,
    /// Validated structural/control data, transmitted as is.
    Structural,
    /// Not allowed.
    Rejected,
}

#[derive(Clone, Debug)]
enum Expect {
    /// Admitted; the inspected texts are exactly these, in this order.
    Accepted(Vec<(TextSlot, &'static str)>),
    /// Rejected as `unsupported_input`.
    Unsupported,
    /// Rejected as `malformed_input` (400): syntax, duplicate keys, malformed arguments.
    Malformed,
}

struct Row {
    /// Field class in the contract.
    class: Class,
    /// Contract location, for the failure message.
    field: &'static str,
    body: &'static str,
    /// The outcome the contract specifies.
    target: Expect,
    /// `None`: the row holds today. `Some("#53")`: planned, rejected until that issue lands.
    owner: Option<&'static str>,
    /// Set to `true` by the owner when the feature lands.
    implemented: bool,
}

const fn msg(index: usize) -> TextSlot {
    TextSlot::Message { index, part: None }
}

const ACC: fn(Vec<(TextSlot, &'static str)>) -> Expect = Expect::Accepted;

#[rustfmt::skip]
fn rows() -> Vec<Row> {
    let r = |class, field, body, target, owner| Row { class, field, body, target, owner, implemented: owner.is_none() };
    vec![
        // ---- Alpha 1, implemented (#18, #19) -------------------------------------------
        r(Class::Text, "messages[].content (string)",
          r#"{"model":"m","messages":[{"role":"user","content":"SYNTHETIC text"}]}"#,
          ACC(vec![(msg(0), "SYNTHETIC text")]), None),
        r(Class::Text, "messages[].content[] (text parts)",
          r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}"#,
          ACC(vec![(TextSlot::Message { index: 0, part: Some(0) }, "a"), (TextSlot::Message { index: 0, part: Some(1) }, "b")]), None),
        r(Class::Structural, "messages[].role (system, developer, user, assistant)",
          r#"{"model":"m","messages":[{"role":"system","content":"s"},{"role":"developer","content":"d"},{"role":"assistant","content":"a"}]}"#,
          ACC(vec![(msg(0), "s"), (msg(1), "d"), (msg(2), "a")]), None),
        r(Class::Text, "stop (string)",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stop":"END"}"#,
          ACC(vec![(msg(0), "x"), (TextSlot::Stop { index: 0 }, "END")]), None),
        r(Class::Text, "stop (array of 1 to 4)",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stop":["a","b"]}"#,
          ACC(vec![(msg(0), "x"), (TextSlot::Stop { index: 0 }, "a"), (TextSlot::Stop { index: 1 }, "b")]), None),
        r(Class::Text, "user",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"user":"u-1"}"#,
          ACC(vec![(msg(0), "x"), (TextSlot::User, "u-1")]), None),
        r(Class::Structural, "model, stream, stream_options.include_usage, sampling controls, n=1, seed, token limits, response_format text and json_object",
          r#"{"model":"ft:gpt-4o:org:n:id","messages":[{"role":"user","content":"x"}],"stream":true,"stream_options":{"include_usage":true},"temperature":0.5,"top_p":1,"presence_penalty":-2,"frequency_penalty":2,"max_tokens":8,"max_completion_tokens":9,"n":1,"seed":-1,"response_format":{"type":"json_object"}}"#,
          ACC(vec![(msg(0), "x")]), None),
        r(Class::Rejected, "messages[].name",
          r#"{"model":"m","messages":[{"role":"user","name":"n","content":"x"}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "messages[].refusal and audio",
          r#"{"model":"m","messages":[{"role":"assistant","refusal":null,"content":"x"}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "messages[].content null without tool_calls",
          r#"{"model":"m","messages":[{"role":"assistant","content":null}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "content part image_url",
          r#"{"model":"m","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/a.png"}}]}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "content part input_audio, file, refusal",
          r#"{"model":"m","messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"f"}}]}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "messages[].role function (legacy)",
          r#"{"model":"m","messages":[{"role":"function","name":"f","content":"x"}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "functions (legacy)",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"functions":[]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "function_call (legacy)",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"function_call":"auto"}"#, Expect::Unsupported, None),
        r(Class::Rejected, "logit_bias",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"logit_bias":{"50256":-100}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "prediction",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"prediction":{"type":"content","content":"x"}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "modalities and audio",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"modalities":["text","audio"],"audio":{"voice":"v","format":"wav"}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "store, service_tier, reasoning_effort, logprobs, top_logprobs, web_search_options",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"store":true}"#, Expect::Unsupported, None),
        r(Class::Rejected, "logprobs",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"logprobs":true}"#, Expect::Unsupported, None),
        r(Class::Rejected, "n other than 1",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"n":2}"#, Expect::Unsupported, None),
        r(Class::Rejected, "unknown top-level field",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"SYNTHETIC_UNKNOWN":1}"#, Expect::Unsupported, None),
        r(Class::Rejected, "unknown nested field in stream_options and response_format",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"stream_options":{"include_usage":true,"x":1}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "client claim that input is already scanned",
          r#"{"model":"m","messages":[{"role":"user","content":"x"}],"redacted":true}"#, Expect::Unsupported, None),

        // ---- Planned for #53: tool history ---------------------------------------------
        r(Class::Label, "assistant tool_calls with null content and a tool result (arguments decoded, leaves inspected)",
          r#"{"model":"m","messages":[{"role":"user","content":"q"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"서울\",\"n\":3}"}}]},{"role":"tool","tool_call_id":"call_1","content":"sunny"}]}"#,
          ACC(vec![
              (msg(0), "q"),
              (TextSlot::ToolCallId { message: 1, call: 0 }, "call_1"),
              (TextSlot::ToolCallName { message: 1, call: 0 }, "get_weather"),
              (TextSlot::ToolCallArgumentKey { message: 1, call: 0, leaf: 0 }, "city"),
              (TextSlot::ToolCallArgumentText { message: 1, call: 0, leaf: 1 }, "서울"),
              (TextSlot::ToolCallArgumentKey { message: 1, call: 0, leaf: 2 }, "n"),
              (TextSlot::ToolResultId { message: 2 }, "call_1"),
              (msg(2), "sunny"),
          ]), None),
        r(Class::Label, "assistant content string beside tool_calls; two calls; tool result as text parts",
          r#"{"model":"m","messages":[{"role":"assistant","content":"calling","tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{}"}},{"id":"b","type":"function","function":{"name":"g","arguments":"{}"}}]},{"role":"tool","tool_call_id":"a","content":[{"type":"text","text":"r1"}]},{"role":"tool","tool_call_id":"b","content":"r2"}]}"#,
          ACC(vec![
              (msg(0), "calling"),
              (TextSlot::ToolCallId { message: 0, call: 0 }, "a"),
              (TextSlot::ToolCallName { message: 0, call: 0 }, "f"),
              (TextSlot::ToolCallId { message: 0, call: 1 }, "b"),
              (TextSlot::ToolCallName { message: 0, call: 1 }, "g"),
              (TextSlot::ToolResultId { message: 1 }, "a"),
              (TextSlot::Message { index: 1, part: Some(0) }, "r1"),
              (TextSlot::ToolResultId { message: 2 }, "b"),
              (msg(2), "r2"),
          ]), None),
        r(Class::Rejected, "tool result with no matching assistant tool call",
          r#"{"model":"m","messages":[{"role":"user","content":"q"},{"role":"tool","tool_call_id":"call_9","content":"x"}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "tool_calls[].function.arguments that is not JSON",
          r#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"not json"}}]}]}"#, Expect::Malformed, None),
        r(Class::Rejected, "tool_calls[].function.arguments that is JSON but not an object",
          r#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"[1]"}}]}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "tool_calls[].function.arguments with duplicate keys",
          r#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{\"a\":1,\"a\":2}"}}]}]}"#, Expect::Malformed, None),
        r(Class::Rejected, "tool_calls[].type other than function (custom)",
          r#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"c","type":"custom","custom":{"name":"f","input":"x"}}]}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "assistant tool_calls: empty array",
          r#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[]}]}"#, Expect::Unsupported, None),

        // ---- Planned for #54: tools, tool_choice, response_format json_schema ----------
        r(Class::Label, "tools[].function (name label, description text, parameters schema), tool_choice, parallel_tool_calls",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Look up weather.","strict":true,"parameters":{"type":"object","properties":{"city":{"type":"string","description":"City name."}},"required":["city"],"additionalProperties":false}}}],"tool_choice":{"type":"function","function":{"name":"get_weather"}},"parallel_tool_calls":false}"#,
          ACC(vec![
              (msg(0), "q"),
              (TextSlot::ToolDefName { tool: 0 }, "get_weather"),
              (TextSlot::ToolDefDescription { tool: 0 }, "Look up weather."),
              (TextSlot::ToolDefSchemaLabel { tool: 0, leaf: 0 }, "city"),
              (TextSlot::ToolDefSchemaText { tool: 0, leaf: 1 }, "City name."),
              (TextSlot::ToolDefSchemaLabel { tool: 0, leaf: 2 }, "city"),
              (TextSlot::ToolChoiceName, "get_weather"),
          ]), Some("#54")),
        r(Class::Structural, "tool_choice string forms none, auto, required",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"required"}"#,
          ACC(vec![(msg(0), "q"), (TextSlot::ToolDefName { tool: 0 }, "f")]), Some("#54")),
        r(Class::Label, "response_format json_schema (name label, description text, schema, strict)",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"response_format":{"type":"json_schema","json_schema":{"name":"answer","description":"The answer.","strict":true,"schema":{"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}}}}"#,
          ACC(vec![
              (msg(0), "q"),
              (TextSlot::ResponseSchemaName, "answer"),
              (TextSlot::ResponseSchemaDescription, "The answer."),
              (TextSlot::ResponseSchemaLabel { leaf: 0 }, "ok"),
              (TextSlot::ResponseSchemaLabel { leaf: 1 }, "ok"),
          ]), Some("#54")),
        r(Class::Rejected, "tool_choice without tools",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tool_choice":"auto"}"#, Expect::Unsupported, None),
        r(Class::Rejected, "tool_choice naming an undeclared tool",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"function","function":{"name":"g"}}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "schema $ref (never fetched or resolved)",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"a":{"$ref":"https://example.invalid/s.json"}}}}}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "schema default and examples (arbitrary data)",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"a":{"type":"string","default":"SYNTHETIC"}}}}}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "tool name outside the identifier charset",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"function","function":{"name":"bad name!"}}]}"#, Expect::Unsupported, None),
        r(Class::Rejected, "tools[].type other than function",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"tools":[{"type":"custom","custom":{"name":"f"}}]}"#, Expect::Unsupported, None),

        // ---- Planned for #55: metadata -------------------------------------------------
        r(Class::Label, "metadata (key label, string value text)",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"metadata":{"trace_id":"abc","team":"synthetic"}}"#,
          ACC(vec![
              (msg(0), "q"),
              (TextSlot::MetadataKey { entry: 0 }, "trace_id"),
              (TextSlot::MetadataValue { entry: 0 }, "abc"),
              (TextSlot::MetadataKey { entry: 1 }, "team"),
              (TextSlot::MetadataValue { entry: 1 }, "synthetic"),
          ]), Some("#55")),
        r(Class::Rejected, "metadata with a non-string value",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"metadata":{"k":1}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "metadata with a nested object",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"metadata":{"k":{"a":"b"}}}"#, Expect::Unsupported, None),
        r(Class::Rejected, "metadata key outside the charset",
          r#"{"model":"m","messages":[{"role":"user","content":"q"}],"metadata":{"bad key":"v"}}"#, Expect::Unsupported, None),
    ]
}

fn route() -> ChatRoute {
    let n = |v| NonZeroU32::new(v).unwrap();
    let admission = Arc::new(Admission::new(&CapacityPlan::new(
        n(4),
        n(8192),
        n(1),
        n(1),
        n(1),
    )));
    ChatRoute::new(
        admission,
        RequestLimits::provisional(),
        RouteId::new(OPENAI_CHAT_COMPLETIONS_ROUTE),
    )
}

fn request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(CHAT_COMPLETIONS_PATH)
        .header("content-type", "application/json")
        .header(
            "authorization",
            "Bearer sk-SYNTHETIC-REVOKED-MATRIX-NOT-A-KEY",
        )
        .header("content-length", body.len().to_string())
        .body(Body::from(body.to_owned()))
        .unwrap()
}

/// Run the admission road and return the visited texts, or the typed rejection.
async fn outcome(body: &str) -> Result<Vec<(TextSlot, String)>, Reject> {
    let route = route();
    let admitted = route.admit(request(body)).await?;
    let mut out = Vec::new();
    admitted
        .chat()
        .for_each_text(|slot, text| out.push((slot, text.to_owned())));
    Ok(out)
}

async fn assert_expect(row: &Row, expect: Expect) {
    let got = outcome(row.body).await;
    match (expect, got) {
        (Expect::Accepted(want), Ok(got)) => {
            let want: Vec<(TextSlot, String)> =
                want.iter().map(|(s, t)| (*s, (*t).to_owned())).collect();
            assert_eq!(got, want, "texts for: {}", row.field);
        }
        (Expect::Unsupported, Err(Reject::Unsupported))
        | (Expect::Malformed, Err(Reject::Malformed)) => {}
        (expect, got) => panic!(
            "row `{}`: expected {expect:?}, got {:?}",
            row.field,
            got.map(|v| v.len())
        ),
    }
}

/// Rows that hold today: accepted forms produce exactly their texts, rejected forms are
/// `unsupported_input`. Planned rows must still be rejected.
#[tokio::test]
async fn every_row_matches_the_implemented_contract_and_planned_rows_stay_rejected() {
    for row in rows() {
        if row.implemented {
            assert_expect(&row, row.target.clone()).await;
        } else {
            assert_expect(&row, Expect::Unsupported).await;
        }
    }
}

/// Every accepted text class has a stable mode and is represented by an accepted row.
#[test]
fn table_is_well_formed() {
    let rows = rows();
    assert!(rows.iter().any(|r| r.class == Class::Text));
    assert!(rows.iter().any(|r| r.class == Class::Structural));
    assert!(rows.iter().any(|r| r.class == Class::Label));
    assert!(rows.iter().any(|r| r.class == Class::Rejected));
    for row in &rows {
        // A rejected row targets rejection; a planned owner is one of the three field tasks.
        if row.class == Class::Rejected {
            assert!(
                matches!(row.target, Expect::Unsupported | Expect::Malformed),
                "{}",
                row.field
            );
        }
        if let Some(owner) = row.owner {
            assert!(["#53", "#54", "#55"].contains(&owner), "{}", row.field);
            assert!(matches!(row.target, Expect::Accepted(_)), "{}", row.field);
        }
    }
    // Detect-only slots are exactly the label classes.
    for row in &rows {
        if let Expect::Accepted(texts) = &row.target {
            for (slot, _) in texts {
                if slot.mode() == SlotMode::DetectOnly {
                    assert!(
                        row.class == Class::Label || row.owner.is_some(),
                        "{}",
                        row.field
                    );
                }
            }
        }
    }
}

async fn check_target(owner: &str) {
    for row in rows().iter().filter(|r| r.owner == Some(owner)) {
        assert_expect(row, row.target.clone()).await;
    }
}

#[tokio::test]
#[ignore = "rejected-until-#54: remove this attribute when tool definitions and schemas land"]
async fn target_rows_for_54_tool_definitions_and_schemas() {
    check_target("#54").await;
}

#[tokio::test]
#[ignore = "rejected-until-#55: remove this attribute when metadata lands"]
async fn target_rows_for_55_metadata() {
    check_target("#55").await;
}
