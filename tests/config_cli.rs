//! Real-binary tests: config validation, startup rejection before binding, loopback
//! default, serve + SIGTERM, and static (no hot reload) configuration. Synthetic data only.

#![allow(clippy::expect_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const SAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/config.skeleton.json");
const MARKER: &str = "SYNTH-SECRET-MARKER-0000";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
}

fn config_with(addr_line: &str, extra: &str) -> String {
    format!(
        r#"{{"schema_version": 1, {extra}
        "deployment": {{"listener": {{{addr_line}}}}},
        "content": {{"profile": "common"}},
        "resources": {{"capacity": {{"receipt": 1, "memory_units": 1, "inspection": 1,
        "upstream": 1, "stream": 1}}}}}}"#
    )
}

fn temp_file(name: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsg-config-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write");
    path
}

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .expect("write");
    let mut text = String::new();
    s.read_to_string(&mut text).expect("read");
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("status");
    (status, text)
}

struct Serving {
    child: Child,
    addr: String,
    out: BufReader<std::process::ChildStdout>,
}

fn serve(path: &Path) -> Serving {
    let mut child = bin()
        .arg("serve")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("listening line");
    let addr = line
        .trim()
        .strip_prefix("listening ")
        .expect("handshake")
        .to_owned();
    Serving { child, addr, out }
}

fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
}

#[test]
fn sample_config_validates() {
    let out = bin()
        .args(["validate-config", SAMPLE])
        .output()
        .expect("run");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "config valid (schema_version 1)"
    );
}

#[test]
fn invalid_configs_exit_nonzero_with_safe_diagnostics() {
    let cases = [
        (
            "unknown.json",
            config_with(r#""address": "127.0.0.1:0""#, &format!(r#""{MARKER}": 1,"#)),
        ),
        ("nonloop.json", config_with(r#""address": "0.0.0.0:0""#, "")),
        ("malformed.json", format!("{{ {MARKER}")),
        (
            "version.json",
            config_with(r#""address": "127.0.0.1:0""#, "")
                .replace("\"schema_version\": 1", "\"schema_version\": 99"),
        ),
    ];
    for (name, doc) in cases {
        let path = temp_file(name, &doc);
        let out = bin()
            .args(["validate-config"])
            .arg(&path)
            .output()
            .expect("run");
        assert_eq!(out.status.code(), Some(1), "{name}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("invalid_config"), "{name}: {stderr}");
        assert!(!stderr.contains(MARKER), "{name}");
        assert!(!stderr.contains(path.to_string_lossy().as_ref()), "{name}");
        assert!(out.stdout.is_empty());
    }
    let missing = bin()
        .args(["validate-config", "/nonexistent/SYNTH-PATH.json"])
        .output()
        .expect("run");
    assert_eq!(missing.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&missing.stderr).contains("SYNTH-PATH"));
}

#[test]
fn invalid_config_fails_before_binding() {
    // Reserve a port, release it, point an invalid config at it, and confirm `serve`
    // exits without ever holding it.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    let port = probe.local_addr().expect("addr").port();
    drop(probe);
    let doc = config_with(
        &format!(r#""address": "127.0.0.1:{port}""#),
        r#""unexpected": true,"#,
    );
    let path = temp_file("prebind.json", &doc);
    let out = bin().arg("serve").arg(&path).output().expect("run");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "nothing is announced as listening");
    assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
}

#[test]
fn non_loopback_with_explicit_setting_validates() {
    let path = temp_file(
        "nonloop-ack.json",
        &config_with(r#""address": "0.0.0.0:0", "allow_non_loopback": true"#, ""),
    );
    let out = bin()
        .args(["validate-config"])
        .arg(&path)
        .output()
        .expect("run");
    assert!(out.status.success());
}

#[test]
fn serve_runs_health_rejects_proxy_routes_and_exits_cleanly_on_sigterm() {
    let path = temp_file(
        "serve.json",
        &config_with(r#""address": "127.0.0.1:0""#, ""),
    );
    let mut s = serve(&path);
    assert!(s.addr.starts_with("127.0.0.1:"));
    assert_eq!(http_get(&s.addr, "/healthz").0, 200);
    assert_eq!(http_get(&s.addr, "/readyz").0, 200);
    let (status, text) = http_get(&s.addr, &format!("/v1/chat/completions?x={MARKER}"));
    assert_eq!(status, 404);
    assert!(text.contains("unsupported_input"));
    assert!(!text.contains(MARKER));

    sigterm(&s.child);
    let status = s.child.wait().expect("wait");
    assert!(status.success(), "graceful exit code 0");
    let mut rest = String::new();
    s.out.read_to_string(&mut rest).expect("rest");
    assert!(rest.contains("shutdown complete"));
}

#[test]
fn configuration_is_static_after_startup() {
    let path = temp_file(
        "static.json",
        &config_with(r#""address": "127.0.0.1:0""#, ""),
    );
    let mut s = serve(&path);
    assert_eq!(http_get(&s.addr, "/readyz").0, 200);
    // Replace with an invalid file, then remove it: a running server must not notice.
    std::fs::write(&path, "{ not json").expect("rewrite");
    for _ in 0..5 {
        assert_eq!(http_get(&s.addr, "/readyz").0, 200);
    }
    std::fs::remove_file(&path).expect("remove");
    for _ in 0..5 {
        assert_eq!(http_get(&s.addr, "/readyz").0, 200);
        assert_eq!(http_get(&s.addr, "/healthz").0, 200);
    }
    sigterm(&s.child);
    assert!(s.child.wait().expect("wait").success());
}
