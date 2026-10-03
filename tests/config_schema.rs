//! Configuration schema freeze (#62, ADR 0032): the published machine-readable schema
//! `docs/schema/gateway-config.v1.schema.json` is checked against the real loader
//! (`config::parse`/`load_from_path`), the shipped and fixture configs, the loader's source key
//! lists, the provisional limit defaults in code, and `docs/contracts/resource-limits.md`, so the
//! schema cannot drift from the implementation. Synthetic data only; nothing reads a network.
//!
//! The schema validator below is deliberately tiny and strict: it implements only the keywords
//! the schema uses and fails on any other keyword, so an unchecked keyword can never make the
//! schema look stricter than it is.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use redact_secret_gateway::admission::RequestLimits;
use redact_secret_gateway::config::{self, ConfigError, Provider};
use redact_secret_gateway::transport::destination::reviewed_routes;
use redact_secret_gateway::transport::local_auth::{LocalToken, TokenReference};
use serde_json::{Value, json};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|_| panic!("read {rel}"))
}

fn schema() -> Value {
    serde_json::from_str(&read("docs/schema/gateway-config.v1.schema.json")).expect("schema JSON")
}

/// A committed fixture cannot carry a secret or the required file mode, so fixtures resolve
/// their `local_auth.token` reference to this synthetic value instead of touching the real
/// environment or filesystem (that path is covered by `tests/local_auth_startup.rs`).
fn synthetic_token(_: &TokenReference) -> Result<LocalToken, ConfigError> {
    LocalToken::from_bytes(b"SYNTHETIC-local-token-0123456789-abcdefghij")
}

fn load(rel: &str) -> Result<config::RuntimePlan, ConfigError> {
    config::load_from_path_with(&root().join(rel), &synthetic_token)
}

fn parse_value(v: &Value) -> Result<config::RuntimePlan, ConfigError> {
    config::parse_with(
        serde_json::to_string(v).expect("json").as_bytes(),
        &synthetic_token,
    )
}

// ---------------------------------------------------------------------------------------------
// Minimal JSON Schema (2020-12 subset) validator
// ---------------------------------------------------------------------------------------------

const SUPPORTED: &[&str] = &[
    "type",
    "const",
    "enum",
    "required",
    "properties",
    "additionalProperties",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "items",
    "minLength",
    "maxLength",
    "pattern",
    "minProperties",
    "maxProperties",
];
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
];

fn is_annotation(key: &str) -> bool {
    ANNOTATIONS.contains(&key) || key.starts_with("x-")
}

/// Fail on any keyword the validator does not implement. Skips `$defs` and `x-*`
/// extensions.
fn assert_supported(node: &Value, at: &str) {
    let Some(map) = node.as_object() else { return };
    for (k, v) in map {
        if is_annotation(k) || k == "$defs" {
            continue;
        }
        assert!(
            SUPPORTED.contains(&k.as_str()),
            "{at}: unsupported keyword `{k}`"
        );
        match k.as_str() {
            "properties" => {
                for (name, child) in v.as_object().expect("properties object") {
                    assert_supported(child, &format!("{at}.{name}"));
                }
            }
            "items" => assert_supported(v, &format!("{at}[]")),
            _ => {}
        }
    }
}

fn type_ok(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64(),
        _ => panic!("unsupported type {ty}"),
    }
}

/// The only two regular expressions the schema uses, matched by hand so the validator needs
/// no regex dependency and fails loudly on any new pattern.
fn pattern_ok(pattern: &str, s: &str) -> bool {
    match pattern {
        "^/" => s.starts_with('/'),
        "^[A-Z_][A-Z0-9_]{0,63}$" => {
            let b = s.as_bytes();
            (1..=64).contains(&b.len())
                && b.iter().enumerate().all(|(i, c)| {
                    c.is_ascii_uppercase() || *c == b'_' || (i > 0 && c.is_ascii_digit())
                })
        }
        other => panic!("unsupported pattern {other}"),
    }
}

fn int(v: &Value) -> Option<i128> {
    v.as_i64()
        .map(i128::from)
        .or_else(|| v.as_u64().map(i128::from))
}

