//! Upgrade, rollback and restart-activation compatibility (#64; ADR 0033,
//! docs/contracts/config-upgrade-rollback.md). Every migration fixture under
//! `tests/fixtures/config/migration/` goes through the real loader and the real binary;
//! unsupported older and newer combinations are rejected with a documented kind and static
//! location; a failed start never announces a listener; and the docs name the same files and
//! pairs the index lists. Synthetic data only; nothing reads a network.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use redact_secret_gateway::config::{self, ConfigError};
use redact_secret_gateway::transport::local_auth::{LocalToken, TokenReference};
use serde_json::Value;

const TOKEN: &str = "SYNTHETIC-migration-local-token-0123456789-abc";
const BASE: &str = "tests/fixtures/config/migration";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|_| panic!("read {rel}"))
}

fn index() -> Value {
    serde_json::from_str(&read(&format!("{BASE}/index.json"))).expect("index")
}

fn synthetic_token(_: &TokenReference) -> Result<LocalToken, ConfigError> {
    LocalToken::from_bytes(TOKEN.as_bytes())
}

fn load(rel: &str) -> Result<config::RuntimePlan, ConfigError> {
    config::load_from_path_with(&root().join(BASE).join(rel), &synthetic_token)
}

fn files_in(dir: &str) -> BTreeSet<String> {
    fs::read_dir(root().join(BASE).join(dir))
        .expect("dir")
        .map(|e| format!("{dir}/{}", e.expect("entry").file_name().to_string_lossy()))
        .filter(|n| n.ends_with(".json"))
        .collect()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("str").to_owned())
        .collect()
}

#[test]
fn the_index_lists_exactly_the_migration_fixtures() {
    let idx = index();
    let mut listed = BTreeSet::new();
    listed.extend(strs(&idx["alpha_loads_unchanged"]));
    for u in idx["upgrades"].as_array().expect("upgrades") {
        listed.insert(u["from"].as_str().expect("from").to_owned());
        listed.insert(u["to"].as_str().expect("to").to_owned());
    }
    for r in idx["rejected"].as_array().expect("rejected") {
        listed.insert(r["file"].as_str().expect("file").to_owned());
    }
    let mut on_disk = files_in("alpha");
    on_disk.extend(files_in("beta"));
    on_disk.extend(files_in("rejected"));
    assert_eq!(listed, on_disk, "index.json must list exactly the fixtures");
}

#[test]
fn alpha_configs_that_remain_valid_load_unchanged_with_no_auth() {
    for f in strs(&index()["alpha_loads_unchanged"]) {
        let plan = load(&f).unwrap_or_else(|e| panic!("{f}: {e}"));
        assert!(
            !plan.deployment().local_auth().is_enforced(),
            "{f}: an Alpha file keeps meaning no caller token"
        );
    }
}

#[test]
fn every_upgrade_target_loads_and_says_exactly_what_it_adds() {
    for u in index()["upgrades"].as_array().expect("upgrades") {
        let (id, from, to) = (
            u["id"].as_str().expect("id"),
            u["from"].as_str().expect("from"),
            u["to"].as_str().expect("to"),
        );
        let target = load(to).unwrap_or_else(|e| panic!("{id}: {to}: {e}"));
        let adds = u["adds_auth"].as_bool().expect("adds_auth");
        assert_eq!(
            target.deployment().local_auth().is_enforced(),
            adds,
            "{id}: adds_auth must equal what the loader enforces"
        );
        // The source either still loads (and then means no auth) or is rejected with the
        // documented tightening. Nothing else may change between the pair.
        match (load(from), u.get("alpha_rejected_by_beta")) {
            (Ok(p), None) => assert!(!p.deployment().local_auth().is_enforced(), "{id}"),
            (Err(e), Some(exp)) => assert_eq!(
                (e.kind().as_str(), e.location()),
                (
                    exp["kind"].as_str().expect("kind"),
                    exp["location"].as_str().expect("location")
                ),
                "{id}"
            ),
            (Ok(_), Some(_)) => panic!("{id}: documented as rejected but the loader accepts it"),
            (Err(e), None) => panic!("{id}: {from} must load, got {e}"),
        }
        // The migration is only the local_auth addition: removing that key from the target
        // yields the source byte-for-byte as JSON. That is also the rollback config.
        let mut stripped: Value = serde_json::from_str(&read(&format!("{BASE}/{to}"))).unwrap();
        stripped["deployment"]
            .as_object_mut()
            .unwrap()
            .remove("local_auth");
        let source: Value = serde_json::from_str(&read(&format!("{BASE}/{from}"))).unwrap();
        assert_eq!(
            stripped, source,
            "{id}: target differs from source by more than local_auth"
        );
    }
}

