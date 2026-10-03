//! Provider-leg connection setup cost: a standalone loopback microbenchmark (ADR 0008,
//! ADR 0024, #42). Measurement scaffolding only. It is NOT the gateway, does not touch the
//! product or the qualification seam, and makes no performance claim.
//!
//! It stands up a loopback TLS server (a throwaway CA and leaf from `rcgen`, `tokio-rustls`
//! with the `aws-lc-rs` provider the gateway also uses) and a client built from the same
//! crates, then measures, one request at a time, how long a small request/response exchange
//! takes when it must first set up a connection compared with when it reuses one:
//!
//! - `tcp_fresh`: TCP connect, one exchange, close.
//! - `tcp_reuse`: one exchange on an open TCP connection.
//! - `tls13_fresh`: TCP connect, full TLS 1.3 handshake, one exchange, close.
//! - `tls13_resumed`: as above, but the client offers a session ticket from an earlier
//!   connection (what a shared client session store would do).
//! - `tls12_fresh`: TCP connect, full TLS 1.2 handshake, one exchange, close.
//! - `tls_reuse`: one exchange on an open TLS 1.3 connection.
//!
//! LIMITS, stated so the numbers are not over-read: client and server run in one process on
//! one loopback interface, so a figure is the CPU time of BOTH ends plus scheduling, with a
//! round trip of about zero. On a real path the network adds one round trip for TCP and one
//! (TLS 1.3) or two (TLS 1.2) for the handshake, which this tool cannot measure; use
//! `extra_round_trips` with a path latency you know. The certificate is a short ECDSA P-256
//! chain and the exchange is 256 bytes each way; a real provider sends a longer chain, so
//! its verification costs more. The reqwest/hyper client the gateway uses adds its own
//! per-connection work that is not included here.
//!
//! ```text
//! cargo run --locked --release --example tls_handshake_cost -- [--iterations N] [--runs R]
//! ```
//!
//! Output: JSON lines (header, one `run` line per configuration and run, one `summary` line
//! per configuration with the median and spread over runs, footer). Load average and the top
//! CPU consumers are recorded before and after. Nothing here is secret: the keys are
//! generated in memory and discarded.
#![forbid(unsafe_code)]
// Measurement tool, not shipped code: it may panic on a broken environment.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const MSG: usize = 256;

fn sh(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
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
    // Command names only (no arguments), the five busiest processes.
    sh("ps", &["-Ao", "pcpu=,comm=", "-r"])
        .lines()
        .take(5)
        .map(|l| {
            let l = l.trim();
            let (cpu, comm) = l.split_once(' ').unwrap_or((l, ""));
            let name = comm.trim().rsplit('/').next().unwrap_or("");
            format!("{cpu}% {name}")
        })
        .collect()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

struct Pki {
    ca_der: Vec<u8>,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

fn make_pki() -> Pki {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(vec![]).expect("params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "synthetic measurement CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .expect("params")
        .signed_by(&leaf_key, &issuer)
        .expect("leaf");
    Pki {
        ca_der: ca_cert.der().to_vec(),
        cert_der: leaf.der().to_vec(),
        key_der: leaf_key.serialize_der(),
    }
}

fn client_config(pki: &Pki, versions: &[&'static rustls::SupportedProtocolVersion]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(pki.ca_der.clone()))
        .expect("root");
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

async fn serve_conn<S>(mut s: S)
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut buf = [0u8; MSG];
    let reply = [b'r'; MSG];
    while s.read_exact(&mut buf).await.is_ok() {
        if s.write_all(&reply).await.is_err() || s.flush().await.is_err() {
            break;
        }
    }
}

async fn start_server(pki: &Pki, tls: bool) -> SocketAddr {
    let cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(pki.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pki.key_der.clone())),
        )
        .expect("server config");
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                break;
            };
            let _ = tcp.set_nodelay(true);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if tls {
                    if let Ok(t) = acceptor.accept(tcp).await {
                        serve_conn(t).await;
                    }
                } else {
                    serve_conn(tcp).await;
                }
            });
        }
    });
    addr
}