/// Returns the failure paths; empty means valid.
fn check(node: &Value, v: &Value, at: &str, out: &mut Vec<String>) {
    let Some(map) = node.as_object() else { return };
    if let Some(ty) = map.get("type")
        && !type_ok(ty.as_str().expect("type string"), v)
    {
        out.push(format!("{at}: type"));
        return;
    }
    if let Some(c) = map.get("const")
        && c != v
    {
        out.push(format!("{at}: const"));
    }
    if let Some(e) = map.get("enum")
        && !e.as_array().expect("enum array").contains(v)
    {
        out.push(format!("{at}: enum"));
    }
    if let (Some(lo), Some(n)) = (map.get("minimum"), int(v))
        && n < int(lo).expect("min")
    {
        out.push(format!("{at}: minimum"));
    }
    if let (Some(hi), Some(n)) = (map.get("maximum"), int(v))
        && n > int(hi).expect("max")
    {
        out.push(format!("{at}: maximum"));
    }
    if let Some(s) = v.as_str() {
        let len = s.len() as u64;
        if map
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|m| len < m)
        {
            out.push(format!("{at}: minLength"));
        }
        if map
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|m| len > m)
        {
            out.push(format!("{at}: maxLength"));
        }
        if let Some(p) = map.get("pattern").and_then(Value::as_str)
            && !pattern_ok(p, s)
        {
            out.push(format!("{at}: pattern"));
        }
    }
    if let Some(items) = v.as_array() {
        let len = items.len() as u64;
        if map
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|m| len < m)
        {
            out.push(format!("{at}: minItems"));
        }
        if map
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|m| len > m)
        {
            out.push(format!("{at}: maxItems"));
        }
        if let Some(item_schema) = map.get("items") {
            for (i, item) in items.iter().enumerate() {
                check(item_schema, item, &format!("{at}[{i}]"), out);
            }
        }
    }
    if let Some(obj) = v.as_object() {
        let size = obj.len() as u64;
        if map
            .get("minProperties")
            .and_then(Value::as_u64)
            .is_some_and(|m| size < m)
        {
            out.push(format!("{at}: minProperties"));
        }
        if map
            .get("maxProperties")
            .and_then(Value::as_u64)
            .is_some_and(|m| size > m)
        {
            out.push(format!("{at}: maxProperties"));
        }
        let props = map.get("properties").and_then(Value::as_object);
        if let Some(req) = map.get("required").and_then(Value::as_array) {
            for r in req {
                let r = r.as_str().expect("required name");
                if !obj.contains_key(r) {
                    out.push(format!("{at}.{r}: required"));
                }
            }
        }
        for (k, child) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(s) => check(s, child, &format!("{at}.{k}"), out),
                None if map.get("additionalProperties") == Some(&json!(false)) => {
                    out.push(format!("{at}.{k}: additionalProperties"));
                }
                None => {}
            }
        }
    }
}

fn schema_errors(doc: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(&schema(), doc, "$", &mut out);
    out
}

// ---------------------------------------------------------------------------------------------
// Schema well-formedness and drift against the loader source
// ---------------------------------------------------------------------------------------------

/// Every object the schema describes, as (path, key set), for objects that close their keys.
fn closed_objects(node: &Value, at: &str, out: &mut Vec<(String, BTreeSet<String>)>) {
    let Some(props) = node.get("properties").and_then(Value::as_object) else {
        return;
    };
    assert_eq!(
        node.get("additionalProperties"),
        Some(&json!(false)),
        "{at}: every schema object must set additionalProperties:false"
    );
    out.push((at.to_owned(), props.keys().cloned().collect()));
    for (k, child) in props {
        closed_objects(child, &format!("{at}.{k}"), out);
    }
}

#[test]
fn schema_is_well_formed_closed_and_uses_only_checked_keywords() {
    let s = schema();
    assert_eq!(s["$schema"], "https://json-schema.org/draft/2020-12/schema");
    assert_supported(&s, "$");
    let mut objs = Vec::new();
    closed_objects(&s, "$", &mut objs);
    assert_eq!(
        objs.len(),
        10,
        "root, deployment, listener, upstream, local_auth, token, content, resources, capacity, limits"
    );
    // The schema must be internally valid against its own claims: every default is allowed.
    fn defaults(node: &Value, at: &str) {
        if let Some(props) = node.get("properties").and_then(Value::as_object) {
            for (k, child) in props {
                if let Some(d) = child.get("default") {
                    let mut out = Vec::new();
                    check(child, d, at, &mut out);
                    assert!(
                        out.is_empty(),
                        "{at}.{k}: default violates its own schema: {out:?}"
                    );
                }
                defaults(child, &format!("{at}.{k}"));
            }
        }
    }
    defaults(&s, "$");
}