#[test]
fn unsupported_combinations_are_rejected_explicitly_and_safely() {
    let idx = index();
    let cases = idx["rejected"].as_array().expect("rejected");
    assert!(cases.len() >= 5);
    let mut kinds = BTreeSet::new();
    for c in cases {
        let file = c["file"].as_str().expect("file");
        let err = match load(file) {
            Err(e) => e,
            Ok(_) => panic!("{file} must be rejected"),
        };
        assert_eq!(
            (err.kind().as_str(), err.location()),
            (
                c["kind"].as_str().expect("kind"),
                c["location"].as_str().expect("location")
            ),
            "{file}"
        );
        kinds.insert(err.kind().as_str());
        let shown = err.to_string();
        for leak in ["SYNTHETIC", "rotation", "credential", "GATEWAY_LOCAL_TOKEN"] {
            assert!(!shown.contains(leak), "{file}: diagnostic echoed {leak}");
        }
        assert!(c["why"].as_str().is_some_and(|w| !w.is_empty()), "{file}");
    }
    for needed in [
        "unsupported_schema_version",
        "unknown_field",
        "invalid_combination",
    ] {
        assert!(kinds.contains(needed), "no rejection fixture for {needed}");
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rsg-migration-{}-{tag}", std::process::id()));
    fs::create_dir_all(&d).expect("mkdir");
    d
}

#[test]
fn a_rejected_config_never_announces_a_listener_or_serves() {
    for c in index()["rejected"].as_array().expect("rejected") {
        let file = c["file"].as_str().expect("file");
        let path = root().join(BASE).join(file);
        for sub in ["serve", "validate-config"] {
            let out = bin().arg(sub).arg(&path).output().expect("run");
            assert_eq!(out.status.code(), Some(1), "{sub} {file}");
            assert!(
                out.stdout.is_empty(),
                "{sub} {file}: nothing is announced (no `listening`, no ready signal)"
            );
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(
                err.contains(&format!(
                    "invalid_config: {} at {}",
                    c["kind"].as_str().unwrap(),
                    c["location"].as_str().unwrap()
                )),
                "{sub} {file}: {err}"
            );
            assert!(!err.contains("SYNTHETIC"), "{file}");
        }
    }
}

#[test]
fn an_upgraded_token_config_without_its_secret_refuses_to_start() {
    // The migrated config is valid as a file, but activation needs the secret reference to
    // resolve. With the variable unset the start fails: no listener, no readiness.
    let path = root()
        .join(BASE)
        .join("beta/loopback-openai-token-env.json");
    let out = bin()
        .arg("serve")
        .arg(&path)
        .env_remove("GATEWAY_LOCAL_TOKEN")
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("invalid_config: unreadable at deployment.local_auth.token"));
    assert!(!err.contains("GATEWAY_LOCAL_TOKEN"));
}

fn http(addr: &str, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).expect("t");
    s.write_all(request.as_bytes()).expect("write");
    let mut text = String::new();
    let _ = s.read_to_string(&mut text);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("status");
    (status, text)
}

fn serve_until_listening(cfg: &Path, token_env: Option<&str>) -> (std::process::Child, String) {
    let mut cmd = bin();
    cmd.arg("serve")
        .arg(cfg)
        .env_remove("GATEWAY_LOCAL_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = token_env {
        cmd.env("GATEWAY_LOCAL_TOKEN", t);
    }
    let mut child = cmd.spawn().expect("spawn");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("line");
    let addr = line
        .trim()
        .strip_prefix("listening ")
        .expect("listening handshake")
        .to_owned();
    (child, addr)
}

fn stop(mut child: std::process::Child) {
    let ok = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill")
        .success();
    assert!(ok);
    assert!(child.wait().expect("wait").success(), "graceful exit 0");
}

