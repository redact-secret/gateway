//! Real-binary tests for local caller authentication (#63; ADR 0030,
//! docs/contracts/local-caller-auth.md): startup resolution of the token reference, every
//! failure kind with no echo of the reference or the content, enforcement on the served
//! stack (health stays open, proxy `POST` is refused before anything is read or sent), and
//! restart-only activation. Synthetic data only; nothing here reaches a network. The served
//! requests below are all refused locally: no upstream is reachable from this file and none
//! is contacted.

#![allow(clippy::expect_used, clippy::indexing_slicing)]
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const TOKEN: &str = "SYNTHETIC-startup-local-token-0123456789-abcd";
const TOKEN_LF: &str = "SYNTHETIC-startup-local-token-0123456789-abcd\n";
const OTHER: &str = "SYNTHETIC-startup-local-token-0123456789-abcX";
const KEY: &str = "sk-SYNTHETIC-REVOKED-STARTUP-0000-NOT-A-KEY";
const ENV_NAME: &str = "RSG_SYNTH_LOCAL_TOKEN_63";

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rsg-local-auth-{}-{tag}", std::process::id()));
    fs::create_dir_all(&d).expect("mkdir");
    d
}

fn token_file(d: &Path, name: &str, content: &[u8], mode: u32) -> PathBuf {
    let path = d.join(name);
    fs::write(&path, content).expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

fn config(local_auth: &str, address: &str) -> String {
    format!(
        r#"{{"schema_version": 1,
        "deployment": {{{local_auth}"listener": {{{address}}}}},
        "content": {{"profile": "common"}},
        "resources": {{"capacity": {{"receipt": 2, "memory_units": 4096, "inspection": 1,
        "upstream": 1, "stream": 1}}}}}}"#
    )
}

fn file_auth(path: &Path) -> String {
    format!(
        r#""local_auth": {{"mode": "token", "token": {{"file": "{}"}}}},"#,
        path.display()
    )
}

const ENV_AUTH: &str =
    r#""local_auth": {"mode": "token", "token": {"env": "RSG_SYNTH_LOCAL_TOKEN_63"}},"#;
const LOOPBACK: &str = r#""address": "127.0.0.1:0""#;

fn write_config(d: &Path, doc: &str) -> PathBuf {
    let path = d.join("config.json");
    fs::write(&path, doc).expect("write");
    path
}