/// Quoted names from every `.only(&[ ... ])` call in the production part of `src/config.rs`.
fn loader_key_lists() -> Vec<BTreeSet<String>> {
    let src = read("src/config.rs");
    let prod = src.split("#[cfg(test)]\nmod tests").next().expect("prod");
    let mut lists = Vec::new();
    let mut rest = prod;
    while let Some(i) = rest.find(".only(&[") {
        rest = &rest[i + ".only(&[".len()..];
        let end = rest.find("])").expect("only list end");
        let names: BTreeSet<String> = rest[..end]
            .split('"')
            .enumerate()
            .filter(|(n, _)| n % 2 == 1)
            .map(|(_, s)| s.to_owned())
            .collect();
        lists.push(names);
    }
    lists
}

#[test]
fn schema_key_sets_equal_the_loaders_accepted_key_lists() {
    let s = schema();
    let mut objs = Vec::new();
    closed_objects(&s, "$", &mut objs);
    let mut from_schema: Vec<BTreeSet<String>> = objs.into_iter().map(|(_, k)| k).collect();
    let mut from_loader = loader_key_lists();
    from_schema.sort();
    from_loader.sort();
    assert_eq!(
        from_schema, from_loader,
        "the schema's property sets must equal the loader's `only` lists; update both together"
    );
}

#[test]
fn schema_defaults_equal_the_provisional_values_in_code() {
    let s = schema();
    let props = &s["properties"]["resources"]["properties"]["limits"]["properties"];
    let RequestLimits {
        max_body_bytes,
        max_depth,
        max_nodes,
        max_string_bytes,
        max_messages,
        admission_wait_ms,
        admission_queue,
        body_deadline_ms,
        upstream_connect_ms,
        upstream_header_ms,
        upstream_total_ms,
        max_response_header_bytes,
        max_response_body_bytes,
        shutdown_drain_ms,
        stream_idle_ms,
        stream_lifetime_ms,
        stream_write_stall_ms,
        stream_buffer_bytes,
        max_connections,
    } = RequestLimits::provisional();
    let code: BTreeMap<&str, u32> = BTreeMap::from([
        ("max_body_bytes", max_body_bytes),
        ("max_depth", max_depth),
        ("max_nodes", max_nodes),
        ("max_string_bytes", max_string_bytes),
        ("max_messages", max_messages),
        ("admission_wait_ms", admission_wait_ms),
        ("admission_queue", admission_queue),
        ("body_deadline_ms", body_deadline_ms),
        ("upstream_connect_ms", upstream_connect_ms),
        ("upstream_header_ms", upstream_header_ms),
        ("upstream_total_ms", upstream_total_ms),
        ("max_response_header_bytes", max_response_header_bytes),
        ("max_response_body_bytes", max_response_body_bytes),
        ("shutdown_drain_ms", shutdown_drain_ms),
        ("stream_idle_ms", stream_idle_ms),
        ("stream_lifetime_ms", stream_lifetime_ms),
        ("stream_write_stall_ms", stream_write_stall_ms),
        ("stream_buffer_bytes", stream_buffer_bytes),
        ("max_connections", max_connections),
    ]);
    assert_eq!(props.as_object().expect("limits").len(), code.len());
    for (k, v) in &code {
        assert_eq!(props[*k]["default"], json!(v), "schema default for {k}");
    }
    let c = &s["properties"]["content"]["properties"];
    assert_eq!(
        c["max_findings"]["default"],
        json!(config::DEFAULT_MAX_FINDINGS)
    );
    assert_eq!(
        c["max_findings"]["maximum"],
        json!(config::MAX_FINDINGS_CEILING)
    );
    assert_eq!(c["pii"]["maxItems"], json!(config::MAX_PII_SELECTORS));
    assert_eq!(c["on_warn"]["default"], "reject");
}

// ---------------------------------------------------------------------------------------------
// Examples and fixtures: the real loader and the schema agree
// ---------------------------------------------------------------------------------------------

fn json_files(dir: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(root().join(dir))
        .unwrap_or_else(|_| panic!("dir {dir}"))
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json") && n != "index.json")
        .collect();
    v.sort();
    v
}

