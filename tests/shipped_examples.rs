//! Shipped examples, configs, and SDK pins (issue #22, ADR 0020).
//!
//! Offline checks that the files a clean user follows are consistent with each other and with the
//! SDK qualification: every shipped config validates with the real binary, the example and
//! qualification SDK pins are exact and equal, and both lockfiles carry integrity hashes. The
//! examples themselves are RUN in the qualification workflow; nothing here contacts a network.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    fs::read_to_string(root().join(path)).unwrap_or_else(|_| panic!("read {path}"))
}

#[test]
fn every_shipped_config_validates_with_the_real_binary() {
    for file in [
        "examples/config.skeleton.json",
        "examples/config.openai.json",
        "container/config.container.json",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
            .arg("validate-config")
            .arg(root().join(file))
            .output()
            .expect("run");
        assert!(out.status.success(), "{file} must validate");
    }
}

#[test]
fn the_example_config_names_the_reviewed_provider_and_no_credential() {
    for file in [
        "examples/config.openai.json",
        "container/config.container.json",
    ] {
        let text = read(file);
        assert!(text.contains("\"provider\": \"openai\""), "{file}");
        for forbidden in ["sk-", "Bearer", "api_key", "apikey", "token", "password"] {
            assert!(
                !text.to_ascii_lowercase().contains(forbidden),
                "{file} must carry no credential-shaped text ({forbidden})"
            );
        }
    }
    // The container config is the only one that binds a non-loopback address, and says so.
    let container = read("container/config.container.json");
    assert!(container.contains("\"allow_non_loopback\": true"));
    assert!(!read("examples/config.openai.json").contains("0.0.0.0"));
}

fn node_openai_version(package_json: &str) -> String {
    let doc: serde_json::Value = serde_json::from_str(&read(package_json)).expect("package.json");
    let v = doc["dependencies"]["openai"]
        .as_str()
        .expect("openai dependency")
        .to_owned();
    assert!(
        v.chars().all(|c| c.is_ascii_digit() || c == '.'),
        "{package_json}: the SDK must be pinned to an exact version, found `{v}`"
    );
    // Every other dependency is exact too (no ranges anywhere).
    for section in ["dependencies", "devDependencies"] {
        if let Some(map) = doc[section].as_object() {
            for (name, ver) in map {
                let ver = ver.as_str().expect("version");
                assert!(
                    !ver.starts_with(['^', '~', '>', '<', '*']) && ver != "latest",
                    "{package_json}: {name} must be exact, found `{ver}`"
                );
            }
        }
    }
    v
}

fn python_openai_version(requirements_in: &str) -> String {
    let line = read(requirements_in)
        .lines()
        .find(|l| l.starts_with("openai=="))
        .expect("openai pin")
        .to_owned();
    line.trim_start_matches("openai==").trim().to_owned()
}

#[test]
fn sdk_pins_are_exact_and_the_examples_match_the_qualified_versions() {
    let node_example = node_openai_version("examples/node/package.json");
    let node_qual = node_openai_version("qualification/sdk/node/package.json");
    assert_eq!(
        node_example, node_qual,
        "examples must use the qualified Node SDK"
    );
    let py_example = python_openai_version("examples/python/requirements.in");
    let py_qual = python_openai_version("qualification/sdk/python/requirements.in");
    assert_eq!(
        py_example, py_qual,
        "examples must use the qualified Python SDK"
    );
    // The compiled, hashed requirements agree with the top-level pin.
    for file in [
        "examples/python/requirements.txt",
        "qualification/sdk/python/requirements.txt",
    ] {
        assert!(
            read(file).contains(&format!("openai=={py_example} \\")),
            "{file}"
        );
    }
}

#[test]
fn lockfiles_carry_integrity_hashes_for_everything_installed() {
    for lock in [
        "examples/node/package-lock.json",
        "qualification/sdk/node/package-lock.json",
    ] {
        let doc: serde_json::Value = serde_json::from_str(&read(lock)).expect("lock");
        let packages = doc["packages"].as_object().expect("packages");
        let mut installed = 0_u32;
        for (path, entry) in packages {
            if path.is_empty() {
                continue; // the root project
            }
            installed = installed.saturating_add(1);
            assert!(
                entry["integrity"]
                    .as_str()
                    .is_some_and(|i| i.starts_with("sha512-")),
                "{lock}: {path} has no sha512 integrity"
            );
            assert!(
                entry["resolved"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("https://registry.npmjs.org/")),
                "{lock}: {path} must resolve from the npm registry"
            );
        }
        assert!(installed > 0, "{lock} must list the installed packages");
    }
    for file in [
        "examples/python/requirements.txt",
        "qualification/sdk/python/requirements.txt",
    ] {
        let text = read(file);
        // Every requirement line is an exact pin followed by at least one sha256 hash.
        let mut pins = 0_u32;
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if line.is_empty() || line.starts_with(['#', ' ']) {
                continue;
            }
            pins = pins.saturating_add(1);
            assert!(line.contains("=="), "{file}: `{line}` is not an exact pin");
            let next = lines.peek().copied().unwrap_or("");
            assert!(
                next.trim_start().starts_with("--hash=sha256:"),
                "{file}: `{line}` has no hash"
            );
        }
        assert!(pins >= 5, "{file}: the SDK's dependencies are locked too");
        assert!(!text.contains("--index-url") && !text.contains("git+"));
    }
}

#[test]
fn examples_and_compose_keep_their_safety_labels() {
    let node = read("examples/node/openai-via-gateway.ts");
    let py = read("examples/python/openai_via_gateway.py");
    for (name, text) in [("node", node), ("python", py)] {
        assert!(
            text.contains("OPENAI_API_KEY"),
            "{name} reads the key at runtime"
        );
        assert!(text.contains("Never put a key in this file"), "{name}");
        assert!(
            text.contains("maxRetries: 0") || text.contains("max_retries=0"),
            "{name}"
        );
        assert!(
            text.contains("Verified how"),
            "{name} states how it was verified"
        );
        assert!(!text.contains("sk-"), "{name} carries no key-shaped text");
    }
    let compose = read("examples/compose/compose.yaml");
    assert!(compose.contains("127.0.0.1:8787:8787"));
    assert!(!compose.contains("0.0.0.0:"));
    assert!(compose.contains("cap_drop"));
    assert!(!compose.to_ascii_lowercase().contains("environment:"));
    assert!(!Path::new(&root().join("examples/compose/compose.skeleton.yaml")).exists());
}
