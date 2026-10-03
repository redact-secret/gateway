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
    // The one reviewed environment read: the local caller token reference
    // (`deployment.local_auth.token.env`, #63) is resolved once at startup by this file. The
    // variable name comes only from validated configuration, the value is only ever a token,
    // and nothing about a destination, proxy, or TLS rule is read from the environment.
    let local_auth = fs::read_to_string(root().join("src/transport/local_auth.rs")).expect("read");
    let prod = local_auth
        .split("#[cfg(test)]\nmod tests")
        .next()
        .expect("production part");
    assert_eq!(prod.matches("std::env::var_os").count(), 1);
    assert!(!prod.contains("std::env::var("));
    for path in files.iter().filter(|p| {
        !p.ends_with("src/transport/tests.rs") && !p.ends_with("src/transport/local_auth.rs")
    }) {
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

// ---------------------------------------------------------------------------------------
// Qualification test build (ADR 0020, issue #22).
//
// SDK qualification needs a gateway whose upstream is a local fake provider. That path exists
// ONLY in a separate, non-release crate generated by `qualification/build.sh` from a copy of
// `src/` plus `qualification/seam.patch` and `qualification/overlay/*`. Nothing in the shipped
// package, its sources, features, lockfile, CI, candidate workflow, scripts, or container gains a
// seam; every scan above is unchanged. The tests below make the isolation itself testable:
// an explicit allowlist of the only non-document files that may contain the seam markers, a test
// that the allowlist is exactly the decided set, a repository-wide scan, a scan of the shipped
// binary's bytes, and checks of the dedicated workflow.
// ---------------------------------------------------------------------------------------

/// Markers of the qualification seam. Built from fragments so this file (and the checker script)
/// never contains them whole and so never matches its own scan.
fn seam_markers() -> [String; 4] {
    [
        ["RSG-QUALIFICATION", "-BUILD-NOT-FOR-RELEASE"].concat(),
        ["redact-secret-gateway", "-qualification"].concat(),
        ["--fake", "-provider"].concat(),
        ["fake", "_provider"].concat(),
    ]
}

/// The ONLY non-document files that may contain a seam marker. Relaxation of the "no test
/// upstream anywhere" policy is limited to exactly these paths (ADR 0020, decision 3). A change
/// here is a deliberate maintainer decision and must update the ADR and
/// `qualification_allowlist_is_exactly_the_decided_set` in the same change.
const QUALIFICATION_SEAM_ALLOWLIST: &[&str] = &[
    ".github/workflows/beta2.yml",
    "qualification/sidecar/run.py",
    ".github/workflows/qualification.yml",
    "qualification/build.sh",
    "qualification/run-suites.sh",
    "qualification/seam.patch",
    "qualification/perf/run.mjs",
    "qualification/overlay/main.rs",
    "qualification/overlay/qualification.rs",
    "qualification/overlay/transport_qualification.rs",
];

/// Directories never scanned: build output, caches, dependency trees, and the generated crate.
fn is_ignored_dir(path: &Path) -> bool {
    let rel = path.strip_prefix(root()).unwrap_or(path);
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name,
        ".git" | "target" | "graft" | "node_modules" | ".venv" | ".claude"
    ) || rel == Path::new("qualification/gen")
        || rel == Path::new("qualification/evidence")
}

fn repo_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                if !is_ignored_dir(&path) {
                    walk(&path, out);
                }
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&root(), &mut out);
    out
}