#[test]
fn every_shipped_example_and_valid_fixture_loads_and_matches_the_schema() {
    let mut files: Vec<String> = vec![
        "examples/config.skeleton.json".into(),
        "examples/config.openai.json".into(),
        "examples/config.reference.json".into(),
        "container/config.container.json".into(),
    ];
    let valid = json_files("tests/fixtures/config/valid");
    assert!(valid.len() >= 12, "valid fixtures went missing");
    files.extend(
        valid
            .iter()
            .map(|n| format!("tests/fixtures/config/valid/{n}")),
    );
    for f in files {
        if let Err(e) = load(&f) {
            panic!("{f} must load into the real plan, got {e}");
        }
        let doc: Value = serde_json::from_str(&read(&f)).expect("json");
        let errs = schema_errors(&doc);
        assert!(errs.is_empty(), "{f} must match the schema: {errs:?}");
    }
}

#[test]
fn invalid_fixtures_reject_at_the_documented_stage_with_the_documented_error() {
    let index: Value =
        serde_json::from_str(&read("tests/fixtures/config/invalid/index.json")).expect("index");
    let cases = index["cases"].as_array().expect("cases");
    let listed: BTreeSet<String> = cases
        .iter()
        .map(|c| c["file"].as_str().expect("file").to_owned())
        .collect();
    let on_disk: BTreeSet<String> = json_files("tests/fixtures/config/invalid")
        .into_iter()
        .collect();
    assert_eq!(
        listed, on_disk,
        "index.json must list exactly the invalid fixtures"
    );
    let mut stages = BTreeSet::new();
    for c in cases {
        let file = c["file"].as_str().expect("file");
        let (stage, kind, loc) = (
            c["stage"].as_str().expect("stage"),
            c["kind"].as_str().expect("kind"),
            c["location"].as_str().expect("location"),
        );
        stages.insert(stage.to_owned());
        let rel = format!("tests/fixtures/config/invalid/{file}");
        let err = match load(&rel) {
            Err(e) => e,
            Ok(_) => panic!("{file} must be rejected by the loader"),
        };
        assert_eq!(
            (err.kind().as_str(), err.location()),
            (kind, loc),
            "{file}: loader diagnostic"
        );
        // Diagnostics never echo file content or key names.
        let shown = err.to_string();
        assert!(
            !shown.contains("SYNTHETIC") && !shown.contains("api_key"),
            "{file}"
        );
        match stage {
            "syntax" => assert!(
                matches!(kind, "malformed" | "too_large" | "unreadable"),
                "{file}: syntax stage kinds"
            ),
            "version" => assert_eq!(loc, "schema_version", "{file}: version stage location"),
            "structure" | "semantic" => {}
            other => panic!("{file}: unknown stage {other}"),
        }
        // The schema covers the version and structure stages, and accepts what only the
        // loader can reject (semantic), so the loader stays authoritative.
        if let Ok(doc) = serde_json::from_str::<Value>(&read(&rel)) {
            let errs = schema_errors(&doc);
            match stage {
                "version" | "structure" => {
                    assert!(
                        !errs.is_empty(),
                        "{file}: the schema must reject a {stage} failure"
                    );
                }
                _ => assert!(
                    errs.is_empty(),
                    "{file}: the schema cannot express this failure, got {errs:?}"
                ),
            }
        }
    }
    assert_eq!(
        stages,
        BTreeSet::from(["semantic", "structure", "syntax", "version"].map(str::to_owned)),
        "every documented stage needs at least one fixture"
    );
}

