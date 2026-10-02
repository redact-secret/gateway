//! Outbound destination policy, seen from outside the crate (issue #23, ADR 0009, ADR 0013).
//!
//! The behavioral tests (redirects, TLS, address policy, proxy environment, fake upstream
//! routing) live next to the code in `src/transport/tests.rs` because the only mechanism
//! that can reach a loopback fake is a `cfg(test)` item. This file proves the other half:
//! from a production build, with any configuration, that mechanism does not exist.
//! `trybuild` regenerate: `TRYBUILD=overwrite cargo test --locked --test destination_policy`.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use redact_secret_gateway::config::{self, Provider};
use redact_secret_gateway::transport::destination::{Origin, OriginError};
use redact_secret_gateway::transport::resolver::is_public_destination;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn caller_controlled_destinations_do_not_typecheck() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/pass_route_id_destination.rs");
    cases.compile_fail("tests/ui/fail_post_with_url_string.rs");
    cases.compile_fail("tests/ui/fail_origin_test_http.rs");
    cases.compile_fail("tests/ui/fail_destination_construct.rs");
}

/// Every test-only seam must sit directly under a `#[cfg(test)]` attribute (or inside the
/// `cfg(test)` module), so it is absent from the library and binary in every build.
#[test]
fn test_only_seams_are_cfg_test_gated_in_source() {
    const SEAMS: &[&str] = &["for_test_http", "for_test_https", "PublicOrLoopback"];
    // `Scheme::Http` as a whole token (not a prefix of `Scheme::Https`).
    let is_http_variant = |line: &str| {
        line.trim() == "Http,"
            || line.match_indices("Scheme::Http").any(|(i, m)| {
                !line
                    .get(i.saturating_add(m.len())..)
                    .is_some_and(|r| r.starts_with('s'))
            })
    };
    let mut files = Vec::new();
    files_under(&root().join("src"), &mut files);
    let mut found = 0_u32;
    for path in files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
    {
        if path.ends_with("src/transport/tests.rs") {
            continue; // the cfg(test) module itself, declared `#[cfg(test)] mod tests;`
        }
        let text = fs::read_to_string(path).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//")
                || !(SEAMS.iter().any(|s| line.contains(s)) || is_http_variant(line))
            {
                continue;
            }
            // Inside `mod tests` (everything after the last `#[cfg(test)]\nmod tests`) is fine.
            let in_test_mod = lines
                .iter()
                .take(i)
                .rposition(|l| l.trim() == "mod tests {")
                .is_some();
            // Otherwise the item (fn/variant/arm) must be annotated within the preceding lines.
            let annotated = lines
                .iter()
                .take(i.saturating_add(1))
                .rev()
                .take(4)
                .any(|l| l.trim() == "#[cfg(test)]");
            let is_decl_or_use = code.starts_with("#[cfg(test)]");
            assert!(
                in_test_mod || annotated || is_decl_or_use,
                "{}:{} test seam without #[cfg(test)]: {}",
                path.display(),
                i.saturating_add(1),
                code
            );
            found = found.saturating_add(1);
        }
    }
    assert!(found > 0, "scan must actually see the seams");
}

#[test]
fn no_cargo_feature_or_build_script_can_enable_a_test_upstream() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("manifest");
    assert!(
        !manifest.contains("[features]"),
        "no cargo features exist, so none can enable a test upstream"
    );
    assert!(!root().join("build.rs").exists());
    // Release, CI, container, and smoke tooling never selects features.
    let mut files = Vec::new();
    for dir in [".github", "scripts", "container"] {
        files_under(&root().join(dir), &mut files);
    }
    for path in files {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in [
            "--features",
            "--all-features",
            "--no-default-features",
            "RUSTFLAGS",
        ] {
            // The compile-time test seam is cfg(test) only; no tool may pass cfgs either.
            if needle == "RUSTFLAGS" {
                assert!(!text.contains("--cfg"), "{} passes --cfg", path.display());
            } else {
                assert!(!text.contains(needle), "{} uses {needle}", path.display());
            }
        }
    }
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_redact-secret-gateway"))
}

