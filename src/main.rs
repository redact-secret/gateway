//! `redact-secret-gateway` executable. Scaffold only: it reports its version and exits.
//! Configuration validation and serving belong to later issues (#4, #18).
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let extra = args.next();
    match (first.as_deref(), extra) {
        (Some(flag), None) if flag == OsStr::new("--version") || flag == OsStr::new("-V") => {
            let mut out = std::io::stdout().lock();
            if writeln!(out, "{}", redact_secret_gateway::version_line()).is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        _ => {
            // Never echo arguments: they could carry sensitive text.
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "usage: redact-secret-gateway --version");
            ExitCode::from(2)
        }
    }
}
