//! Black-box adversarial checks against the real built binary (#25). Synthetic data only.
//!
//! The binary is started with a configuration that has **no upstream**, with a hostile
//! environment (proxy variables, base-URL variables, and a bogus trust-store path that no
//! gateway code reads), and is attacked over raw sockets. With no upstream, a request that
//! passes every check ends in a local `501 not_implemented`; every attack below must end
//! earlier (a 4xx or a closed connection), so reaching `501` or `2xx` would mean the attack
//! got past admission. Checks that need a fake provider (zero upstream bytes, redirects,
//! credential isolation) cannot run against the real binary by design (there is no
//! production path to a fake; `tests/destination_policy.rs`) and live in
//! `src/transport/tests/attack_tests.rs`.
//!
//! Control map: `docs/qualification/alpha1-threat-control-map.md`.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::leak::Markers;

const KEY: &str = "sk-SYNTHETIC-REVOKED-ATK0-0000-NOT-A-KEY";
const HOSTILE: &str = "SYNTH-HOSTILE-ROUTE-6F3A";
const GOOD: &str = r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hello"}]}"#;
const PATH: &str = "/v1/chat/completions";

struct Gateway {
    child: Child,
    addr: String,
    out: BufReader<std::process::ChildStdout>,
}

fn config() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsg-attack-surface-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("attack.json");
    std::fs::write(
        &path,
        r#"{"schema_version": 1,
        "deployment": {"listener": {"address": "127.0.0.1:0"}},
        "content": {"profile": "common"},
        "resources": {"capacity": {"receipt": 4, "memory_units": 8192, "inspection": 1,
        "upstream": 1, "stream": 1}, "limits": {"body_deadline_ms": 1000}}}"#,
    )
    .expect("write");
    path
}

fn start() -> Gateway {
    let mut child = Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
        .arg("serve")
        .arg(config())
        // Nothing in the gateway reads any of these; they exist to prove that.
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "*")
        .env("OPENAI_BASE_URL", "http://evil.test/v1")
        .env("OPENAI_API_BASE", "http://evil.test/v1")
        .env("OPENAI_API_KEY", KEY)
        .env("SSL_CERT_FILE", "/nonexistent/SYNTH-HOSTILE-ROUTE-6F3A")
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
    Gateway { child, addr, out }
}

impl Gateway {
    /// Write `bytes`, read until the server closes (bounded; closing is the gated event).
    fn exchange(&self, bytes: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(&self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let _ = stream.write_all(bytes);
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return out,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
    }

    fn stop(mut self) -> (String, String) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(status.success());
        let exit = self.child.wait().expect("wait");
        assert!(exit.success(), "graceful exit");
        let mut stdout = String::new();
        self.out.read_to_string(&mut stdout).expect("stdout");
        let mut stderr = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            e.read_to_string(&mut stderr).expect("stderr");
        }
        (stdout, stderr)
    }
}

fn status_of(bytes: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix("HTTP/1.1 ")?.get(..3)?.parse().ok()
}

fn post(target: &str, extra: &str, framing: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {target} HTTP/1.1\r\nHost: gw.test\r\nContent-Type: application/json\r\n\
         Authorization: Bearer {KEY}\r\n{framing}Connection: close\r\n{extra}\r\n{body}"
    )
    .into_bytes()
}

fn good() -> Vec<u8> {
    post(
        PATH,
        "",
        &format!("Content-Length: {}\r\n", GOOD.len()),
        GOOD,
    )
}

#[test]
fn real_binary_resists_routing_framing_and_slow_head_attacks() {
    let gateway = start();
    let markers = Markers::standard()
        .with("key", KEY)
        .with("hostile", HOSTILE);

    // Control: a clean request passes every local check and ends at the missing upstream.
    let ok = gateway.exchange(&good());
    assert_eq!(status_of(&ok), Some(501));

    // Routing: none of these may pass admission.
    let n = GOOD.len();
    let routing: Vec<Vec<u8>> = vec![
        post(
            &format!("http://evil.test{PATH}"),
            "",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
        post(
            &format!("//{PATH}"),
            "",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
        post(
            "/v1/%63hat/completions",
            "",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
        post(
            "/V1/CHAT/COMPLETIONS",
            "",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
        post(
            &format!("{PATH}?upstream=http://evil.test"),
            "",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
        post(
            PATH,
            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: c3ludGhldGlj\r\n",
            &format!("Content-Length: {n}\r\n"),
            GOOD,
        ),
        b"CONNECT evil.test:443 HTTP/1.1\r\nHost: evil.test:443\r\n\r\n".to_vec(),
        format!("GET {PATH} HTTP/1.1\r\nHost: gw.test\r\n\r\n").into_bytes(),
    ];
    for (i, request) in routing.iter().enumerate() {
        let wire = gateway.exchange(request);
        if let Some(status) = status_of(&wire) {
            assert!((400..500).contains(&status), "routing case {i}: {status}");
        }
        markers.assert_clean(&format!("routing case {i}"), &wire);
    }

    // Framing: ambiguity is answered with the local 400 and closed, never admitted.
    let ch = format!("{:x}\r\n{GOOD}\r\n0\r\n\r\n", GOOD.len());
    for (label, fields) in [
        (
            "CL then TE",
            format!("Content-Length: {n}\r\nTransfer-Encoding: chunked\r\n"),
        ),
        (
            "TE then CL",
            format!("Transfer-Encoding: chunked\r\nContent-Length: {n}\r\n"),
        ),
    ] {
        let wire = gateway.exchange(&post(PATH, "", &fields, &ch));
        assert_eq!(status_of(&wire), Some(400), "{label}: local refusal");
        let text = String::from_utf8_lossy(&wire).to_ascii_lowercase();
        assert!(text.contains("connection: close"), "{label}");
        assert!(
            text.contains(r#"{"error":{"code":"malformed_input"}}"#),
            "{label}"
        );
    }

    // One request per connection: every response says so, and a pipelined second request
    // gets no answer.
    let health =
        b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\nGET /healthz HTTP/1.1\r\nHost: x\r\n\r\n";
    let wire = gateway.exchange(health);
    let text = String::from_utf8_lossy(&wire).to_ascii_lowercase();
    assert_eq!(status_of(&wire), Some(200));
    assert!(text.contains("connection: close"));
    assert_eq!(text.matches("http/1.1 ").count(), 1, "one response only");

    // Slow heads: a silent connection and a partial head are cut at the head deadline
    // (1000 ms by configuration), without a response and without holding the server.
    let silent = TcpStream::connect(&gateway.addr).expect("connect");
    let mut partial = TcpStream::connect(&gateway.addr).expect("connect");
    partial
        .write_all(format!("POST {PATH} HTTP/1.1\r\nHost: x\r\n").as_bytes())
        .expect("write");
    for (label, mut stream) in [("silent", silent), ("partial", partial)] {
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let started = Instant::now();
        let mut buf = [0_u8; 64];
        let n = stream.read(&mut buf).unwrap_or(0);
        assert_eq!(n, 0, "{label}: closed with no bytes");
        assert!(started.elapsed() < Duration::from_secs(20), "{label}");
    }

    // Still serving after all of it.
    assert_eq!(status_of(&gateway.exchange(&good())), Some(501));

    // Nothing the gateway printed contains a marker or a key.
    let (stdout, stderr) = gateway.stop();
    markers.assert_clean("stdout", stdout.as_bytes());
    markers.assert_clean("stderr", stderr.as_bytes());
}
