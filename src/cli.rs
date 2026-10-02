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
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use crate::config::{self, SCHEMA_VERSION};
use crate::server::{self, ShutdownSignal, StartupError};

const USAGE: &str = "usage: redact-secret-gateway --version\n       \
redact-secret-gateway validate-config <path>\n       \
redact-secret-gateway serve <path>";

/// Run the CLI over already-collected arguments (program name excluded).
#[must_use]
pub fn run(args: &[OsString]) -> ExitCode {
    match args {
        [flag] if flag == "--version" || flag == "-V" => print_out(&crate::version_line()),
        [flag] if flag == "--help" || flag == "-h" => print_out(USAGE),
        [cmd, path] if cmd == "validate-config" => validate(Path::new(path)),
        [cmd, path] if cmd == "serve" => serve(Path::new(path)),
        _ => {
            // Never echo arguments: they could carry sensitive text.
            print_err(USAGE);
            ExitCode::from(2)
        }
    }
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

fn serve(path: &Path) -> ExitCode {
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
    match runtime.block_on(run_server(plan)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}

async fn run_server(plan: Arc<config::RuntimePlan>) -> Result<(), StartupError> {
    let signals = ShutdownSignal::install()?;
    let listener = *plan.deployment().listener();
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
    bound.serve(signals.recv()).await?;
    print_out("shutdown complete");
    Ok(())
}