fn validate(path: &Path, env_token: Option<&str>) -> (Option<i32>, String, String) {
    let mut cmd = bin();
    cmd.arg("validate-config").arg(path).env_remove(ENV_NAME);
    if let Some(v) = env_token {
        cmd.env(ENV_NAME, v);
    }
    let out = cmd.output().expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_valid_file_or_env_token_validates_and_prints_nothing_derived_from_it() {
    let d = dir("valid");
    for (what, ok_file) in [
        ("mode 0600", token_file(&d, "a", TOKEN.as_bytes(), 0o600)),
        (
            "trailing LF",
            token_file(&d, "b", format!("{TOKEN}\n").as_bytes(), 0o600),
        ),
        (
            "trailing CRLF",
            token_file(&d, "c", format!("{TOKEN}\r\n").as_bytes(), 0o400),
        ),
        (
            "group readable",
            token_file(&d, "d", TOKEN.as_bytes(), 0o640),
        ),
        (
            "group readable and writable",
            token_file(&d, "e", TOKEN.as_bytes(), 0o660),
        ),
    ] {
        let cfg = write_config(&d, &config(&file_auth(&ok_file), LOOPBACK));
        let (code, out, err) = validate(&cfg, None);
        assert_eq!(code, Some(0), "{what}: {err}");
        assert_eq!(out.trim(), "config valid (schema_version 1)", "{what}");
        assert!(err.is_empty(), "{what}");
    }
    // The longest and the shortest token.
    for (n, name) in [(32_usize, "min"), (128, "max")] {
        let f = token_file(&d, name, "a".repeat(n).as_bytes(), 0o600);
        let cfg = write_config(&d, &config(&file_auth(&f), LOOPBACK));
        assert_eq!(validate(&cfg, None).0, Some(0), "{n} bytes");
    }
    let cfg = write_config(&d, &config(ENV_AUTH, LOOPBACK));
    let (code, out, _) = validate(&cfg, Some(TOKEN));
    assert_eq!(code, Some(0));
    assert!(!out.contains("SYNTHETIC"));
}

#[test]
fn every_startup_failure_is_static_bounded_and_echoes_no_reference_or_content() {
    let d = dir("fail");
    let secret_name = "SYNTHETIC-secret-file-name";
    let other_readable = token_file(&d, secret_name, TOKEN.as_bytes(), 0o604);
    let world_writable = token_file(&d, "w", TOKEN.as_bytes(), 0o602);
    let other_exec = token_file(&d, "x", TOKEN.as_bytes(), 0o601);
    let short = token_file(&d, "short", b"SYNTHETIC-too-short", 0o600);
    let long = token_file(&d, "long", "a".repeat(129).as_bytes(), 0o600);
    let bad_alphabet = token_file(
        &d,
        "alpha",
        format!("{}+/=", &TOKEN[..32]).as_bytes(),
        0o600,
    );
    let inner_ws = token_file(
        &d,
        "ws",
        format!("{} {}", &TOKEN[..20], &TOKEN[20..]).as_bytes(),
        0o600,
    );
    let two_lf = token_file(&d, "twolf", format!("{TOKEN}\n\n").as_bytes(), 0o600);
    let empty = token_file(&d, "empty", b"", 0o600);
    let huge = token_file(&d, "huge", "a".repeat(257).as_bytes(), 0o600);
    let at_limit_but_long = token_file(&d, "limit", "a".repeat(256).as_bytes(), 0o600);
    let directory = d.join("a-directory");
    fs::create_dir_all(&directory).expect("mkdir");
    let missing = d.join("does-not-exist");
    let dangling = d.join("dangling");
    symlink(d.join("nowhere"), &dangling).expect("symlink");

    let file_cases: Vec<(&str, &Path, &str)> = vec![
        ("other-readable", &other_readable, "invalid_value"),
        ("other-writable", &world_writable, "invalid_value"),
        ("other-executable", &other_exec, "invalid_value"),
        ("too short", &short, "invalid_value"),
        ("too long (129)", &long, "invalid_value"),
        ("out of alphabet", &bad_alphabet, "invalid_value"),
        ("inner whitespace", &inner_ws, "invalid_value"),
        ("two trailing newlines", &two_lf, "invalid_value"),
        ("empty file", &empty, "invalid_value"),
        ("over 256 bytes", &huge, "invalid_value"),
        (
            "256 bytes, over 128 after stripping",
            &at_limit_but_long,
            "invalid_value",
        ),
        ("directory", &directory, "invalid_value"),
        ("missing", &missing, "unreadable"),
        ("dangling symlink", &dangling, "unreadable"),
    ];
    for (what, path, kind) in file_cases {
        let cfg = write_config(&d, &config(&file_auth(path), LOOPBACK));
        let (code, out, err) = validate(&cfg, None);
        assert_eq!(code, Some(1), "{what}");
        assert_eq!(
            err.trim(),
            format!("error: invalid_config: {kind} at deployment.local_auth.token"),
            "{what}"
        );
        assert!(out.is_empty(), "{what}");
        for leaked in [secret_name, "SYNTHETIC", d.to_string_lossy().as_ref()] {
            assert!(!err.contains(leaked), "{what}: echoed {leaked}");
        }
    }

    // `serve` fails the same way and never binds or announces a listener.
    let cfg = write_config(&d, &config(&file_auth(&other_readable), LOOPBACK));
    let out = bin().arg("serve").arg(&cfg).output().expect("run");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "nothing is announced as listening");

    // Environment sources.
    let cfg = write_config(&d, &config(ENV_AUTH, LOOPBACK));
    for (what, value, kind) in [
        ("unset", None, "unreadable"),
        ("empty", Some(""), "unreadable"),
        ("short", Some("SYNTHETIC-short"), "invalid_value"),
        ("trailing newline", Some(TOKEN_LF), "invalid_value"),
        (
            "bad alphabet",
            Some("SYNTHETIC token with spaces 0123456789abcdef"),
            "invalid_value",
        ),
    ] {
        let (code, out, err) = validate(&cfg, value);
        assert_eq!(code, Some(1), "{what}");
        assert_eq!(
            err.trim(),
            format!("error: invalid_config: {kind} at deployment.local_auth.token"),
            "{what}"
        );
        assert!(out.is_empty());
        assert!(
            !err.contains(ENV_NAME) && !err.contains("SYNTHETIC"),
            "{what}"
        );
    }
}