fn rel(path: &Path) -> String {
    path.strip_prefix(root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn qualification_allowlist_is_exactly_the_decided_set() {
    // Written out a second time on purpose: widening the allowlist fails here until the
    // decision is made visibly (and the ADR is updated), not by editing one constant.
    let decided: [&str; 10] = [
        ".github/workflows/beta2.yml",
        "qualification/sidecar/run.py",
        ".github/workflows/qualification.yml",
        "qualification/build.sh",
        "qualification/run-suites.sh",
        "qualification/seam.patch",
        "qualification/perf/run.mjs",
        "qualification/overlay/main.rs",
        "qualification/overlay/qualification.rs",
        "qualification/overlay/transport_qualification.rs",
    ];
    assert_eq!(QUALIFICATION_SEAM_ALLOWLIST, decided.as_slice());
    for entry in QUALIFICATION_SEAM_ALLOWLIST {
        assert!(
            !entry.contains(['*', '?', '[', '{']) && !entry.ends_with('/') && !entry.contains(".."),
            "{entry}: an allowlist entry is one exact file, never a pattern or a directory"
        );
        assert!(root().join(entry).is_file(), "{entry} must exist");
        for shipped in ["src/", "container/", "scripts/", "tests/", "examples/"] {
            assert!(
                !entry.starts_with(shipped),
                "{entry}: shipped paths are never allowlisted"
            );
        }
        assert!(
            !matches!(
                *entry,
                ".github/workflows/ci.yml"
                    | ".github/workflows/artifacts.yml"
                    | "Cargo.toml"
                    | "Cargo.lock"
                    | "rust-toolchain.toml"
                    | "deny.toml"
            ),
            "{entry}: release, CI, and build configuration are never allowlisted"
        );
    }
}

#[test]
fn qualification_seam_markers_exist_only_in_the_allowlisted_files() {
    let markers = seam_markers();
    let mut found = Vec::new();
    for path in repo_files() {
        let name = rel(&path);
        // Documents may describe the decision; code, config, scripts, and workflows may not.
        if path.extension().is_some_and(|e| e == "md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue; // binary or non-UTF-8: not source
        };
        if markers.iter().any(|m| text.contains(m.as_str())) {
            found.push(name);
        }
    }
    found.sort();
    let mut allowed: Vec<String> = QUALIFICATION_SEAM_ALLOWLIST
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    allowed.sort();
    let outside: Vec<&String> = found.iter().filter(|f| !allowed.contains(f)).collect();
    assert!(
        outside.is_empty(),
        "seam markers outside the allowlist: {outside:?}"
    );
    assert!(
        !found.is_empty(),
        "the scan must actually see the qualification files"
    );
}

#[test]
fn seam_patch_touches_only_the_decided_source_files_and_opens_nothing_in_the_shipped_tree() {
    let patch = fs::read_to_string(root().join("qualification/seam.patch")).expect("patch");
    let mut targets: Vec<&str> = patch
        .lines()
        .filter_map(|l| l.strip_prefix("+++ b/"))
        .collect();
    targets.sort_unstable();
    assert_eq!(
        targets,
        [
            "src/lib.rs",
            "src/server.rs",
            "src/transport.rs",
            "src/transport/destination.rs",
            "src/transport/resolver.rs",
        ]
    );
    // The shipped tree is untouched: the test-only constructors are still `cfg(test)` there.
    let destination =
        fs::read_to_string(root().join("src/transport/destination.rs")).expect("read");
    assert!(destination.contains("#[cfg(test)]\n    pub(crate) fn for_test_http"));
    let mut sources = Vec::new();
    files_under(&root().join("src"), &mut sources);
    for path in sources {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for marker in seam_markers() {
            assert!(
                !text.contains(marker.as_str()),
                "{} carries a qualification marker",
                path.display()
            );
        }
    }
}

#[test]
fn the_overlay_is_loopback_only_and_labels_the_build() {
    let overlay =
        fs::read_to_string(root().join("qualification/overlay/transport_qualification.rs"))
            .expect("overlay");
    assert!(
        overlay.contains("is_loopback()"),
        "the fake address must be refused unless it is loopback"
    );
    assert!(overlay.contains(&seam_markers()[0]));
    let entry =
        fs::read_to_string(root().join("qualification/overlay/qualification.rs")).expect("entry");
    assert!(entry.contains("is_loopback()"));
    assert!(
        entry.contains(&seam_markers()[0]),
        "the banner labels the build"
    );
    // Real destinations stay untouched: the overlay never edits the reviewed origin table or
    // the production address policy.
    assert!(!overlay.contains("REVIEWED_HOSTS"));
    assert!(!overlay.contains("is_public_destination"));
}

#[test]
fn shipped_manifest_has_no_features_workspace_or_second_package() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("manifest");
    assert!(!manifest.contains("[features]"));
    assert!(!manifest.contains("[workspace"));
    assert!(!manifest.contains("[patch"));
    assert_eq!(
        manifest.matches("[[bin]]").count(),
        1,
        "exactly one shipped binary target"
    );
    assert!(manifest.contains("name = \"redact-secret-gateway\""));
    assert!(!manifest.contains(&seam_markers()[1]));
    assert!(!root().join("build.rs").exists());
    // The generated qualification crate is its own root, never a member of the product, and
    // no manifest for it is committed (it is generated from the shipped one).
    assert!(!root().join("qualification/Cargo.toml").exists());
    assert!(root().join("qualification/build.sh").exists());
}

#[test]
fn the_shipped_binary_contains_no_qualification_marker_or_seam_symbol() {
    let bytes = fs::read(env!("CARGO_BIN_EXE_redact-secret-gateway")).expect("binary");
    let mut seams: Vec<String> = seam_markers().to_vec();
    // Names of the test-only constructors: present only in the generated crate's binary.
    seams.extend(
        ["for_test_http", "for_test_https", "PublicOrLoopback"]
            .iter()
            .map(|s| (*s).to_owned()),
    );
    for needle in seams {
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "the shipped binary contains `{needle}`"
        );
    }
}

