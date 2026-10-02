//! `redact-secret-gateway-qualification`: the NON-RELEASE test build (ADR 0020).
//! RSG-QUALIFICATION-BUILD-NOT-FOR-RELEASE. Never ship, upload as a candidate, or publish.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    redact_secret_gateway::qualification::run(&args)
}
