//! Per-connection cost measurement for the connection bound (ADR 0008, ADR 0022, #40).
//! Measurement scaffolding only: it records, as JSON lines, the file descriptors, resident
//! memory, and threads the gateway process holds while it keeps N idle or slow-head
//! connections open. It makes no performance claim and prints no payloads.
//!
//! ```text
//! cargo build --locked --release
//! cargo run --locked --release --example conn_cost
//! ```
//!
//! It starts the built `redact-secret-gateway` binary (same profile as the example) on a
//! loopback port with a very large `max_connections`, opens the connections from this
//! process, and samples the gateway process from outside with `ps` and `lsof` (or `/proc`
//! on Linux). The gateway serves on one thread, and axum spawns one task per connection,
//! so tasks per connection is 1 by construction; it is not observable from outside.
//! Client-side descriptors are in this process, not the gateway's.
//!
//! Phases per connection count: `baseline` (no connection), `idle` (connected, nothing
//! sent), `fat_head` (60 KiB of an unfinished request head sent, the most the head guard
//! holds per connection is 64 KiB). Set `GATEWAY_COMMIT` to record the source commit.
#![forbid(unsafe_code)]
// Measurement tool, not shipped code: it may panic on a broken environment.
#![allow(clippy::expect_used, clippy::arithmetic_side_effects)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const COUNTS: [usize; 3] = [64, 256, 1024];
const HEAD_BYTES: usize = 60 * 1024;

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    String::from_utf8(out.stdout).ok()
}

fn rss_kb(pid: u32) -> Option<u64> {
    sh("ps", &["-o", "rss=", "-p", &pid.to_string()])?
        .trim()
        .parse()
        .ok()
}

fn fds(pid: u32) -> Option<usize> {
    if let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        return Some(dir.count());
    }
    // lsof prints one header line; subtract it.
    let out = sh("lsof", &["-n", "-P", "-p", &pid.to_string()])?;
    Some(out.lines().count().saturating_sub(1))
}

fn threads(pid: u32) -> Option<usize> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        return status
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .and_then(|n| n.trim().parse().ok());
    }
    let out = sh("ps", &["-M", "-p", &pid.to_string()])?;
    Some(out.lines().count().saturating_sub(1))
}

fn loadavg() -> String {
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        return s.trim().to_owned();
    }
    sh("sysctl", &["-n", "vm.loadavg"])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn nofile_limit() -> String {
    sh("sh", &["-c", "ulimit -n"])
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn gateway_binary() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    // target/<profile>/examples/conn_cost -> target/<profile>/redact-secret-gateway
    exe.parent()
        .and_then(std::path::Path::parent)
        .map(|p| p.join("redact-secret-gateway"))
        .expect("target directory")
}

fn start(config: &std::path::Path) -> (Child, std::net::SocketAddr) {
    let mut child = Command::new(gateway_binary())
        .arg("serve")
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the gateway (run `cargo build --locked` first)");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("listening line");
    let addr = line
        .trim()
        .strip_prefix("listening ")
        .and_then(|a| a.parse().ok())
        .expect("listening <addr>");
    (child, addr)
}

fn probe(addr: std::net::SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect(addr) else {
        return false;
    };
    let _ = s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let mut buf = String::new();
    let _ = std::io::Read::read_to_string(&mut s, &mut buf);
    buf.starts_with("HTTP/1.1 200")
}

fn line(phase: &str, n: usize, pid: u32, base: (u64, usize)) -> serde_json::Value {
    let rss = rss_kb(pid).unwrap_or(0);
    let fd = fds(pid).unwrap_or(0);
    let held = u64::try_from(n).unwrap_or(1).max(1);
    serde_json::json!({
        "phase": phase,
        "connections": n,
        "gateway_rss_kb": rss,
        "gateway_fds": fd,
        "gateway_threads": threads(pid),
        "rss_delta_kb": rss.saturating_sub(base.0),
        "fd_delta": fd.saturating_sub(base.1),
        "rss_bytes_per_connection": rss.saturating_sub(base.0).saturating_mul(1024) / held,
    })
}

fn main() {
    let dir = std::env::temp_dir().join(format!("conn-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        r#"{"schema_version":1,"deployment":{"listener":{"address":"127.0.0.1:0"}},
"content":{"profile":"common"},
"resources":{"capacity":{"receipt":8,"memory_units":65536,"inspection":2,"upstream":8,"stream":8},
"limits":{"max_connections":65536,"body_deadline_ms":300000}}}"#,
    )
    .expect("write config");

    let commit = std::env::var("GATEWAY_COMMIT").ok();
    println!(
        "{}",
        serde_json::json!({
            "record": "header",
            "tool": "conn_cost",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "source_commit": commit,
            "loadavg_before": loadavg(),
            "ulimit_n": nofile_limit(),
            "note": "point-in-time samples from one run; not a bound, not tuned",
        })
    );

    for n in COUNTS {
        for fat in [false, true] {
            let (mut child, addr) = start(&config);
            let pid = child.id();
            assert!(probe(addr), "gateway answers before measurement");
            let base = (rss_kb(pid).unwrap_or(0), fds(pid).unwrap_or(0));
            println!("{}", line("baseline", 0, pid, base));
            let mut held = Vec::new();
            for _ in 0..n {
                let mut s = TcpStream::connect(addr).expect("connect");
                if fat {
                    let mut head = b"GET /healthz HTTP/1.1\r\nX-Pad: ".to_vec();
                    head.resize(HEAD_BYTES, b'a');
                    s.write_all(&head).expect("send partial head");
                }
                held.push(s);
            }
            // The probe is accepted after every held connection (backlog order), so once
            // it is answered the server has taken them all; the extra descriptor of the
            // probe is closed by the time the sample is taken.
            assert!(probe(addr), "gateway still answers with {n} held");
            let end = Instant::now() + Duration::from_secs(10);
            while fds(pid).unwrap_or(0) > base.1 + n && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
            println!(
                "{}",
                line(if fat { "fat_head" } else { "idle" }, n, pid, base)
            );
            drop(held);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    println!(
        "{}",
        serde_json::json!({"record": "footer", "loadavg_after": loadavg()})
    );
    let _ = std::fs::remove_dir_all(&dir);
}