async fn exchange<S>(s: &mut S)
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let req = [b'q'; MSG];
    let mut buf = [0u8; MSG];
    s.write_all(&req).await.expect("write");
    s.flush().await.expect("flush");
    s.read_exact(&mut buf).await.expect("read");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    TcpFresh,
    TcpReuse,
    Tls13Fresh,
    Tls13Resumed,
    Tls12Fresh,
    TlsReuse,
}

impl Mode {
    const ALL: [Mode; 6] = [
        Mode::TcpReuse,
        Mode::TcpFresh,
        Mode::TlsReuse,
        Mode::Tls13Fresh,
        Mode::Tls13Resumed,
        Mode::Tls12Fresh,
    ];
    fn name(self) -> &'static str {
        match self {
            Mode::TcpFresh => "tcp_fresh",
            Mode::TcpReuse => "tcp_reuse",
            Mode::Tls13Fresh => "tls13_fresh",
            Mode::Tls13Resumed => "tls13_resumed",
            Mode::Tls12Fresh => "tls12_fresh",
            Mode::TlsReuse => "tls_reuse",
        }
    }
    /// Extra network round trips a fresh connection costs on a real path, before the
    /// request can be sent (TCP handshake 1; TLS 1.3 adds 1, TLS 1.2 adds 2, resumed
    /// TLS 1.3 adds 1). Zero for a reused connection. Stated, not measured.
    fn extra_round_trips(self) -> u32 {
        match self {
            Mode::TcpFresh => 1,
            Mode::Tls13Fresh | Mode::Tls13Resumed => 2,
            Mode::Tls12Fresh => 3,
            Mode::TcpReuse | Mode::TlsReuse => 0,
        }
    }
}

/// A fresh connection leaves a TIME_WAIT entry (30 s on macOS) and the ephemeral port range
/// is about 16k wide, so fresh-connection modes are followed by a pause proportional to the
/// connections they opened. Not synchronization: it runs after the timed section ends.
async fn pace(mode: Mode, iterations: usize) {
    if matches!(mode, Mode::TcpReuse | Mode::TlsReuse) {
        return;
    }
    let conns = u64::try_from(iterations + 50).unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(conns * 3)).await;
}

fn pct(sorted: &[u128], p: f64) -> u128 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

