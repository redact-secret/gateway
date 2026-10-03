//! QUALIFICATION BUILD ONLY (ADR 0020). RSG-QUALIFICATION-BUILD-NOT-FOR-RELEASE.
//!
//! Entry point of the non-release test binary `redact-secret-gateway-qualification`. It is
//! generated from the shipped source plus `qualification/seam.patch` by
//! `qualification/build.sh`; this file is not part of the shipped crate.
//!
//! ```text
//! redact-secret-gateway-qualification --version
//! redact-secret-gateway-qualification validate-config <path>
//! redact-secret-gateway-qualification serve <path> --fake-provider <loopback-ip:port>
//! ```
//!
//! `serve` runs the production startup and request path unchanged, except that the reviewed
//! route's destination is the loopback fake provider (plain HTTP, loopback only). A
//! loopback-only metrics listener prints its address on stdout (`qualification-metrics
//! <addr>`) and serves a JSON snapshot of the in-process stage counters, for the synthetic
//! timing and memory measurements. It carries no payload, header, or credential data.

use std::ffi::OsString;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::admission::Admission;
use crate::config::{self, RuntimePlan};
use crate::server::{self, Services, ShutdownSignal, StartupError};
use crate::telemetry::{Metrics, Stage, StreamEnd};
use crate::transport::Upstream;

/// Printed in the banner and the version line so the binary can never be mistaken for the
/// product. The shipped binaries must not contain it (checked in CI and by tests).
pub const MARKER: &str = "RSG-QUALIFICATION-BUILD-NOT-FOR-RELEASE";

const USAGE: &str = "usage: redact-secret-gateway-qualification --version\n       \
redact-secret-gateway-qualification validate-config <path>\n       \
redact-secret-gateway-qualification serve <path> --fake-provider <loopback-ip:port>";

