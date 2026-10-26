//! Subprocess tests for configuration loading and fail-closed behavior.
//!
//! These spawn the real binary and assert that an explicit config that is
//! missing, malformed, or contains an invalid type/enum exits nonzero before
//! any command dispatch, while an absent optional default config still permits
//! documented defaults and the shipped template loads cleanly.

use std::path::PathBuf;
use std::process::Command;

/// The exact template that `couchdb-file-sync install` ships (embedded verbatim
/// into the binary via `include_str!`). Integration tests run with the package
/// root as the working directory.
const SHIPPED_TEMPLATE: &str = "couchdb-file-sync.yaml.example";

const fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_couchdb-file-sync")
}

/// A fresh scratch dir used as a sync path for `init` (which needs no `CouchDB`).
fn scratch_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("spawn couchdb-file-sync binary")
}

#[test]
fn missing_explicit_config_exits_nonzero() {
    let missing = PathBuf::from("/nonexistent/couchdb-file-sync.yaml");
    let out = run(&["--config", missing.to_str().unwrap(), "status"]);
    assert!(
        !out.status.success(),
        "missing explicit config must fail closed; status={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn malformed_yaml_exits_nonzero() {
    let dir = scratch_dir();
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "couchdb:\n  url: [unclosed\n").expect("write bad yaml");
    let out = run(&["--config", bad.to_str().unwrap(), "status"]);
    assert!(
        !out.status.success(),
        "malformed YAML must fail closed; status={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn invalid_enum_value_exits_nonzero() {
    let dir = scratch_dir();
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "conflicts:\n  default_strategy: bogus\n").expect("write bad enum");
    let out = run(&["--config", bad.to_str().unwrap(), "status"]);
    assert!(
        !out.status.success(),
        "invalid enum value must fail closed; status={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn missing_default_config_permits_defaults() {
    // Point HOME at an empty dir so no default config file exists, then `init`
    // a fresh path: config must fall back to defaults and the command must run.
    let home = scratch_dir();
    let sync = scratch_dir();
    let out = Command::new(binary())
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .arg("init")
        .arg(sync.path())
        .output()
        .expect("spawn with empty HOME");
    assert!(
        out.status.success(),
        "absent optional default config must permit defaults; status={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn shipped_template_loads() {
    // A copy of the exact shipped template named config.yaml must load without
    // a config error; `init` needs no CouchDB so it exercises dispatch.
    let dir = scratch_dir();
    let config = dir.path().join("config.yaml");
    std::fs::copy(SHIPPED_TEMPLATE, &config).expect("copy shipped template");
    let sync = dir.path().join("sync");
    let out = run(&[
        "--config",
        config.to_str().unwrap(),
        "init",
        sync.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "shipped template must load; status={:?} stdout={} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