#[test]
fn a_symlink_to_a_valid_file_is_followed_once_and_the_target_is_checked() {
    let d = dir("symlink");
    let target = token_file(&d, "target", TOKEN.as_bytes(), 0o600);
    let link = d.join("link");
    symlink(&target, &link).expect("symlink");
    let cfg = write_config(&d, &config(&file_auth(&link), LOOPBACK));
    assert_eq!(validate(&cfg, None).0, Some(0));
    // The mode of the target decides, not the link.
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("chmod");
    assert_eq!(validate(&cfg, None).0, Some(1));
}

#[test]
fn a_fifo_is_refused_without_blocking() {
    let d = dir("fifo");
    let fifo = d.join("fifo");
    let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(made.success());
    let cfg = write_config(&d, &config(&file_auth(&fifo), LOOPBACK));
    let started = std::time::Instant::now();
    let (code, _, err) = validate(&cfg, None);
    assert_eq!(code, Some(1));
    assert!(err.contains("invalid_value at deployment.local_auth.token"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no blocking open"
    );
}

#[test]
fn structural_mistakes_are_rejected_before_the_source_is_touched() {
    let d = dir("shape");
    // The file reference is invalid on every line below, and the file it names does not
    // exist: the structural diagnostic (not `unreadable`) must win.
    for (what, auth, kind, loc) in [
        (
            "disabled with token",
            r#""local_auth": {"mode": "disabled", "token": {"env": "A"}},"#,
            "invalid_combination",
            "deployment.local_auth.token",
        ),
        (
            "token mode without token",
            r#""local_auth": {"mode": "token"},"#,
            "missing_field",
            "deployment.local_auth.token",
        ),
        (
            "both sources",
            r#""local_auth": {"mode": "token", "token": {"env": "A", "file": "/nonexistent/x"}},"#,
            "invalid_combination",
            "deployment.local_auth.token",
        ),
        (
            "neither source",
            r#""local_auth": {"mode": "token", "token": {}},"#,
            "invalid_combination",
            "deployment.local_auth.token",
        ),
        (
            "inline value",
            r#""local_auth": {"mode": "token", "token": {"value": "SYNTHETIC-inline-value"}},"#,
            "unknown_field",
            "deployment.local_auth.token",
        ),
        (
            "relative file",
            r#""local_auth": {"mode": "token", "token": {"file": "relative/token"}},"#,
            "invalid_value",
            "deployment.local_auth.token",
        ),
        (
            "bad env name",
            r#""local_auth": {"mode": "token", "token": {"env": "lower-case"}},"#,
            "invalid_value",
            "deployment.local_auth.token",
        ),
        (
            "unknown mode",
            r#""local_auth": {"mode": "bearer"},"#,
            "invalid_value",
            "deployment.local_auth.mode",
        ),
    ] {
        let cfg = write_config(&d, &config(auth, LOOPBACK));
        let (code, _, err) = validate(&cfg, None);
        assert_eq!(code, Some(1), "{what}");
        assert_eq!(
            err.trim(),
            format!("error: invalid_config: {kind} at {loc}"),
            "{what}"
        );
        assert!(!err.contains("SYNTHETIC"), "{what}");
    }
}

#[test]
fn listener_and_token_combinations() {
    let d = dir("combos");
    let good = token_file(&d, "tok", TOKEN.as_bytes(), 0o600);
    let exposed = r#""address": "0.0.0.0:0", "allow_non_loopback": true"#;
    // (auth, address, expected exit)
    for (what, auth, address, ok) in [
        ("loopback, absent", String::new(), LOOPBACK, true),
        (
            "loopback, disabled",
            r#""local_auth": {"mode": "disabled"},"#.to_owned(),
            LOOPBACK,
            true,
        ),
        ("loopback, token", file_auth(&good), LOOPBACK, true),
        ("non-loopback, absent", String::new(), exposed, false),
        (
            "non-loopback, disabled",
            r#""local_auth": {"mode": "disabled"},"#.to_owned(),
            exposed,
            false,
        ),
        ("non-loopback, token", file_auth(&good), exposed, true),
        (
            "non-loopback unacknowledged, token",
            file_auth(&good),
            r#""address": "0.0.0.0:0""#,
            false,
        ),
        (
            "loopback acknowledged, token",
            file_auth(&good),
            r#""address": "127.0.0.1:0", "allow_non_loopback": true"#,
            false,
        ),
    ] {
        let cfg = write_config(&d, &config(&auth, address));
        let (code, _, err) = validate(&cfg, None);
        assert_eq!(code == Some(0), ok, "{what}: {err}");
        if !ok {
            assert!(err.contains("invalid_combination"), "{what}: {err}");
        }
    }
}

// ------------------------------------------------------------------------- served stack

struct Serving {
    child: Child,
    addr: String,
    out: BufReader<std::process::ChildStdout>,
}

fn serve(cfg: &Path) -> Serving {
    let mut child = bin()
        .arg("serve")
        .arg(cfg)
        .env_remove(ENV_NAME)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("listening line");
    let addr = line
        .trim()
        .strip_prefix("listening ")
        .expect("handshake")
        .to_owned();
    Serving { child, addr, out }
}

fn stop(mut s: Serving) -> String {
    let status = Command::new("kill")
        .args(["-TERM", &s.child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
    assert!(s.child.wait().expect("wait").success());
    let mut rest = String::new();
    s.out.read_to_string(&mut rest).expect("rest");
    let mut err = String::new();
    s.child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut err)
        .expect("stderr text");
    format!("{rest}{err}")
}

/// One raw request; returns (status, full response text).
fn http(addr: &str, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    s.write_all(request.as_bytes()).expect("write");
    let mut text = String::new();
    let _ = s.read_to_string(&mut text);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("status");
    (status, text)
}

fn post(addr: &str, key: Option<&str>, extra: &str, body: &str) -> (u16, String) {
    let auth = key.map_or(String::new(), |k| format!("Authorization: Bearer {k}\r\n"));
    http(
        addr,
        &format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             {auth}Content-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        ),
    )
}

fn get(addr: &str, path: &str, extra: &str) -> (u16, String) {
    http(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}\r\n"),
    )
}

