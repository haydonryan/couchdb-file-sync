//! Subprocess tests for the root-level sync ownership lock.
//!
//! These spawn the real binary and assert that independent mutating processes
//! (`daemon`, which is the first thing to acquire and hold a root lock) are
//! serialized per canonical root: a second mutator on the same root is blocked
//! while the first holds the lock, an unrelated root stays concurrent, a path
//! spelling that canonicalizes to the same root contends on the same lock, and
//! the abrupt death of the owner releases the lock so a waiting mutator
//! proceeds.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_couchdb-file-sync")
}

/// How long the first daemon may take to acquire the root lock and start.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// A short window in which a blocked mutator must NOT have started.
const BLOCKED_WINDOW: Duration = Duration::from_secs(2);
/// How long a waiter may take to acquire the lock after the owner dies.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// Spawn a poll-mode daemon on `root` and return it plus a channel feeding its
/// stdout lines. Poll mode is used so the daemon acquires and holds the root
/// lock for its lifetime without needing a live `CouchDB` to stay alive.
fn spawn_daemon(root: &Path) -> (Child, Receiver<String>) {
    let mut child = Command::new(binary())
        .args([
            "daemon",
            root.to_str().expect("root is valid UTF-8"),
            "--interval",
            "100000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let stdout = child.stdout.take().expect("daemon stdout piped");
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

/// Whether a line containing `needle` arrives on `rx` within `timeout`.
fn wait_for_line(rx: &Receiver<String>, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains(needle) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// Stop a daemon, tolerating an already-exited child.
fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn mutating_processes_serialize_per_root_and_owner_death_releases() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root_a = dir.path().join("rootA");
    let root_b = dir.path().join("rootB");
    std::fs::create_dir_all(&root_a).expect("create rootA");
    std::fs::create_dir_all(&root_b).expect("create rootB");

    // First mutator on rootA acquires and holds the lock.
    let (daemon_a1, rx_a1) = spawn_daemon(&root_a);
    assert!(
        wait_for_line(&rx_a1, "daemon started", START_TIMEOUT),
        "first daemon should acquire the root lock and start"
    );

    // A second mutator on the same root is blocked while the first holds it.
    let (daemon_a2, rx_a2) = spawn_daemon(&root_a);
    assert!(
        !wait_for_line(&rx_a2, "daemon started", BLOCKED_WINDOW),
        "second mutator on the same root must be blocked by the held lock"
    );

    // An unrelated root stays concurrent: its own lock is immediately free.
    let (daemon_b, rx_b) = spawn_daemon(&root_b);
    assert!(
        wait_for_line(&rx_b, "daemon started", START_TIMEOUT),
        "an unrelated root must remain concurrent"
    );

    // Abruptly kill the first owner; the OS releases its lock and the blocked
    // waiter proceeds.
    stop(daemon_a1);
    assert!(
        wait_for_line(&rx_a2, "daemon started", RELEASE_TIMEOUT),
        "after the owner dies the lock is released and the waiter proceeds"
    );

    stop(daemon_a2);
    stop(daemon_b);
}

#[test]
fn canonical_root_aliases_contend_on_one_lock() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("create root");
    // A path spelling that canonicalizes to the same root (`root/.`).
    let alias = root.join(".");

    let (daemon1, rx1) = spawn_daemon(&root);
    assert!(
        wait_for_line(&rx1, "daemon started", START_TIMEOUT),
        "first daemon should acquire the root lock and start"
    );

    let (daemon2, rx2) = spawn_daemon(&alias);
    assert!(
        !wait_for_line(&rx2, "daemon started", BLOCKED_WINDOW),
        "a canonical-root alias must contend on the same lock"
    );

    stop(daemon1);
    assert!(
        wait_for_line(&rx2, "daemon started", RELEASE_TIMEOUT),
        "the alias waiter proceeds once the owner exits"
    );
    stop(daemon2);
}
