//! Dependency-pin and module-authority checks (ADR 0005, ADR 0011).

// Test helpers may use expect(); production code may not (clippy.toml covers only #[test] fns).
#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;

/// crates.io sha256 of `redact-secret 0.1.0-beta.12` (source commit
/// 4227160c4dac402d7add53d3f8fe990f693912c1, tag v0.1.0-beta.12), per ADR 0011.
const CORE_CHECKSUM: &str = "2cc951e8b991e9ec27343a872627f53a25b252cc8148cb4ec7160192ed9d856d";

#[test]
fn core_pin_matches_lockfile_and_const() {
    let lock = include_str!("../Cargo.lock");
    let block = lock
        .split("[[package]]")
        .find(|b| b.contains("name = \"redact-secret\"\n"))
        .expect("core is locked");
    assert!(block.contains(&format!(
        "version = \"{}\"",
        redact_secret_gateway::core_bridge::PINNED_CORE_VERSION
    )));
    assert!(block.contains("source = \"registry+https://github.com/rust-lang/crates.io-index\""));
    assert!(block.contains(&format!("checksum = \"{CORE_CHECKSUM}\"")));
}

#[test]
fn manifest_pins_core_exactly() {
    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.contains("redact-secret = \"=0.1.0-beta.12\""));
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Only `transport` may name the HTTP client; `protocol` must not import `transport`
/// or `boundary` (ADR 0005 authority direction). Sealed-constructor privacy is enforced by the compiler (api_boundary).
#[test]
fn module_authority_direction_holds() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty());
    for path in files {
        let text = fs::read_to_string(&path).expect("read source");
        let name = path.to_string_lossy();
        let in_transport = name.ends_with("src/transport.rs");
        let in_protocol = name.contains("src/protocol/");
        // Strip comment lines so documentation can mention the names.
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        if !in_transport {
            assert!(
                !code.contains("reqwest"),
                "reqwest outside transport: {name}"
            );
        }
        if in_protocol {
            assert!(
                !code.contains("transport"),
                "protocol imports transport: {name}"
            );
            assert!(
                !code.contains("boundary"),
                "protocol imports boundary: {name}"
            );
        }
    }
}