fn code_of(text: &str) -> &str {
    text.split(r#""code":""#)
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap_or_default()
}

#[test]
fn the_served_stack_enforces_the_token_and_keeps_probes_open() {
    let d = dir("serve");
    let file = token_file(&d, "tok", TOKEN.as_bytes(), 0o600);
    let cfg = write_config(&d, &config(&file_auth(&file), LOOPBACK));
    let s = serve(&cfg);
    let hdr = |t: &str| format!("X-Gateway-Local-Token: {t}\r\n");

    // Probes and unknown routes need no token and ignore one.
    for extra in [String::new(), hdr(TOKEN), hdr(OTHER)] {
        assert_eq!(get(&s.addr, "/healthz", &extra).0, 200);
        assert_eq!(get(&s.addr, "/readyz", &extra).0, 200);
        assert_eq!(get(&s.addr, "/v1/models", &extra).0, 404);
        // A wrong method on the served route is the route's 405, not an auth result.
        assert_eq!(get(&s.addr, "/v1/chat/completions", &extra).0, 405);
    }
    let (_, health) = get(&s.addr, "/healthz", "");
    let (_, health_with) = get(&s.addr, "/healthz", &hdr(TOKEN));
    assert_eq!(
        health.split("\r\n\r\n").nth(1),
        health_with.split("\r\n\r\n").nth(1),
        "health is identical with and without the token"
    );

    // Proxy POSTs: refused locally before anything is read or sent.
    let big = "{}";
    for (what, key, extra, want_status, want_code) in [
        (
            "no token",
            Some(KEY),
            String::new(),
            401,
            "local_auth_required",
        ),
        (
            "wrong token",
            Some(KEY),
            hdr(OTHER),
            401,
            "local_auth_invalid",
        ),
        (
            "duplicate token",
            Some(KEY),
            format!("{}{}", hdr(TOKEN), hdr(TOKEN)),
            401,
            "local_auth_invalid",
        ),
        ("token only", None, hdr(TOKEN), 401, "missing_credential"),
        // Authenticated, then the ordinary local validation: nothing is forwarded.
        (
            "both credentials, empty object",
            Some(KEY),
            hdr(TOKEN),
            422,
            "unsupported_input",
        ),
    ] {
        let (status, text) = post(&s.addr, key, &extra, big);
        assert_eq!((status, code_of(&text)), (want_status, want_code), "{what}");
        for secret in [TOKEN, OTHER, KEY] {
            assert!(
                !text.contains(secret),
                "{what}: response echoed a credential"
            );
        }
    }
    let output = stop(s);
    for secret in [TOKEN, OTHER, KEY] {
        assert!(
            !output.contains(secret),
            "no token or key in the process output"
        );
    }
}

#[test]
fn activation_is_restart_only() {
    let d = dir("rotate");
    let file = token_file(&d, "tok", TOKEN.as_bytes(), 0o600);
    let cfg = write_config(&d, &config(&file_auth(&file), LOOPBACK));
    let s = serve(&cfg);
    let hdr = |t: &str| format!("X-Gateway-Local-Token: {t}\r\n");
    assert_eq!(post(&s.addr, Some(KEY), &hdr(TOKEN), "{}").0, 422);
    // Rotating the file on disk changes nothing for the running process, in either direction.
    fs::write(&file, OTHER).expect("rotate");
    assert_eq!(post(&s.addr, Some(KEY), &hdr(TOKEN), "{}").0, 422);
    assert_eq!(post(&s.addr, Some(KEY), &hdr(OTHER), "{}").0, 401);
    fs::remove_file(&file).expect("remove");
    assert_eq!(post(&s.addr, Some(KEY), &hdr(TOKEN), "{}").0, 422);
    assert_eq!(get(&s.addr, "/readyz", "").0, 200);
    stop(s);
    // A restart picks up the new value (here: the file is gone, so it refuses to start).
    let out = bin().arg("serve").arg(&cfg).output().expect("run");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn the_env_token_is_read_once_at_startup() {
    let d = dir("env");
    let cfg = write_config(&d, &config(ENV_AUTH, LOOPBACK));
    let mut child = bin()
        .arg("serve")
        .arg(&cfg)
        .env(ENV_NAME, TOKEN)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("listening line");
    let addr = line
        .trim()
        .strip_prefix("listening ")
        .expect("handshake")
        .to_owned();
    let hdr = |t: &str| format!("X-Gateway-Local-Token: {t}\r\n");
    assert_eq!(post(&addr, Some(KEY), &hdr(TOKEN), "{}").0, 422);
    assert_eq!(post(&addr, Some(KEY), &hdr(OTHER), "{}").0, 401);
    assert_eq!(post(&addr, Some(KEY), "", "{}").0, 401);
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    assert!(child.wait().expect("wait").success());
}

#[test]
fn disabled_and_absent_modes_ignore_the_header_and_serve_as_in_alpha() {
    let d = dir("disabled");
    for auth in [
        String::new(),
        r#""local_auth": {"mode": "disabled"},"#.to_owned(),
    ] {
        let cfg = write_config(&d, &config(&auth, LOOPBACK));
        let s = serve(&cfg);
        for extra in [
            String::new(),
            "X-Gateway-Local-Token: SYNTHETIC-ignored\r\n".to_owned(),
            "X-Gateway-Local-Token: a\r\nX-Gateway-Local-Token: b\r\n".to_owned(),
        ] {
            assert_eq!(post(&s.addr, Some(KEY), &extra, "{}").0, 422);
        }
        stop(s);
    }
}

#[test]
fn a_non_loopback_listener_starts_with_a_token_and_still_refuses_unauthenticated_posts() {
    let d = dir("exposed");
    let file = token_file(&d, "tok", TOKEN.as_bytes(), 0o600);
    let exposed = r#""address": "0.0.0.0:0", "allow_non_loopback": true"#;
    let cfg = write_config(&d, &config(&file_auth(&file), exposed));
    let s = serve(&cfg);
    let port = s.addr.rsplit(':').next().expect("port").to_owned();
    let local = format!("127.0.0.1:{port}");
    assert_eq!(post(&local, Some(KEY), "", "{}").0, 401);
    assert_eq!(get(&local, "/healthz", "").0, 200);
    let output = stop(s);
    assert!(
        output.contains("unsupported exposure"),
        "the warning remains"
    );
}
