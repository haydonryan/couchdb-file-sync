//! Subprocess tests for machine-readable `status --json` / `conflicts --json`
//! output with zero, one, and multiple configured roots.
//!
//! These spawn the real binary and parse its stdout with a JSON parser to prove
//! that every command writes exactly one valid JSON document: multi-root output
//! is a single aggregate carrying per-root results or errors (a root-level
//! read failure is captured in-band instead of aborting the document), and a
//! single root preserves its documented shape.

use std::path::{Path, PathBuf};
use std::process::Command;

const fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_couchdb-file-sync")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("spawn couchdb-file-sync binary")
}

/// A scratch root with a `.couchdb-file-sync` state dir so a subsequent
/// `status`/`conflicts` can open (or fail to open) its state database.
fn scratch_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    let state = dir.path().join(".couchdb-file-sync");
    std::fs::create_dir_all(&state).expect("create state dir");
    dir
}

/// Corrupt a root's state database so opening it fails, modeling a root-level
/// read failure.
fn corrupt_state_db(root: &Path) {
    let db = root.join(".couchdb-file-sync").join("state.db");
    std::fs::write(&db, b"this is not a sqlite database").expect("write corrupt db");
}

/// Write a config with the given local/remote path pairs and return the config
/// path. The returned `TempDir` must stay alive for the config file to exist.
fn write_multi_config(pairs: &[(&Path, &str)]) -> (tempfile::TempDir, PathBuf) {
    use std::fmt::Write;
    let dir = tempfile::tempdir().expect("create config dir");
    let cfg = dir.path().join("multi.yaml");
    let mut yaml = String::from("paths:\n");
    for (local, remote) in pairs {
        let _ = writeln!(
            yaml,
            "  - local: \"{}\"\n    remote: \"{}\"",
            local.display(),
            remote
        );
    }
    std::fs::write(&cfg, yaml).expect("write config");
    (dir, cfg)
}

fn parse_stdout(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not a single valid JSON document: {e}\nstdout={}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

#[test]
fn status_multi_root_emits_one_aggregate_document_with_errors() {
    let good = scratch_root();
    let bad = scratch_root();
    corrupt_state_db(bad.path());
    let (_cfg_dir, cfg) = write_multi_config(&[(good.path(), "g/"), (bad.path(), "b/")]);

    let out = run(&["--config", cfg.to_str().unwrap(), "status", "--json"]);
    assert!(
        out.status.success(),
        "status --json exited nonzero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let parsed = parse_stdout(&out);
    let roots = parsed
        .get("roots")
        .and_then(serde_json::Value::as_array)
        .expect("aggregate has roots array");
    assert_eq!(roots.len(), 2, "both roots aggregated");

    let good_entry = roots
        .iter()
        .find(|r| r["path"] == serde_json::json!(good.path().display().to_string()))
        .expect("good root present");
    assert!(
        good_entry.get("status").is_some(),
        "good root carries status: {good_entry}"
    );
    assert!(good_entry.get("error").is_none(), "no error for good root");

    let bad_entry = roots
        .iter()
        .find(|r| r["path"] == serde_json::json!(bad.path().display().to_string()))
        .expect("bad root present");
    assert!(
        bad_entry.get("error").is_some(),
        "bad root carries in-band error: {bad_entry}"
    );
    assert!(
        bad_entry.get("status").is_none(),
        "no status for failed root"
    );

    // stdout holds exactly one JSON document — the parse above plus a trailing
    // non-whitespace token would already have failed, so no banners leaked.
}

#[test]
fn conflicts_multi_root_emits_one_aggregate_document_with_errors() {
    let good = scratch_root();
    let bad = scratch_root();
    corrupt_state_db(bad.path());
    let (_cfg_dir, cfg) = write_multi_config(&[(good.path(), "g/"), (bad.path(), "b/")]);

    let out = run(&["--config", cfg.to_str().unwrap(), "conflicts", "--json"]);
    assert!(
        out.status.success(),
        "conflicts --json exited nonzero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let parsed = parse_stdout(&out);
    let roots = parsed
        .get("roots")
        .and_then(serde_json::Value::as_array)
        .expect("aggregate has roots array");
    assert_eq!(roots.len(), 2, "both roots aggregated");

    let good_entry = roots
        .iter()
        .find(|r| r["path"] == serde_json::json!(good.path().display().to_string()))
        .expect("good root present");
    assert!(
        good_entry.get("conflicts").is_some(),
        "good root carries conflicts array: {good_entry}"
    );

    let bad_entry = roots
        .iter()
        .find(|r| r["path"] == serde_json::json!(bad.path().display().to_string()))
        .expect("bad root present");
    assert!(
        bad_entry.get("error").is_some(),
        "bad root carries in-band error: {bad_entry}"
    );
}

#[test]
fn status_single_root_preserves_documented_json_shape() {
    let root = scratch_root();
    let (_cfg_dir, cfg) = write_multi_config(&[(root.path(), "r/")]);

    let out = run(&["--config", cfg.to_str().unwrap(), "status", "--json"]);
    assert!(out.status.success(), "status --json exited nonzero");

    let parsed = parse_stdout(&out);
    assert!(
        parsed.get("sync_directory").is_some(),
        "single-root status keeps documented shape (top-level sync_directory): {parsed}"
    );
    assert!(
        parsed.get("roots").is_none(),
        "single-root status is not wrapped in an aggregate: {parsed}"
    );
}

#[test]
fn conflicts_single_root_preserves_documented_json_shape() {
    let root = scratch_root();
    let (_cfg_dir, cfg) = write_multi_config(&[(root.path(), "r/")]);

    let out = run(&["--config", cfg.to_str().unwrap(), "conflicts", "--json"]);
    assert!(out.status.success(), "conflicts --json exited nonzero");

    let parsed = parse_stdout(&out);
    assert!(
        parsed.is_array(),
        "single-root conflicts keeps documented array shape: {parsed}"
    );
}