fn with_port_zero(rel: &str, d: &Path, name: &str) -> PathBuf {
    let doc = read(&format!("{BASE}/{rel}")).replace("127.0.0.1:8787", "127.0.0.1:0");
    let path = d.join(name);
    fs::write(&path, doc).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
    path
}

#[test]
fn upgrade_and_rollback_are_restarts_and_rollback_never_hides_a_downgrade() {
    let d = tmp("cycle");
    let alpha = with_port_zero("alpha/loopback-openai.json", &d, "alpha.json");
    let beta = with_port_zero("beta/loopback-openai-token-env.json", &d, "beta.json");
    let probe = "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
                 Authorization: Bearer sk-SYNTHETIC-REVOKED-MIGRATION-0000\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let ready = "GET /readyz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

    // Before: Alpha-style config, no caller token.
    let (child, addr) = serve_until_listening(&alpha, None);
    assert_eq!(http(&addr, ready).0, 200);
    assert_ne!(
        http(&addr, probe).0,
        401,
        "Alpha-style: no caller token asked for"
    );
    // Editing the file while running changes nothing: activation is a restart.
    fs::copy(&beta, &alpha).expect("overwrite");
    assert!(
        !http(&addr, probe).1.contains("local_auth"),
        "no hot reload"
    );
    stop(child);

    // After the restart onto the migrated config the token is enforced.
    let (child, addr) = serve_until_listening(&alpha, Some(TOKEN));
    assert_eq!(http(&addr, ready).0, 200, "probes stay open");
    let (status, body) = http(&addr, probe);
    assert_eq!(status, 401);
    assert!(body.contains("local_auth_required"));
    stop(child);

    // Rolling the config back to the Alpha shape (local_auth removed) is accepted by the
    // loader as an explicit no-auth deployment: the downgrade is the operator's visible
    // edit, never something a binary does by ignoring the key.
    let mut doc: Value = serde_json::from_str(&read(&format!(
        "{BASE}/beta/loopback-openai-token-env.json"
    )))
    .unwrap();
    doc["deployment"]
        .as_object_mut()
        .unwrap()
        .remove("local_auth");
    let rolled = d.join("rolled-back.json");
    fs::write(&rolled, doc.to_string()).expect("write");
    let plan = config::load_from_path(&rolled).expect("alpha-shaped config loads");
    assert!(!plan.deployment().local_auth().is_enforced());
}

#[test]
fn docs_name_the_same_fixtures_pairs_and_semantics() {
    let contract = read("docs/contracts/config-upgrade-rollback.md");
    let idx = index();
    for u in idx["upgrades"].as_array().expect("upgrades") {
        for key in ["from", "to"] {
            let f = u[key].as_str().unwrap();
            assert!(contract.contains(f), "contract must name {f}");
        }
    }
    for r in idx["rejected"].as_array().expect("rejected") {
        let f = r["file"].as_str().unwrap();
        assert!(contract.contains(f), "contract must name {f}");
    }
    for needle in [
        "restart",
        "no hot reload",
        "unknown_field",
        "unsupported_schema_version",
        "docs/contracts/config-schema.md",
        "ADR 0033",
        "claims no",
    ] {
        assert!(
            contract.to_lowercase().contains(&needle.to_lowercase()),
            "contract must mention {needle}"
        );
    }
    let linkers = [
        ("README.md", "config-upgrade-rollback.md"),
        ("docs/configuration.md", "config-upgrade-rollback.md"),
        (
            "docs/contracts/config-schema.md",
            "config-upgrade-rollback.md",
        ),
        ("docs/contracts/README.md", "config-upgrade-rollback.md"),
        (
            "docs/decisions/README.md",
            "0033-config-upgrade-rollback.md",
        ),
        ("docs/artifacts.md", "config-upgrade-rollback.md"),
    ];
    for (file, needle) in linkers {
        assert!(read(file).contains(needle), "{file} must link {needle}");
    }
    // No stable-compatibility or production claim slipped into the public docs.
    for file in [
        "README.md",
        "docs/configuration.md",
        "docs/contracts/config-upgrade-rollback.md",
    ] {
        let text = read(file).to_lowercase();
        for banned in [
            "seamless rotation is supported",
            "production-ready",
            "stable release",
        ] {
            assert!(!text.contains(banned), "{file}: {banned}");
        }
    }
}
