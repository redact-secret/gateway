//! Startup/version smoke test against the real binary.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
}

#[test]
fn version_flag_prints_versions_and_exits_cleanly() {
    let out = bin().arg("--version").output().expect("run binary");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.starts_with("redact-secret-gateway "));
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
    assert!(stdout.contains("redact-secret 0.1.0-beta.12"));
    assert!(out.stderr.is_empty());
}

#[test]
fn unknown_arguments_fail_without_echoing_them() {
    let out = bin().arg("SYNTHETIC-ARG").output().expect("run binary");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("SYNTHETIC-ARG"));
    assert!(out.stdout.is_empty());
}

#[test]
fn no_arguments_is_a_usage_error() {
    let out = bin().output().expect("run binary");
    assert_eq!(out.status.code(), Some(2));
}
