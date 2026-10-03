//! Command-line interface (the binary's `main` only forwards here).
//!
//! ```text
//! redact-secret-gateway --version | -V
//! redact-secret-gateway --help | -h
//! redact-secret-gateway validate-config <path>
//! redact-secret-gateway serve <path>
//! ```
//!
//! Output never echoes arguments, paths, file content, or configuration values.
//! Exit codes: 0 success, 1 validation/startup/runtime failure, 2 usage error.

use std::ffi::OsString;
use std::io::Read;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{self, SCHEMA_VERSION};
use crate::server::{self, ShutdownSignal, StartupError};

const USAGE: &str = "usage: redact-secret-gateway --version\n       \
redact-secret-gateway validate-config <path>\n       \
redact-secret-gateway serve <path>\n       \
redact-secret-gateway serve-observed <path>\n       \
redact-secret-gateway probe live|ready <loopback-address>";

/// Run the CLI over already-collected arguments (program name excluded).
#[must_use]
pub fn run(args: &[OsString]) -> ExitCode {
    match args {
        [flag] if flag == "--version" || flag == "-V" => print_out(&crate::version_line()),
        [flag] if flag == "--help" || flag == "-h" => print_out(USAGE),
        [cmd, path] if cmd == "validate-config" => validate(Path::new(path)),
        [cmd, path] if cmd == "serve" => serve(Path::new(path), false),
        [cmd, path] if cmd == "serve-observed" => serve(Path::new(path), true),
        [cmd, kind, address] if cmd == "probe" => probe(kind, address),
        _ => {
            // Never echo arguments: they could carry sensitive text.
            print_err(USAGE);
            ExitCode::from(2)
        }
    }
}

/// Distroless exec probe: numeric loopback only, fixed health paths, no config or
/// credentials. One absolute deadline and a 1024-byte response cap bound peers.
fn probe(kind: &OsString, address: &OsString) -> ExitCode {
    let path = match kind.to_str() {
        Some("live") => "/healthz",
        Some("ready") => "/readyz",
        _ => return ExitCode::from(2),
    };
    let Some(addr) = address.to_str().and_then(|s| s.parse::<SocketAddr>().ok()) else {
        return ExitCode::from(2);
    };
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return ExitCode::from(2);
    }
    if probe_health(addr, path).is_ok() {
        ExitCode::SUCCESS
    } else {
        print_err("probe failed");
        ExitCode::FAILURE
    }
}

fn probe_health(addr: SocketAddr, path: &str) -> std::io::Result<()> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .ok_or(std::io::ErrorKind::InvalidInput)?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))
    };
    stream.set_write_timeout(Some(remaining()?))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::with_capacity(1024);
    loop {
        stream.set_read_timeout(Some(remaining()?))?;
        let mut chunk = [0; 128];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if response.len().saturating_add(n) > 1024 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        response.extend_from_slice(chunk.get(..n).ok_or(std::io::ErrorKind::InvalidData)?);
    }
    let text = std::str::from_utf8(&response).map_err(|_| std::io::ErrorKind::InvalidData)?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or(std::io::ErrorKind::InvalidData)?;
    let expected = if path == "/healthz" {
        r#"{"status":"live"}"#
    } else {
        r#"{"status":"ready"}"#
    };
    if head.lines().next() != Some("HTTP/1.1 200 OK") || body != expected {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(())
}

fn print_out(line: &str) -> ExitCode {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_err(line: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{line}");
}

fn fail(e: StartupError) -> ExitCode {
    print_err(&format!("error: {e}"));
    ExitCode::FAILURE
}

fn validate(path: &Path) -> ExitCode {
    match config::load_from_path(path) {
        Ok(_) => print_out(&format!("config valid (schema_version {SCHEMA_VERSION})")),
        Err(e) => fail(e.into()),
    }
}

fn serve(path: &Path, observed: bool) -> ExitCode {
    // Parse and validate once. Nothing below rereads the file.
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
    match runtime.block_on(run_server(plan, observed)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

async fn run_server(plan: Arc<config::RuntimePlan>, observed: bool) -> Result<(), StartupError> {
    let signals = ShutdownSignal::install()?;
    let listener = *plan.deployment().listener();
    if observed && !listener.addr().ip().is_loopback() {
        return Err(StartupError::Bind);
    }
    let bound = server::bind(plan, server::Services::init).await?;
    if listener.non_loopback_acknowledged() {
        print_err("warning: non-loopback listener is unsupported exposure (ADR 0009)");
    }
    let addr = bound.local_addr()?;
    // Stdout line is the readiness handshake for scripts and tests.
    let mut out = std::io::stdout().lock();
    if writeln!(out, "listening {addr}")
        .and_then(|()| out.flush())
        .is_err()
    {
        return Err(StartupError::Serve);
    }
    drop(out);
    if observed {
        bound.serve_observed(signals.recv()).await?;
    } else {
        bound.serve(signals.recv()).await?;
    }
    print_out("shutdown complete");
    Ok(())
}