/// Run over already-collected arguments (program name excluded).
#[must_use]
pub fn run(args: &[OsString]) -> ExitCode {
    match args {
        [flag] if flag == "--version" || flag == "-V" => {
            println!(
                "redact-secret-gateway-qualification {} (core redact-secret {}) [{MARKER}]",
                env!("CARGO_PKG_VERSION"),
                crate::core_bridge::PINNED_CORE_VERSION
            );
            ExitCode::SUCCESS
        }
        [cmd, _] if cmd == "validate-config" => crate::cli::run(args),
        [cmd, _, _] if cmd == "probe" => crate::cli::run(args),
        [cmd, path, flag, addr]
            if (cmd == "serve" || cmd == "serve-observed") && flag == "--fake-provider" =>
        {
            let Some(addr) = addr.to_str().and_then(|a| a.parse::<SocketAddr>().ok()) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            serve(Path::new(path), addr, cmd == "serve-observed")
        }
        _ => {
            // Never echo arguments: they could carry sensitive text.
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn fail(e: StartupError) -> ExitCode {
    eprintln!("error: {e}");
    ExitCode::FAILURE
}

fn serve(path: &Path, fake: SocketAddr, observed: bool) -> ExitCode {
    if !fake.ip().is_loopback() {
        eprintln!("error: the fake provider must be a loopback address");
        return ExitCode::from(2);
    }
    let plan = match config::load_from_path(path) {
        Ok(plan) => Arc::new(plan),
        Err(e) => return fail(e.into()),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return fail(StartupError::Init),
    };
    match runtime.block_on(run_server(plan, fake, observed)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

async fn run_server(
    plan: Arc<RuntimePlan>,
    fake: SocketAddr,
    operations_enabled: bool,
) -> Result<(), StartupError> {
    let signals = ShutdownSignal::install()?;
    let metrics = Arc::new(Metrics::new());
    let for_init = Arc::clone(&metrics);
    // Occupancy observer for the load measurements (#58): counts only, no request data.
    let observed: Arc<Mutex<Option<Arc<Admission>>>> = Arc::new(Mutex::new(None));
    let observed_init = Arc::clone(&observed);
    let bound = server::bind(plan, move |plan| {
        let upstream =
            Upstream::from_plan_with_fake_provider(plan, fake).map_err(|_| StartupError::Init)?;
        let services = Services::init_with(plan, upstream, for_init)?;
        if let Ok(mut slot) = observed_init.lock() {
            *slot = Some(services.admission());
        }
        Ok(services)
    })
    .await?;
    eprintln!(
        "{MARKER}: QUALIFICATION BUILD, not the product; the provider route is a loopback fake"
    );
    let addr = bound.local_addr()?;
    let metrics_listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| StartupError::Bind)?;
    let metrics_addr = metrics_listener
        .local_addr()
        .map_err(|_| StartupError::Bind)?;
    {
        let mut out = std::io::stdout().lock();
        if writeln!(out, "qualification-metrics {metrics_addr}")
            .and_then(|()| writeln!(out, "listening {addr}"))
            .and_then(|()| out.flush())
            .is_err()
        {
            return Err(StartupError::Serve);
        }
    }
    let metrics_task = tokio::spawn(serve_metrics(metrics_listener, metrics, observed));
    let result = if operations_enabled {
        bound.serve_observed(signals.recv()).await
    } else {
        bound.serve(signals.recv()).await
    };
    metrics_task.abort();
    result?;
    println!("shutdown complete");
    Ok(())
}

fn stage_json(metrics: &Metrics, stage: Stage) -> Value {
    let s = metrics.stage(stage);
    let hist: Vec<Value> = metrics
        .histogram(stage)
        .into_iter()
        .map(|(upper, n)| json!([upper, n]))
        .collect();
    json!({"count": s.count, "total_us": s.total_micros, "max_us": s.max_micros, "hist": hist})
}

fn admission_json(observed: &Mutex<Option<Arc<Admission>>>) -> Value {
    let load = observed
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|a| (a.load(), a.memory_total_units())));
    match load {
        Some((l, memory_total)) => json!({
            "receipt_in_use": l.receipt_in_use,
            "memory_units_in_use": l.memory_units_in_use,
            "memory_units_total": memory_total,
            "inspection_in_use": l.inspection_in_use,
            "upstream_in_use": l.upstream_in_use,
            "stream_in_use": l.stream_in_use,
            "waiting": l.waiting,
        }),
        None => Value::Null,
    }
}

fn snapshot(metrics: &Metrics, observed: &Mutex<Option<Arc<Admission>>>) -> Value {
    let ends = [
        ("completed", StreamEnd::Completed),
        ("upstream_error", StreamEnd::UpstreamError),
        ("idle_timeout", StreamEnd::IdleTimeout),
        ("lifetime_exceeded", StreamEnd::LifetimeExceeded),
        ("buffer_exceeded", StreamEnd::BufferExceeded),
        ("shutdown", StreamEnd::Shutdown),
        ("abandoned", StreamEnd::Abandoned),
    ];
    let mut stream_ends = serde_json::Map::new();
    for (name, end) in ends {
        stream_ends.insert(name.to_owned(), json!(metrics.streams_ended(end)));
    }
    json!({
        "stages": {
            "admission_wait": stage_json(metrics, Stage::AdmissionWait),
            "parse": stage_json(metrics, Stage::Parse),
            "inspection": stage_json(metrics, Stage::Inspection),
            "serialization": stage_json(metrics, Stage::Serialization),
            "upstream_first_response": stage_json(metrics, Stage::UpstreamFirstResponse),
            "upstream_total": stage_json(metrics, Stage::UpstreamTotal),
            "stream_first_byte": stage_json(metrics, Stage::StreamFirstByte),
            "stream_total": stage_json(metrics, Stage::StreamTotal),
            "stream_upstream_wait": stage_json(metrics, Stage::StreamUpstreamWait),
            "stream_downstream_wait": stage_json(metrics, Stage::StreamDownstreamWait),
        },
        "upstream_attempts": metrics.upstream_attempts(),
        "streams_started": metrics.streams_started(),
        "streams_ended": stream_ends,
        "stream_bytes": metrics.stream_bytes(),
        "stream_buffered_now": metrics.stream_buffered(),
        "stream_buffered_peak": metrics.stream_buffered_peak(),
        "admission": admission_json(observed),
        "rss_peak_kb": peak_rss_kb(),
        "marker": MARKER,
    })
}

/// Process peak resident memory (`VmHWM`) where `/proc` exists; `null` elsewhere (the
/// measurement driver samples the process from outside on those hosts).
fn peak_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse().ok())
}

async fn serve_metrics(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    observed: Arc<Mutex<Option<Arc<Admission>>>>,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0_u8; 1024];
        let _ = socket.read(&mut buf).await;
        let body = snapshot(&metrics, &observed).to_string();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body.as_bytes()).await;
        let _ = socket.shutdown().await;
    }
}
