//! Local-leg connection setup cost: what a reused client connection would save compared
//! with one connection per request (ADR 0008, ADR 0024, #42). Measurement scaffolding only.
//! It does not change or enable anything in the gateway and makes no performance claim.
//!
//! The gateway answers every request with `Connection: close` (ADR 0019), so it cannot be
//! asked to reuse a connection, and this tool does not patch it to. It measures instead:
//!
//! - `surrogate`: an in-process HTTP/1.1 server built from the same stack (axum on hyper on
//!   tokio, `http1` only) with a trivial handler that reads the whole body and answers
//!   512 bytes. It is run twice: with keep-alive (client reuses one connection) and with
//!   `Connection: close` on every response (client opens one connection per request, which
//!   is what every client of the gateway does today). The difference is the connection
//!   setup and teardown share of a request for this stack, with none of the gateway's own
//!   work in either arm. That is an upper bound on what reuse could save per request on the
//!   local leg.
//! - `gateway`: the built release gateway binary (separate process, loopback), one
//!   connection per request as served today, for `GET /healthz` only (a `POST` to the chat
//!   route without a provider is refused before its body is read, which is not a
//!   comparable request; the full request path is measured by `qualification/perf`). It
//!   shows how the real per-request time of the smallest request relates to the
//!   surrogate's setup cost. No reuse arm exists for it.
//!
//! Workloads: small (4 KiB) and large (384 KiB) bodies; sequential (1 client) and
//! concurrent (8 clients); per configuration at least `--runs` runs (default 5), runs
//! interleaved across configurations so a slow minute hits all of them. Reported per run:
//! p50/p95/p99 request latency and throughput; per configuration: median and spread (min
//! and max) over runs. Load average and the busiest processes are recorded before and
//! after. Bodies are invented filler.
//!
//! LIMITS: client and surrogate share a process; the gateway arm has its own process but
//! the same host. Loopback has a round trip of about zero, so on a real network the
//! setup share is larger by the extra round trip (the local leg is loopback by default,
//! ADR 0009, so this is the relevant case). A raw HTTP/1.1 client is used so reuse is
//! under control; SDK clients add their own per-request work.
//!
//! ```text
//! cargo build --locked --release && cargo run --locked --release --example keepalive_cost -- [--runs R] [--scale S]
//! ```
#![forbid(unsafe_code)]
// Measurement tool, not shipped code: it may panic on a broken environment.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SMALL: usize = 4 * 1024;
const LARGE: usize = 384 * 1024;

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

fn loadavg() -> String {
    sh("sysctl", &["-n", "vm.loadavg"]).trim().to_owned()
}

fn top_cpu() -> Vec<String> {
    sh("ps", &["-Ao", "pcpu=,comm=", "-r"])
        .lines()
        .take(5)
        .map(|l| {
            let l = l.trim();
            let (cpu, comm) = l.split_once(' ').unwrap_or((l, ""));
            format!("{cpu}% {}", comm.trim().rsplit('/').next().unwrap_or(""))
        })
        .collect()
}

fn arg(name: &str, default: usize) -> usize {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == name {
            return it.next().and_then(|v| v.parse().ok()).unwrap_or(default);
        }
    }
    default
}

// ------------------------------------------------------------------ surrogate server

async fn handler(body: axum::body::Bytes) -> (axum::http::StatusCode, String) {
    let _ = body.len();
    (axum::http::StatusCode::OK, "x".repeat(512))
}

async fn mark_close(mut r: axum::response::Response) -> axum::response::Response {
    r.headers_mut().insert(
        axum::http::header::CONNECTION,
        axum::http::HeaderValue::from_static("close"),
    );
    r
}

