//! Consistency checks for the frozen Responses request contract (#82, ADR 0031).
//!
//! The contract is `docs/contracts/responses-request.md`. These tests pin the document itself:
//! its synthetic examples are valid JSON, they obey the `store` rule, and every rejected field
//! family the ADR names is spelled out. Behavior tests for each accepted and rejected form are in
//! `responses_text.rs`, `responses_tools.rs`, `responses_route.rs` (#84 to #86) and, through the
//! pinned SDKs, `qualification/responses-cases.json` (#88). All data here is synthetic.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    fs::read_to_string(root().join(path)).unwrap_or_else(|_| panic!("read {path}"))
}

/// Fenced blocks of the form ```` ```json <kind> [name] ````: `(kind, name, body)`.
fn examples(doc: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut lines = doc.lines();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("```json ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let kind = words.next().unwrap_or("").to_owned();
        let name = words.next().unwrap_or("").to_owned();
        let mut body = String::new();
        for l in lines.by_ref() {
            if l.starts_with("```") {
                break;
            }
            body.push_str(l);
        }
        out.push((kind, name, body));
    }
    out
}

fn contract() -> String {
    read("docs/contracts/responses-request.md")
}

#[test]
fn every_example_is_valid_json_and_the_store_rule_holds() {
    let doc = contract();
    let all = examples(&doc);
    let accepted: Vec<_> = all.iter().filter(|e| e.0 == "accepted").collect();
    let rejected: Vec<_> = all.iter().filter(|e| e.0 == "rejected").collect();
    assert!(accepted.len() >= 2, "contract needs positive examples");
    assert!(rejected.len() >= 12, "contract needs negative examples");
    for (kind, name, body) in &all {
        assert!(
            kind == "accepted" || kind == "rejected",
            "unknown example kind {kind}"
        );
        let value: serde_json::Value =
            serde_json::from_str(body).unwrap_or_else(|e| panic!("example {name}: {e}"));
        let store = value.get("store");
        if kind == "accepted" {
            assert_eq!(
                store,
                Some(&serde_json::Value::Bool(false)),
                "every accepted example sends store:false"
            );
        }
    }
    // The three spellings of the omission decision are all shown as rejected.
    for name in ["store-omitted", "store-true", "store-null"] {
        let (_, _, body) = rejected
            .iter()
            .find(|e| e.1 == name)
            .unwrap_or_else(|| panic!("missing rejected example {name}"));
        let v: serde_json::Value = serde_json::from_str(body).expect("json");
        assert_ne!(v.get("store"), Some(&serde_json::Value::Bool(false)));
    }
}

#[test]
fn the_contract_names_every_rejected_state_and_opaque_form() {
    let doc = contract();
    for needle in [
        "previous_response_id",
        "conversation",
        "prompt",
        "background",
        "include",
        "reasoning",
        "item_reference",
        "input_image",
        "input_file",
        "input_audio",
        "web_search",
        "file_search",
        "mcp",
        "encrypted",
        "truncation",
        "service_tier",
        "`store`",
        "does not guarantee zero provider retention",
        "Default-rejected",
    ] {
        assert!(doc.contains(needle), "contract does not mention `{needle}`");
    }
}

#[test]
fn the_adr_the_contract_and_the_indexes_agree() {
    let adr = read("docs/decisions/0031-responses-stateless-text-contract.md");
    assert!(adr.contains("`store` must be present and exactly `false`"));
    assert!(adr.contains("not a retention guarantee"));
    assert!(adr.contains("Implementation status"));
    let contract = contract();
    assert!(contract.contains("0031-responses-stateless-text-contract.md"));
    let adr_index = read("docs/decisions/README.md");
    assert!(adr_index.contains("0031-responses-stateless-text-contract.md"));
    assert!(adr_index.contains("responses-request.md"));
    let contract_index = read("docs/contracts/README.md");
    assert!(contract_index.contains("responses-request"));
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The Responses route exists (#86) and its literal path is spelled in exactly two places:
/// the inbound path constant and the reviewed destination table. Nothing else (no config,
/// header or body code) may carry it, so it cannot be selected by a caller.
#[test]
fn the_responses_path_is_spelled_only_by_the_route_and_the_destination_table() {
    let mut files = Vec::new();
    rust_files(&root().join("src"), &mut files);
    let mut holders: Vec<String> = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).expect("read");
        if text.contains("\"/v1/responses\"") {
            let rel = path.strip_prefix(root()).expect("under root");
            let rel = rel.to_string_lossy().into_owned();
            // In-crate test modules may spell it to assert the wire.
            if !rel.contains("/tests/") {
                holders.push(rel);
            }
        }
    }
    holders.sort();
    assert_eq!(
        holders,
        ["src/chat_route.rs", "src/transport/destination.rs"],
        "only the inbound path constant and the reviewed destination table spell the path"
    );
}