#[test]
fn fixtures_carry_no_credential_shaped_text() {
    for dir in [
        "tests/fixtures/config/valid",
        "tests/fixtures/config/invalid",
        "examples",
    ] {
        for n in json_files(dir) {
            let text = read(&format!("{dir}/{n}"));
            for pat in ["sk-", "ghp_", "AKIA", "Bearer ", "BEGIN PRIVATE"] {
                assert!(!text.contains(pat), "{dir}/{n} contains `{pat}`");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Finite bounds: schema ceilings are the loader's ceilings
// ---------------------------------------------------------------------------------------------

fn base() -> Value {
    serde_json::from_str(&read("tests/fixtures/config/valid/openai-loopback.json")).expect("base")
}

fn with_limits(pairs: &[(&str, Value)]) -> Value {
    let mut v = base();
    let limits: serde_json::Map<String, Value> = pairs
        .iter()
        .map(|(k, x)| ((*k).to_owned(), x.clone()))
        .collect();
    v["resources"]["limits"] = Value::Object(limits);
    v
}

/// The other limit that must move with `key` to keep the loader's cross-field rules true.
fn companion(key: &str, at_max: bool, at_min: bool) -> Option<(&'static str, u64)> {
    match (key, at_max, at_min) {
        ("stream_idle_ms", true, _) => Some(("stream_lifetime_ms", 3_600_000)),
        ("stream_lifetime_ms", _, true) => Some(("stream_idle_ms", 1)),
        ("upstream_header_ms", true, _) => Some(("upstream_total_ms", 3_600_000)),
        ("upstream_total_ms", _, true) => Some(("upstream_header_ms", 1)),
        _ => None,
    }
}

#[test]
fn every_schema_limit_boundary_matches_the_loader() {
    let s = schema();
    let limits = s["properties"]["resources"]["properties"]["limits"]["properties"]
        .as_object()
        .expect("limits");
    for (key, spec) in limits {
        let lo = spec["minimum"].as_u64().expect("min");
        let hi = spec["maximum"].as_u64().expect("max");
        let loc = format!("resources.limits.{key}");
        for (value, at_max, at_min) in [(lo, false, true), (hi, true, false)] {
            let mut pairs = vec![(key.as_str(), json!(value))];
            if let Some((ck, cv)) = companion(key, at_max, at_min) {
                pairs.push((ck, json!(cv)));
            }
            let doc = with_limits(&pairs);
            assert!(parse_value(&doc).is_ok(), "{key}={value} must load");
            assert!(
                schema_errors(&doc).is_empty(),
                "{key}={value} must match the schema"
            );
        }
        let mut outside = vec![json!(hi + 1)];
        if lo > 0 {
            outside.push(json!(lo - 1));
        }
        for value in outside {
            let doc = with_limits(&[(key.as_str(), value.clone())]);
            let err = parse_value(&doc).expect_err("outside the range must be rejected");
            assert_eq!(
                (err.kind().as_str(), err.location()),
                ("invalid_value", loc.as_str()),
                "{key}={value}"
            );
            assert!(
                !schema_errors(&doc).is_empty(),
                "{key}={value} must fail the schema"
            );
        }
    }
}

#[test]
fn capacity_and_content_boundaries_match_the_loader() {
    let s = schema();
    let cap = s["properties"]["resources"]["properties"]["capacity"]["properties"]
        .as_object()
        .expect("capacity");
    for (key, spec) in cap {
        let hi = spec["maximum"].as_u64().expect("max");
        assert_eq!(
            (spec["minimum"].as_u64(), hi),
            (Some(1), u64::from(u32::MAX)),
            "{key}"
        );
        for (value, ok) in [(1, true), (hi, true), (0, false), (hi + 1, false)] {
            let mut doc = base();
            doc["resources"]["capacity"][key] = json!(value);
            // The aggregate stream budget is a loader-only product; keep it legal at the maximum.
            doc["resources"]["limits"] = json!({"stream_buffer_bytes": 1});
            assert_eq!(parse_value(&doc).is_ok(), ok, "capacity.{key}={value}");
            assert_eq!(
                schema_errors(&doc).is_empty(),
                ok,
                "capacity.{key}={value} schema"
            );
        }
    }
    for (value, ok) in [(1u64, true), (50_000, true), (0, false), (50_001, false)] {
        let mut doc = base();
        doc["content"]["max_findings"] = json!(value);
        assert_eq!(parse_value(&doc).is_ok(), ok, "max_findings={value}");
        assert_eq!(
            schema_errors(&doc).is_empty(),
            ok,
            "max_findings={value} schema"
        );
    }
    for (n, ok) in [(32usize, true), (33, false)] {
        let mut doc = base();
        doc["content"]["pii"] = json!(vec!["pii:family:global:email"; n]);
        assert_eq!(parse_value(&doc).is_ok(), ok, "pii x{n}");
        assert_eq!(schema_errors(&doc).is_empty(), ok, "pii x{n} schema");
    }
    for name in ["full", "common"] {
        let mut doc = base();
        doc["content"]["profile"] = json!(name);
        assert!(parse_value(&doc).is_ok(), "{name}");
    }
}

/// `1,048,576`, `16 MiB`, `3,600,000 (and at most ...)` to a number.
fn leading_number(cell: &str) -> u64 {
    let cell = cell.trim();
    let digits: String = cell
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(char::is_ascii_digit)
        .collect();
    let n: u64 = digits
        .parse()
        .unwrap_or_else(|_| panic!("number in `{cell}`"));
    let rest = cell[cell
        .find(|c: char| !(c.is_ascii_digit() || c == ','))
        .unwrap_or(cell.len())..]
        .trim_start();
    if rest.starts_with("MiB") {
        n * 1024 * 1024
    } else if rest.starts_with("KiB") {
        n * 1024
    } else {
        n
    }
}

#[test]
fn resource_limits_contract_agrees_with_the_schema() {
    let doc = read("docs/contracts/resource-limits.md");
    let s = schema();
    let limits = s["properties"]["resources"]["properties"]["limits"]["properties"]
        .as_object()
        .expect("limits");
    for (key, spec) in limits {
        let prefix = format!("| `{key}` |");
        let row = doc
            .lines()
            .find(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| panic!("resource-limits.md has no row for `{key}`"));
        let cells: Vec<&str> = row.split('|').collect();
        assert_eq!(
            leading_number(cells[2]),
            spec["default"].as_u64().expect("default"),
            "{key}: provisional value in resource-limits.md vs schema default"
        );
        assert_eq!(
            leading_number(cells[3]),
            spec["maximum"].as_u64().expect("maximum"),
            "{key}: ceiling in resource-limits.md vs schema maximum"
        );
    }
    assert!(
        doc.contains("default 1024, ceiling 50,000"),
        "max_findings default and ceiling"
    );
    assert!(doc.contains("4 GiB"), "aggregate stream bound");
}

// ---------------------------------------------------------------------------------------------
// Authority separation, routes and the planned local_auth keys
// ---------------------------------------------------------------------------------------------

#[test]
fn content_and_resource_variation_cannot_change_deployment_authority() {
    let reference = parse_value(&base()).expect("base");
    let want = format!("{:?}", reference.deployment());
    let mut varied = Vec::new();
    for profile in ["full", "common"] {
        for on_warn in ["reject", "forward"] {
            let mut doc = base();
            doc["content"] = json!({"profile": profile, "on_warn": on_warn, "pii": ["pii:family:global:email"], "max_findings": 7});
            doc["resources"]["limits"] = json!({"max_messages": 4096, "max_connections": 1});
            doc["resources"]["capacity"]["stream"] = json!(1);
            varied.push(doc);
        }
    }
    for doc in varied {
        let plan = parse_value(&doc).expect("variant loads");
        assert_eq!(
            format!("{:?}", plan.deployment()),
            want,
            "deployment authority is content/resource independent"
        );
    }
    // Only the deployment object can name an address or a provider.
    let s = schema();
    fn keys(node: &Value, out: &mut BTreeSet<String>) {
        if let Some(props) = node.get("properties").and_then(Value::as_object) {
            for (k, child) in props {
                out.insert(k.clone());
                keys(child, out);
            }
        }
    }
    for part in ["content", "resources"] {
        let mut names = BTreeSet::new();
        keys(&s["properties"][part], &mut names);
        for word in [
            "address",
            "provider",
            "listener",
            "origin",
            "route",
            "local_auth",
            "token",
        ] {
            assert!(
                !names.contains(word),
                "{part} schema must not define `{word}`"
            );
        }
    }
}

#[test]
fn routes_in_the_schema_match_the_reviewed_route_table() {
    let s = schema();
    let routes = s["x-gateway"]["routes"]["openai"]
        .as_array()
        .expect("routes");
    let status = |st: &str| -> BTreeSet<String> {
        routes
            .iter()
            .filter(|r| r["status"] == st)
            .map(|r| r["id"].as_str().expect("id").to_owned())
            .collect()
    };
    let in_code: BTreeSet<String> = reviewed_routes(Provider::OpenAi)
        .expect("routes")
        .iter()
        .map(|r| r.id().as_str().to_owned())
        .collect();
    assert_eq!(
        status("implemented"),
        in_code,
        "x-gateway.routes.openai `implemented` must equal the reviewed route table; when #86 \
         adds a route, flip its status here and in docs/contracts/config-schema.md"
    );
    assert_eq!(
        in_code,
        BTreeSet::from([
            "openai.chat_completions".to_owned(),
            "openai.responses".to_owned()
        ]),
        "the reviewed table is exactly Chat and Responses (#86, ADR 0031)"
    );
    assert!(status("planned").is_empty(), "no planned route remains");
    // The route is fixed by the provider profile: no config key can name or add one.
    for rel in [
        "tests/fixtures/config/invalid/deployment-unknown-routes.json",
        "tests/fixtures/config/invalid/upstream-route.json",
        "tests/fixtures/config/invalid/upstream-provider-route-id.json",
    ] {
        assert!(load(rel).is_err(), "{rel}");
    }
}

#[test]
fn local_auth_is_implemented_in_the_schema_and_the_loader() {
    let s = schema();
    // The planned placeholder is gone: the definition lives in deployment.properties.
    assert!(s.get("$defs").is_none(), "no planned definitions remain");
    assert!(s["x-gateway"].get("planned").is_none());
    assert!(
        !std::path::Path::new(&root().join("tests/fixtures/config/planned")).exists(),
        "planned fixtures moved into valid/ and invalid/"
    );
    let la = &s["properties"]["deployment"]["properties"]["local_auth"];
    assert_eq!(la["required"], json!(["mode"]));
    assert_eq!(
        la["properties"]["mode"]["enum"],
        json!(["disabled", "token"])
    );
    let token = &la["properties"]["token"];
    assert_eq!(
        (
            token["minProperties"].as_u64(),
            token["maxProperties"].as_u64()
        ),
        (Some(1), Some(1))
    );
    // The schema and the loader agree on every local_auth fixture.
    for (name, valid) in [
        ("local-auth-disabled-explicit.json", true),
        ("local-auth-token-env.json", true),
        ("local-auth-token-file.json", true),
        ("local-auth-non-loopback-with-token.json", true),
    ] {
        let rel = format!("tests/fixtures/config/valid/{name}");
        assert_eq!(load(&rel).is_ok(), valid, "{name}");
    }
}

#[test]
fn local_auth_token_is_a_reference_and_never_a_value() {
    for token in [
        json!({"value": "SYNTHETIC-local-token-0123456789-abcdefghij"}),
        json!({"env": "GATEWAY_LOCAL_TOKEN", "value": "x"}),
        json!({"literal": "x"}),
    ] {
        let mut doc = base();
        doc["deployment"]["local_auth"] = json!({"mode": "token", "token": token});
        let err = parse_value(&doc).expect_err("a value key is not accepted");
        assert_eq!(err.kind().as_str(), "unknown_field");
        assert!(!schema_errors(&doc).is_empty());
        assert!(!err.to_string().contains("SYNTHETIC"));
    }
    let mut doc = base();
    doc["deployment"]["local_auth"] = json!({"mode": "token", "token": "SYNTHETIC-direct-string"});
    assert!(parse_value(&doc).is_err());
    assert!(!schema_errors(&doc).is_empty());
}

#[test]
fn container_config_enforces_a_token_from_a_mounted_file() {
    let doc: Value = serde_json::from_str(&read("container/config.container.json")).expect("json");
    let la = &doc["deployment"]["local_auth"];
    assert_eq!(la["mode"], "token");
    assert_eq!(
        la["token"],
        json!({"file": "/run/secrets/gateway-local-token"})
    );
    assert_eq!(
        doc["deployment"]["listener"]["allow_non_loopback"],
        json!(true)
    );
}

#[test]
fn freeze_metadata_names_an_exact_revision_and_the_provisional_limits() {
    let s = schema();
    let freeze = &s["x-gateway"]["freeze"];
    assert_eq!(freeze["status"], "frozen");
    assert_eq!(freeze["schema_version"], json!(config::SCHEMA_VERSION));
    let rev = freeze["base_revision"].as_str().expect("revision");
    assert!(
        rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()),
        "40-hex revision"
    );
    assert!(
        !freeze["provisional"]
            .as_array()
            .expect("provisional")
            .is_empty()
    );
    let doc = read("docs/contracts/config-schema.md");
    assert!(
        doc.contains(rev),
        "the contract must name the same base revision"
    );
    assert!(doc.contains("docs/schema/gateway-config.v1.schema.json"));
    assert!(Path::new(&root().join("docs/schema/gateway-config.v1.schema.json")).exists());
    assert!(read("docs/configuration.md").contains("docs/schema/gateway-config.v1.schema.json"));
}