async fn start_surrogate(close: bool) -> SocketAddr {
    let mut app = axum::Router::new().route("/echo", axum::routing::post(handler));
    if close {
        app = app.layer(axum::middleware::map_response(mark_close));
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

// ------------------------------------------------------------------ gateway process

fn gateway_binary() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    exe.parent()
        .and_then(std::path::Path::parent)
        .map(|p| p.join("redact-secret-gateway"))
        .expect("target directory")
}

fn start_gateway(dir: &std::path::Path) -> (Child, SocketAddr) {
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        r#"{"schema_version":1,"deployment":{"listener":{"address":"127.0.0.1:0"}},
"content":{"profile":"common"},
"resources":{"capacity":{"receipt":8,"memory_units":262144,"inspection":2,"upstream":8,"stream":8}}}"#,
    )
    .expect("write config");
    let mut child = Command::new(gateway_binary())
        .arg("serve")
        .arg(&config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the gateway (run `cargo build --locked --release` first)");
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

// ------------------------------------------------------------------ raw client

fn body_of(len: usize) -> Arc<Vec<u8>> {
    let json = format!(
        "{{\"model\":\"synthetic-model\",\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]}}",
        "synthetic filler ".repeat(len / 17)
    );
    Arc::new(json.into_bytes())
}

fn request_bytes(method: &str, path: &str, body: &[u8], close: bool) -> Vec<u8> {
    let mut r = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    if close {
        r.push_str("Connection: close\r\n");
    }
    if method == "POST" {
        r.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    r.push_str("\r\n");
    let mut out = r.into_bytes();
    out.extend_from_slice(body);
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read one response; return its status code. Content-Length framing, or read to EOF when
/// the response has no length and says it closes.
async fn read_response(s: &mut TcpStream) -> u16 {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            break p + 4;
        }
        let n = s.read(&mut tmp).await.expect("read head");
        assert!(n > 0, "connection closed before a response head");
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    match len {
        Some(len) => {
            let mut have = buf.len() - head_end;
            while have < len {
                let n = s.read(&mut tmp).await.expect("read body");
                assert!(n > 0, "connection closed inside a body");
                have += n;
            }
        }
        None => while s.read(&mut tmp).await.expect("read to eof") > 0 {},
    }
    status
}

struct Target {
    addr: SocketAddr,
    req: Vec<u8>,
    reuse: bool,
}

/// One client: `n` requests; returns the latency of each and the statuses seen.
async fn client(t: Arc<Target>, n: usize) -> (Vec<u128>, std::collections::BTreeSet<u16>) {
    let mut lat = Vec::with_capacity(n);
    let mut statuses = std::collections::BTreeSet::new();
    let mut conn: Option<TcpStream> = None;
    for _ in 0..n {
        let start = Instant::now();
        let mut s = match conn.take() {
            Some(s) => s,
            None => {
                let s = TcpStream::connect(t.addr).await.expect("connect");
                s.set_nodelay(true).expect("nodelay");
                s
            }
        };
        s.write_all(&t.req).await.expect("write");
        statuses.insert(read_response(&mut s).await);
        lat.push(start.elapsed().as_nanos());
        if t.reuse {
            conn = Some(s);
        }
    }
    (lat, statuses)
}

fn pct(sorted: &[u128], p: f64) -> u128 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

struct Config {
    name: String,
    target: Arc<Target>,
    clients: usize,
    per_client: usize,
}

struct RunResult {
    p50: f64,
    p95: f64,
    p99: f64,
    rps: f64,
    statuses: std::collections::BTreeSet<u16>,
}

async fn run_config(c: &Config) -> RunResult {
    // Warm-up on a fresh set of connections, not recorded.
    let _ = client(Arc::clone(&c.target), 20).await;
    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..c.clients {
        tasks.push(tokio::spawn(client(Arc::clone(&c.target), c.per_client)));
    }
    let mut all = Vec::new();
    let mut statuses = std::collections::BTreeSet::new();
    for t in tasks {
        let (lat, st) = t.await.expect("client task");
        all.extend(lat);
        statuses.extend(st);
    }
    let wall = started.elapsed().as_secs_f64();
    all.sort_unstable();
    RunResult {
        p50: pct(&all, 50.0) as f64 / 1000.0,
        p95: pct(&all, 95.0) as f64 / 1000.0,
        p99: pct(&all, 99.0) as f64 / 1000.0,
        rps: all.len() as f64 / wall,
        statuses,
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let runs = arg("--runs", 5);
    let scale = arg("--scale", 100); // percent of the default iteration counts
    let it = |n: usize| (n * scale / 100).max(10);
    let dir = std::env::temp_dir().join(format!("keepalive-cost-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let ka = start_surrogate(false).await;
    let cl = start_surrogate(true).await;
    let (mut gw, gw_addr) = start_gateway(&dir);

    let small = body_of(SMALL);
    let large = body_of(LARGE);
    let mut configs: Vec<Config> = Vec::new();
    for (label, body, seq_n) in [("small_4KiB", &small, it(3000)), ("large_384KiB", &large, it(300))] {
        for (shape, clients, per) in [("sequential", 1, seq_n), ("concurrent8", 8, seq_n / 4)] {
            let post = |close: bool| request_bytes("POST", "/echo", body, close);
            configs.push(Config {
                name: format!("surrogate_keepalive/{label}/{shape}"),
                target: Arc::new(Target { addr: ka, req: post(false), reuse: true }),
                clients,
                per_client: per,
            });
            configs.push(Config {
                name: format!("surrogate_close/{label}/{shape}"),
                target: Arc::new(Target { addr: cl, req: post(false), reuse: false }),
                clients,
                per_client: per,
            });
        }
    }
    for (shape, clients, per) in [("sequential", 1, it(3000)), ("concurrent8", 8, it(750))] {
        configs.push(Config {
            name: format!("gateway_close_healthz/{shape}"),
            target: Arc::new(Target {
                addr: gw_addr,
                req: request_bytes("GET", "/healthz", b"", false),
                reuse: false,
            }),
            clients,
            per_client: per,
        });
        configs.push(Config {
            name: format!("surrogate_keepalive_healthz_like/{shape}"),
            target: Arc::new(Target {
                addr: ka,
                req: request_bytes("POST", "/echo", b"", false),
                reuse: true,
            }),
            clients,
            per_client: per,
        });
        configs.push(Config {
            name: format!("surrogate_close_healthz_like/{shape}"),
            target: Arc::new(Target {
                addr: cl,
                req: request_bytes("POST", "/echo", b"", false),
                reuse: false,
            }),
            clients,
            per_client: per,
        });
    }

    println!(
        "{}",
        serde_json::json!({
            "record": "header", "tool": "keepalive_cost",
            "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
            "chip": sh("sysctl", &["-n", "machdep.cpu.brand_string"]).trim(),
            "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "source_commit": std::env::var("GATEWAY_COMMIT").ok(),
            "runs": runs, "scale_percent": scale,
            "loadavg_before": loadavg(),
            "uptime_before": sh("uptime", &[]).trim(),
            "top_cpu_before": top_cpu(),
            "note": "surrogate = in-process axum/hyper, trivial handler; gateway = release binary, one connection per request as served today",
        })
    );

    let mut acc: Vec<Vec<RunResult>> = configs.iter().map(|_| Vec::new()).collect();
    for run in 0..runs {
        for (i, c) in configs.iter().enumerate() {
            let r = run_config(c).await;
            if !c.target.reuse {
                // TIME_WAIT lasts 30 s on macOS and the ephemeral range is about 16k wide:
                // pause in proportion to the connections just opened. Not synchronization;
                // the timed section has ended.
                let conns = u64::try_from(c.clients * c.per_client + 20).unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_millis(conns * 3)).await;
            }
            println!(
                "{}",
                serde_json::json!({
                    "record": "run", "config": c.name, "run": run,
                    "clients": c.clients, "requests": c.clients * c.per_client,
                    "p50_us": r.p50, "p95_us": r.p95, "p99_us": r.p99,
                    "throughput_rps": r.rps, "statuses": r.statuses,
                    "loadavg": loadavg(),
                })
            );
            acc[i].push(r);
        }
    }
    for (c, rs) in configs.iter().zip(acc.iter()) {
        let mut p50: Vec<f64> = rs.iter().map(|r| r.p50).collect();
        let mut p95: Vec<f64> = rs.iter().map(|r| r.p95).collect();
        let mut rps: Vec<f64> = rs.iter().map(|r| r.rps).collect();
        let span = |v: &[f64]| {
            let min = v.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            (min, max)
        };
        let (p50_lo, p50_hi) = span(&p50);
        let (rps_lo, rps_hi) = span(&rps);
        println!(
            "{}",
            serde_json::json!({
                "record": "summary", "config": c.name, "runs": rs.len(),
                "clients": c.clients,
                "median_p50_us": median(&mut p50), "p50_min_us": p50_lo, "p50_max_us": p50_hi,
                "median_p95_us": median(&mut p95),
                "median_throughput_rps": median(&mut rps),
                "throughput_min_rps": rps_lo, "throughput_max_rps": rps_hi,
            })
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "record": "footer", "loadavg_after": loadavg(),
            "uptime_after": sh("uptime", &[]).trim(), "top_cpu_after": top_cpu(),
        })
    );
    let _ = gw.kill();
    let _ = gw.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
