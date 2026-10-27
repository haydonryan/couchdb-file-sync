//! End-to-end subprocess test for logging lifecycle.
//!
//! A short-lived process with file logging enabled must flush its buffered
//! diagnostics to the configured log file on exit, and a `~/...` log path must
//! be expanded (spaces preserved) rather than written as a literal `~`
//! directory.

use std::process::Command;

const fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_couchdb-file-sync")
}

#[test]
fn short_lived_process_flushes_log_and_expands_tilde_path() {
    let home = tempfile::tempdir().expect("temp home");
    let dir = tempfile::tempdir().expect("temp dir");
    let sync_root = dir.path().join("root");
    std::fs::create_dir_all(&sync_root).expect("create sync root");

    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        "couchdb:\n  url: \"http://127.0.0.1:1\"\n  database: test_db\n  remote_path: \"\"\n  timeout_seconds: 1\n  retry_attempts: 1\nlogging:\n  file: \"~/My Logs/sync.log\"\n  format: compact\n",
    )
    .expect("write config");

    // The sync cannot reach CouchDB, but "Running sync in" is logged before
    // any connection attempt. The process exits (successfully or not) and the
    // owned logging guard must have flushed that line to the file.
    let status = Command::new(binary())
        .env("HOME", home.path())
        .args([
            "-c",
            config_path.to_str().expect("config path"),
            "sync",
            sync_root.to_str().expect("sync root"),
        ])
        .status()
        .expect("spawn couchdb-file-sync binary");

    // A closed port refuses immediately; the run must terminate on its own.
    let _ = status;

    let log_path = home.path().join("My Logs").join("sync.log");
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("log file at {log_path:?} missing after exit: {e}"));
    assert!(
        log.contains("Running sync in"),
        "expected flushed 'Running sync in' in log, got: {log:?}"
    );
    // The literal `~` directory must not have been created.
    assert!(
        !home.path().join("~").exists(),
        "literal ~ directory must not be created"
    );
}

#[test]
fn json_format_writes_parseable_log_lines() {
    let home = tempfile::tempdir().expect("temp home");
    let dir = tempfile::tempdir().expect("temp dir");
    let sync_root = dir.path().join("root");
    std::fs::create_dir_all(&sync_root).expect("create sync root");

    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        "couchdb:\n  url: \"http://127.0.0.1:1\"\n  database: test_db\n  remote_path: \"\"\n  timeout_seconds: 1\n  retry_attempts: 1\nlogging:\n  file: \"sync.log\"\n  format: json\n",
    )
    .expect("write config");

    let status = Command::new(binary())
        .env("HOME", home.path())
        .args([
            "-c",
            config_path.to_str().expect("config path"),
            "sync",
            sync_root.to_str().expect("sync root"),
        ])
        .status()
        .expect("spawn couchdb-file-sync binary");
    let _ = status;

    // Relative `file` path resolves against the working directory of the test
    // subprocess (the package root).
    let log_path = std::path::Path::new("sync.log");
    let log = std::fs::read_to_string(log_path)
        .unwrap_or_else(|e| panic!("log file at {log_path:?} missing after exit: {e}"));

    let mut parsed = 0;
    for line in log.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("expected JSON log line, got {line:?}: {e}"));
        parsed += 1;
    }
    assert!(
        parsed > 0,
        "json log must contain at least one record, got: {log:?}"
    );
    std::fs::remove_file(log_path).ok();
}