#[test]
fn qualification_workflow_is_isolated_and_never_uploads_a_binary() {
    let wf =
        fs::read_to_string(root().join(".github/workflows/qualification.yml")).expect("workflow");
    // Minimal permissions, no secrets, no persisted credentials, SHA-pinned actions.
    assert!(wf.contains("permissions: {}"), "default-deny permissions");
    assert!(!wf.contains("secrets."), "no secrets");
    assert!(!wf.contains("id-token"), "no OIDC token");
    assert!(!wf.contains("packages: write") && !wf.contains("contents: write"));
    for line in wf
        .lines()
        .filter(|l| l.trim_start().starts_with("uses:") || l.trim_start().starts_with("- uses:"))
    {
        let spec = line
            .trim_start()
            .trim_start_matches("- ")
            .trim_start_matches("uses:")
            .trim();
        let (action, rest) = spec.split_once('@').expect("pinned");
        let sha = rest.split_whitespace().next().expect("ref");
        assert!(
            sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "{action} must be pinned to a full commit SHA"
        );
    }
    let checkouts = wf.matches("actions/checkout@").count();
    assert!(checkouts > 0);
    assert_eq!(
        wf.matches("persist-credentials: false").count(),
        checkouts,
        "every checkout drops credentials"
    );
    // Only evidence text is uploaded: never a binary, never under a candidate name.
    let mut uploads = 0_u32;
    for (i, line) in wf.lines().enumerate() {
        if line.trim_start().starts_with("path:") {
            uploads = uploads.saturating_add(1);
            let value = line.trim_start().trim_start_matches("path:").trim();
            assert!(
                value.starts_with("qualification/evidence"),
                "line {}: uploads must be evidence files only, found `{value}`",
                i.saturating_add(1)
            );
        }
    }
    assert!(uploads > 0, "the workflow uploads its evidence");
    assert!(!wf.contains("skeleton-candidate") && !wf.contains("stage-"));
    assert!(
        !wf.contains("gen/target"),
        "the test binary is never uploaded"
    );

    // The release/candidate workflow and ordinary CI know nothing about the test build.
    for name in ["artifacts.yml", "ci.yml"] {
        let text =
            fs::read_to_string(root().join(".github/workflows").join(name)).expect("workflow");
        assert!(
            !text.contains("qualification/") && !text.contains("qualification-"),
            "{name} must not reference the qualification build"
        );
    }
    // Container and candidate scripts never mention the seam (the one script that names the
    // markers, to prove their absence, builds them from fragments).
    for dir in ["container", "scripts"] {
        let mut files = Vec::new();
        files_under(&root().join(dir), &mut files);
        for path in files {
            let text = fs::read_to_string(&path).unwrap_or_default();
            for m in seam_markers() {
                assert!(
                    !text.contains(m.as_str()),
                    "{} mentions the seam",
                    path.display()
                );
            }
        }
    }
}