fn write_config(name: &str, upstream: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rsg-dest-policy-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(name);
    let text = format!(
        r#"{{"schema_version": 1,
        "deployment": {{"listener": {{"address": "127.0.0.1:0"}}, "upstream": {upstream}}},
        "content": {{"profile": "common"}},
        "resources": {{"capacity": {{"receipt": 1, "memory_units": 1, "inspection": 1,
        "upstream": 1, "stream": 1}}}}}}"#
    );
    fs::write(&path, text).expect("write");
    path
}

#[test]
fn real_binary_accepts_only_a_reviewed_provider_and_no_test_or_tls_toggles() {
    let ok = write_config("ok.json", r#"{"provider": "openai"}"#);
    let out = bin().arg("validate-config").arg(&ok).output().expect("run");
    assert!(out.status.success(), "reviewed provider must validate");

    for (name, upstream) in [
        (
            "a.json",
            r#"{"provider": "openai", "test_upstream": "http://127.0.0.1:1"}"#,
        ),
        (
            "b.json",
            r#"{"provider": "openai", "origin": "http://127.0.0.1:1"}"#,
        ),
        (
            "c.json",
            r#"{"provider": "openai", "insecure_skip_verify": true}"#,
        ),
        ("d.json", r#"{"provider": "openai", "allow_http": true}"#),
        (
            "e.json",
            r#"{"provider": "openai", "proxy": "http://127.0.0.1:3128"}"#,
        ),
        ("f.json", r#"{"provider": "http://127.0.0.1:1"}"#),
        ("g.json", r#"{"provider": "fake"}"#),
        ("h.json", r#"{"provider": "test"}"#),
        ("i.json", r#"{"provider": "loopback"}"#),
    ] {
        let path = write_config(name, upstream);
        let out = bin()
            .arg("validate-config")
            .arg(&path)
            .output()
            .expect("run");
        assert!(!out.status.success(), "{name} must be rejected");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!text.contains("127.0.0.1:1"), "diagnostic echoed a value");
    }
}

#[test]
fn environment_cannot_enable_a_test_upstream_in_the_real_binary() {
    let ok = write_config("env.json", r#"{"provider": "openai"}"#);
    let out = bin()
        .arg("validate-config")
        .arg(&ok)
        .env("REDACT_SECRET_GATEWAY_TEST_UPSTREAM", "http://127.0.0.1:1")
        .env("GATEWAY_TEST_UPSTREAM", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .expect("run");
    // Validation is unchanged by the environment and no variable name is read by the code.
    assert!(out.status.success());
    let mut files = Vec::new();
    files_under(&root().join("src"), &mut files);
    for path in files
        .iter()
        .filter(|p| !p.ends_with("src/transport/tests.rs"))
    {
        let text = fs::read_to_string(path).expect("read");
        assert!(
            !text.contains("std::env::var") && !text.contains("env::var_os"),
            "{} reads the environment",
            path.display()
        );
    }
}

#[test]
fn public_origin_and_address_policy_from_outside() {
    assert!(Origin::parse("https://api.openai.com").is_ok());
    assert_eq!(
        Origin::parse("http://api.openai.com"),
        Err(OriginError::Scheme)
    );
    assert_eq!(Origin::parse("https://127.0.0.1"), Err(OriginError::Host));
    assert_eq!(
        Origin::parse("https://api.openai.com@127.0.0.1"),
        Err(OriginError::Userinfo)
    );
    assert!(!is_public_destination(
        "169.254.169.254".parse().expect("ip")
    ));
    assert!(is_public_destination("8.8.8.8".parse().expect("ip")));
    assert_eq!(Provider::from_name("openai"), Some(Provider::OpenAi));
    assert_eq!(Provider::from_name("OpenAI"), None);
    // The public config API has no way to carry an origin.
    let doc = r#"{"schema_version":1,"deployment":{"listener":{"address":"127.0.0.1:0"},
      "upstream":{"provider":"openai","origin":"https://x.test"}},"content":{"profile":"common"},
      "resources":{"capacity":{"receipt":1,"memory_units":1,"inspection":1,"upstream":1,"stream":1}}}"#;
    assert!(config::parse(doc.as_bytes()).is_err());
}
