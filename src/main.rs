//! `redact-secret-gateway` executable: `--version`, `validate-config <path>`, and
//! `serve <path>` (loopback health skeleton; no proxy routes yet). See `cli`.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    redact_secret_gateway::cli::run(&args)
}
