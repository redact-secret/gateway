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
    http_request(addr, path, "GET")
}

fn http_request(addr: &str, path: &str, method: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
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
    serve_mode(path, "serve")
}

fn serve_mode(path: &Path, mode: &str) -> Serving {
    let mut child = bin()
        .arg(mode)
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

#[test]
fn operations_export_is_opt_in_loopback_and_has_no_request_labels() {
    let path = temp_file(
        "observed.json",
        &config_with(r#""address": "127.0.0.1:0""#, ""),
    );
    let mut ordinary = serve(&path);
    assert_eq!(http_get(&ordinary.addr, "/metrics").0, 404);
    sigterm(&ordinary.child);
    assert!(ordinary.child.wait().expect("ordinary stop").success());
    let mut observed = serve_mode(&path, "serve-observed");
    let rejected = http_get(&observed.addr, &format!("/metrics?{MARKER}"));
    assert_eq!(rejected.0, 404);
    assert!(!rejected.1.contains(MARKER));
    assert!(
        rejected
            .1
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(
        rejected
            .1
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    let (status, response) = http_get(&observed.addr, "/metrics");
    assert_eq!(status, 200);
    assert!(!response.contains(MARKER));
    let (_, body) = response.split_once("\r\n\r\n").expect("HTTP body");
    let snapshot: serde_json::Value = serde_json::from_str(body).expect("snapshot");
    assert_eq!(snapshot["snapshot_version"], 1);
    assert_eq!(snapshot["stages"].as_object().expect("stages").len(), 10);
    assert_eq!(snapshot["upstream_attempts"], 0);
    assert_eq!(snapshot["admission"]["waiting"], 0);
    for method in ["HEAD", "POST"] {
        let (status, response) = http_request(&observed.addr, "/metrics", method);
        assert_eq!(status, 405);
        let headers = response.to_ascii_lowercase();
        assert!(headers.contains("allow: get"));
        assert!(headers.contains("content-type: application/json"));
        assert!(headers.contains("cache-control: no-store"));
    }
    assert!(
        response
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );
    sigterm(&observed.child);
    assert!(observed.child.wait().expect("observed stop").success());
}

#[test]
fn operations_export_refuses_non_loopback_even_with_valid_proxy_auth() {
    let mut doc: serde_json::Value = serde_json::from_str(&config_with(
        r#""address":"0.0.0.0:0","allow_non_loopback":true"#,
        "",
    ))
    .expect("config");
    doc["deployment"]["local_auth"] =
        serde_json::json!({"mode":"token","token":{"env":"SYNTHETIC_OPERATIONS_TOKEN"}});
    let path = temp_file("observed-nonloopback.json", &doc.to_string());
    let out = bin()
        .arg("serve-observed")
        .arg(path)
        .env(
            "SYNTHETIC_OPERATIONS_TOKEN",
            "SYNTHETIC-OPERATIONS-TOKEN-NOT-REAL-000000",
        )
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "error: transport_failure: bind"
    );
}

#[test]
fn exec_probe_has_one_total_deadline_against_a_trickling_response() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (started, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut line = String::new();
        BufReader::new(&mut socket)
            .read_line(&mut line)
            .expect("request line");
        started.send(std::time::Instant::now()).expect("started");
        for _ in 0..60 {
            if socket.write_all(b"H").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let child = bin()
        .args(["probe", "ready", &addr.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("probe");
    // Gate on the actual request: process loading is outside the probe IO deadline.
    let start = received
        .recv_timeout(Duration::from_secs(5))
        .expect("request started");
    let result = child.wait_with_output().expect("probe result");
    assert_eq!(result.status.code(), Some(1));
    assert!(start.elapsed() < Duration::from_secs(4));
    worker.join().expect("worker");
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
fn exec_probe_uses_health_only_and_bounds_invalid_arguments() {
    let path = temp_file(
        "exec-probe.json",
        &config_with(r#""address": "127.0.0.1:0""#, ""),
    );
    let mut serving = serve(&path);
    for kind in ["live", "ready"] {
        let result = bin()
            .args(["probe", kind, &serving.addr])
            .output()
            .expect("probe");
        assert!(result.status.success());
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }
    for args in [
        ["probe", "ready", "192.0.2.1:8787"],
        ["probe", "ready", "localhost:8787"],
        ["probe", "ready", "127.0.0.1:0"],
        ["probe", MARKER, "127.0.0.1:8787"],
    ] {
        let result = bin().args(args).output().expect("invalid probe");
        assert_eq!(result.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(MARKER));
    }
    sigterm(&serving.child);
    assert!(serving.child.wait().expect("wait").success());
    assert_eq!(
        bin()
            .args(["probe", "ready", &serving.addr])
            .status()
            .expect("closed probe")
            .code(),
        Some(1)
    );
}

#[test]
fn exec_probe_refuses_false_health_responses_and_oversize() {
    for response in [
        "HTTP/1.1 200 OK\r\nContent-Length: 18\r\n\r\n{\"status\":\"wrong\"}".to_owned(),
        "HTTP/1.1 503 Service Unavailable\r\n\r\n{\"status\":\"ready\"}".to_owned(),
        format!("HTTP/1.1 200 OK\r\n\r\n{}", "x".repeat(1100)),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let mut request = String::new();
            BufReader::new(&mut socket)
                .read_line(&mut request)
                .expect("request line");
            assert_eq!(request, "GET /readyz HTTP/1.1\r\n");
            let _ = socket.write_all(response.as_bytes());
        });
        let result = bin()
            .args(["probe", "ready", &addr.to_string()])
            .output()
            .expect("probe");
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).trim(),
            "probe failed"
        );
        worker.join().expect("worker");
    }
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
fn non_loopback_with_explicit_setting_requires_a_local_token() {
    let address = r#""address": "0.0.0.0:0", "allow_non_loopback": true"#;
    // The acknowledgement alone is rejected (#63, ADR 0030).
    let path = temp_file("nonloop-ack.json", &config_with(address, ""));
    let out = bin()
        .args(["validate-config"])
        .arg(&path)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("invalid_config: invalid_combination at deployment.local_auth.mode")
    );
    // With a token source it validates; the token comes from the environment here.
    let doc = config_with(address, "").replace(
        "\"deployment\": {",
        r#""deployment": {"local_auth": {"mode": "token", "token": {"env": "RSG_SYNTH_LOCAL_TOKEN"}},"#,
    );
    let path = temp_file("nonloop-token.json", &doc);
    let out = bin()
        .args(["validate-config"])
        .arg(&path)
        .env(
            "RSG_SYNTH_LOCAL_TOKEN",
            "SYNTHETIC-config-cli-local-token-0123456789",
        )
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
    // The chat route is wired (#18): a GET is a local 405, still without any forwarding.
    let (status, text) = http_get(&s.addr, &format!("/v1/chat/completions?x={MARKER}"));
    assert_eq!(status, 405);
    assert!(text.contains("unsupported_input"));
    assert!(!text.contains(MARKER));
    // Every other route stays a local 404.
    let (status, text) = http_get(&s.addr, &format!("/v1/models/{MARKER}"));
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