async fn run_mode(
    mode: Mode,
    plain: SocketAddr,
    tls: SocketAddr,
    c13: &Arc<ClientConfig>,
    c12: &Arc<ClientConfig>,
    iterations: usize,
) -> Vec<u128> {
    let name = ServerName::try_from("localhost").expect("name");
    let mut samples = Vec::with_capacity(iterations);
    let warm = 50;
    match mode {
        Mode::TcpReuse | Mode::TlsReuse => {
            let tcp = TcpStream::connect(if mode == Mode::TcpReuse { plain } else { tls })
                .await
                .expect("connect");
            tcp.set_nodelay(true).expect("nodelay");
            if mode == Mode::TcpReuse {
                let mut s = tcp;
                for i in 0..(iterations + warm) {
                    let t = Instant::now();
                    exchange(&mut s).await;
                    if i >= warm {
                        samples.push(t.elapsed().as_nanos());
                    }
                }
            } else {
                let mut s = TlsConnector::from(Arc::clone(c13))
                    .connect(name, tcp)
                    .await
                    .expect("tls");
                for i in 0..(iterations + warm) {
                    let t = Instant::now();
                    exchange(&mut s).await;
                    if i >= warm {
                        samples.push(t.elapsed().as_nanos());
                    }
                }
            }
        }
        Mode::TcpFresh => {
            for i in 0..(iterations + warm) {
                let t = Instant::now();
                let mut s = TcpStream::connect(plain).await.expect("connect");
                s.set_nodelay(true).expect("nodelay");
                exchange(&mut s).await;
                let e = t.elapsed().as_nanos();
                drop(s);
                if i >= warm {
                    samples.push(e);
                }
            }
        }
        Mode::Tls13Fresh | Mode::Tls13Resumed | Mode::Tls12Fresh => {
            let cfg = if mode == Mode::Tls12Fresh { c12 } else { c13 };
            // Fresh: a new client config per run is NOT used; instead resumption is turned
            // off for the "fresh" modes so every handshake is a full one.
            let cfg = if mode == Mode::Tls13Resumed {
                Arc::clone(cfg)
            } else {
                let mut c = (**cfg).clone();
                c.resumption = rustls::client::Resumption::disabled();
                Arc::new(c)
            };
            let conn = TlsConnector::from(cfg);
            let mut resumed_seen = 0usize;
            for i in 0..(iterations + warm) {
                let t = Instant::now();
                let tcp = TcpStream::connect(tls).await.expect("connect");
                tcp.set_nodelay(true).expect("nodelay");
                let mut s = conn.connect(name.clone(), tcp).await.expect("tls");
                exchange(&mut s).await;
                let e = t.elapsed().as_nanos();
                if matches!(
                    s.get_ref().1.handshake_kind(),
                    Some(rustls::HandshakeKind::Resumed)
                ) {
                    resumed_seen += 1;
                }
                drop(s);
                if i >= warm {
                    samples.push(e);
                }
            }
            if mode == Mode::Tls13Resumed && resumed_seen == 0 {
                eprintln!("note: no resumed handshake observed for tls13_resumed");
            }
            if mode != Mode::Tls13Resumed && resumed_seen != 0 {
                eprintln!("note: unexpected resumption in a fresh mode");
            }
        }
    }
    samples
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

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let iterations = arg("--iterations", 2000);
    let runs = arg("--runs", 5);
    let pki = make_pki();
    let plain = start_server(&pki, false).await;
    let tls = start_server(&pki, true).await;
    let c13 = client_config(&pki, &[&rustls::version::TLS13]);
    let c12 = client_config(&pki, &[&rustls::version::TLS12]);

    println!(
        "{}",
        serde_json::json!({
            "record": "header",
            "tool": "tls_handshake_cost",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "chip": sh("sysctl", &["-n", "machdep.cpu.brand_string"]).trim(),
            "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
            "source_commit": std::env::var("GATEWAY_COMMIT").ok(),
            "iterations_per_run": iterations,
            "runs": runs,
            "message_bytes_each_way": MSG,
            "loadavg_before": loadavg(),
            "uptime_before": sh("uptime", &[]).trim(),
            "top_cpu_before": top_cpu(),
            "note": "client and server share one process and one loopback; microseconds are both ends' CPU plus scheduling, round trip about zero",
        })
    );

    let mut per_mode: Vec<(Mode, Vec<u128>)> = Mode::ALL.iter().map(|m| (*m, vec![])).collect();
    // One discarded pass first: the first mode otherwise pays for cold thread placement.
    for (mode, _) in &per_mode {
        let _ = run_mode(*mode, plain, tls, &c13, &c12, iterations / 4).await;
    }
    for run in 0..runs {
        // Interleave the modes within each run so a slow minute hits all of them alike.
        for (mode, medians) in &mut per_mode {
            let mut s = run_mode(*mode, plain, tls, &c13, &c12, iterations).await;
            pace(*mode, iterations).await;
            s.sort_unstable();
            let (p50, p95, p99) = (pct(&s, 50.0), pct(&s, 95.0), pct(&s, 99.0));
            medians.push(p50);
            println!(
                "{}",
                serde_json::json!({
                    "record": "run", "mode": mode.name(), "run": run,
                    "n": s.len(),
                    "p50_us": p50 as f64 / 1000.0,
                    "p95_us": p95 as f64 / 1000.0,
                    "p99_us": p99 as f64 / 1000.0,
                    "loadavg": loadavg(),
                })
            );
        }
    }
    for (mode, mut medians) in per_mode {
        medians.sort_unstable();
        let median = medians[medians.len() / 2];
        println!(
            "{}",
            serde_json::json!({
                "record": "summary", "mode": mode.name(), "runs": medians.len(),
                "median_of_p50_us": median as f64 / 1000.0,
                "min_p50_us": medians[0] as f64 / 1000.0,
                "max_p50_us": medians[medians.len() - 1] as f64 / 1000.0,
                "extra_round_trips_on_a_real_path": mode.extra_round_trips(),
            })
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "record": "footer",
            "loadavg_after": loadavg(),
            "uptime_after": sh("uptime", &[]).trim(),
            "top_cpu_after": top_cpu(),
        })
    );
    // Let spawned connection tasks drain before exit.
    tokio::time::sleep(Duration::from_millis(50)).await;
}
