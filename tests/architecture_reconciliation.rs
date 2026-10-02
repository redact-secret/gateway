//! Architecture reconciliation checks for the Alpha 1 qualification (issue #22, the "Architecture
//! and performance qualification additions"). The sealed forwarding types, permit lifetimes,
//! parser conformance, and error-type tests already exist (see
//! `docs/qualification/alpha1-qualification-report.md` for the mapping). These source-level
//! checks pin the structural claims that had no test of their own: no protocol-side transport, an
//! immutable startup plan that is never reread, and no output or logging site outside the CLI.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
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

/// The crate-internal unit-test modules of transport (`#[cfg(test)] mod tests;` and its children).
fn is_test_module(rel: &str) -> bool {
    rel == "transport/tests.rs" || rel.starts_with("transport/tests/")
}

/// Production code of a file: comments dropped and anything from a `#[cfg(test)]` module on cut.
fn production_code(path: &Path) -> String {
    let text = fs::read_to_string(path).expect("read");
    let cut = text
        .find("#[cfg(test)]\nmod tests")
        .or_else(|| text.find("#[cfg(test)]\n#[allow"))
        .unwrap_or(text.len());
    text.get(..cut)
        .unwrap_or("")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn files_in(sub: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    rust_files(&src().join(sub), &mut v);
    assert!(!v.is_empty(), "{sub} must exist");
    v
}

#[test]
fn protocol_boundary_and_core_bridge_hold_no_network_or_transport_types() {
    let mut files = files_in("protocol");
    files.extend(files_in("boundary"));
    files.extend(files_in("core_bridge"));
    files.push(src().join("core_bridge.rs"));
    for path in files {
        let code = production_code(&path);
        for forbidden in [
            "reqwest",
            "hyper",
            "tokio::net",
            "TcpStream",
            "TcpListener",
            "UdpSocket",
            "std::net::Tcp",
            "rustls",
        ] {
            assert!(
                !code.contains(forbidden),
                "{} reaches the network through `{forbidden}`: protocol modules have no independent transport",
                path.display()
            );
        }
    }
}

#[test]
fn only_transport_constructs_http_clients_and_only_the_cli_reads_configuration() {
    let mut all = Vec::new();
    rust_files(&src(), &mut all);
    for path in all {
        let rel = path
            .strip_prefix(src())
            .expect("rel")
            .to_string_lossy()
            .into_owned();
        if is_test_module(&rel) {
            continue;
        }
        let code = production_code(&path);
        let in_transport = rel == "transport.rs" || rel.starts_with("transport/");
        if !in_transport {
            assert!(
                !code.contains("Client::builder") && !code.contains("Client::new"),
                "{rel} builds an HTTP client outside transport"
            );
        }
        // The plan is read once at startup by the CLI (and `config` itself); nothing on the
        // request path re-reads or re-parses configuration.
        if rel != "config.rs" && rel != "cli.rs" {
            assert!(
                !code.contains("load_from_path") && !code.contains("config::parse"),
                "{rel} reads configuration after startup"
            );
        }
    }
}

#[test]
fn the_runtime_plan_types_expose_no_mutation_and_the_request_path_cannot_replace_them() {
    let config = production_code(&src().join("config.rs"));
    assert!(
        !config.contains("&mut self"),
        "config.rs production code has a `&mut self` method: the plan must be immutable after validation"
    );
    assert!(!config.contains("RefCell") && !config.contains("Mutex") && !config.contains("RwLock"));
    // Shared by `Arc` and never replaced: the server holds it behind `Arc<RuntimePlan>`.
    let server = production_code(&src().join("server.rs"));
    assert!(server.contains("Arc<RuntimePlan>"));
    assert!(!server.contains("Mutex<RuntimePlan>") && !server.contains("RwLock<RuntimePlan>"));
}

#[test]
fn only_the_cli_writes_output_and_nothing_logs() {
    let mut all = Vec::new();
    rust_files(&src(), &mut all);
    for path in all {
        let rel = path
            .strip_prefix(src())
            .expect("rel")
            .to_string_lossy()
            .into_owned();
        if is_test_module(&rel) {
            continue;
        }
        if rel == "cli.rs" {
            continue; // fixed, non-payload lines: version, usage, `listening <addr>`, safe errors
        }
        let code = production_code(&path);
        for forbidden in [
            "println!",
            "eprintln!",
            "print!(",
            "eprint!(",
            "dbg!",
            "tracing::",
            "log::",
            "std::io::stdout",
            "std::io::stderr",
        ] {
            assert!(
                !code.contains(forbidden),
                "{rel} has an output or logging site (`{forbidden}`): request data could reach a log"
            );
        }
    }
}

#[test]
fn error_and_diagnostic_enums_own_no_payload_text() {
    // Every `pub enum ...Error` family in production code carries no String, Vec<u8>, or boxed
    // text; `tests/diagnostic_surface.rs` proves the same at compile time (`Copy`) for the
    // reachable ones. This keeps a newly added error type from quietly owning a body or key.
    let mut all = Vec::new();
    rust_files(&src(), &mut all);
    let mut checked = 0_u32;
    for path in all {
        let code = production_code(&path);
        let mut in_error_enum = false;
        for line in code.lines() {
            let t = line.trim_start();
            if t.starts_with("pub enum ")
                && (t.contains("Error") || t.contains("Reject") || t.contains("Failure"))
            {
                in_error_enum = true;
                checked = checked.saturating_add(1);
                continue;
            }
            if in_error_enum {
                if t.starts_with('}') {
                    in_error_enum = false;
                    continue;
                }
                for owned in ["String", "Vec<u8>", "Box<str>", "Cow<"] {
                    assert!(
                        !t.contains(owned),
                        "{}: error enum variant owns text (`{t}`)",
                        path.display()
                    );
                }
            }
        }
    }
    assert!(checked >= 5, "the scan must actually see the error enums");
}
