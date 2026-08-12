//! Integration test that validates finding #1 against a real `CouchDB` server:
//!
//! Finding #1 — mtime-based conflict detection can silently lose remote edits.
//! `triage_changes` decides between a conflict and a plain local upload using
//! `remote_is_newer(remote_mtime, last_sync_at)`. The remote doc's `mtime` is
//! the *original local file's modification time*, which can be arbitrarily old
//! (tools like `cp -p`, `rsync -t`, `git checkout`, `touch -t` preserve mtime).
//! When two hosts both edit a file and the remote edit carries a stale mtime,
//! the local host treats the remote as "unchanged" and uploads over it —
//! silently discarding the remote edit instead of raising a conflict.
//!
//! This test reproduces exactly that with two real `SyncEngine` clients sharing
//! one `CouchDB` prefix, and asserts a conflict is detected. With the bug it
//! fails (no conflict is reported and the remote edit is clobbered).

use anyhow::Result;
use couchdb_file_sync::models::{IgnoreMatcher, SyncDirPath};
use couchdb_file_sync::{CouchDb, LocalDb, SyncEngine, SyncReport};
use filetime::{FileTime, set_file_mtime};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(prefix: &str) -> Result<Self> {
        let base = env::current_dir()?.join("_testdata");
        fs::create_dir_all(&base)?;
        let path = base.join(format!("{}-{}", prefix, unique_suffix()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    /// The per-client local state DB lives under the (scanner-ignored)
    /// `.couchdb-file-sync/` subdirectory.
    fn db_path(&self) -> PathBuf {
        let dir = self.join(".couchdb-file-sync");
        fs::create_dir_all(&dir).expect("create state dir");
        dir.join("state.db")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Run one full sync cycle for a client rooted at `dir` against the shared
/// `CouchDB` prefix `remote`.
async fn run_sync(
    dir: &TestDir,
    url: &str,
    db: &str,
    user: Option<&str>,
    pass: Option<&str>,
    remote: &str,
) -> Result<SyncReport> {
    let couch = CouchDb::new(url, user, pass, db, remote, 30, 3).await?;
    let local = LocalDb::open(dir.db_path())?;
    let mut engine = SyncEngine::with_ignore(
        couch,
        local,
        SyncDirPath::new(&dir.path).expect("resolve sync dir"),
        IgnoreMatcher::empty(),
    );
    engine.sync().await
}

/// Remove every doc (and its chunks) under `remote` so the shared test DB is
/// left clean for the next run.
async fn cleanup_remote(url: &str, db: &str, user: Option<&str>, pass: Option<&str>, remote: &str) {
    let Ok(couch) = CouchDb::new(url, user, pass, db, remote, 30, 3).await else {
        return;
    };
    let docs = couch.get_all_files().await.unwrap_or_default();
    let chunks: Vec<String> = docs.iter().flat_map(|d| d.children.clone()).collect();
    if !chunks.is_empty() {
        let _ = couch.delete_chunks(&chunks).await;
    }
    for doc in docs {
        let _ = couch.delete_file(&doc.id).await;
    }
}

#[tokio::test]
#[ignore = "requires a running CouchDB server (see COUCHDB_FILE_SYNC_TEST_DB_* env vars)"]
async fn stale_remote_mtime_does_not_mask_conflict() -> Result<()> {
    let (url, db_name, user, pass, _) = test_db_config();
    let remote = format!("conflict-mtime-{}", unique_suffix());

    // Two independent clients (separate dirs + separate state DBs) sharing the
    // same remote prefix — the shape of a real two-machine deployment.
    let dir_a = TestDir::new("conflict-mtime-a")?;
    let dir_b = TestDir::new("conflict-mtime-b")?;

    let test_result: Result<()> = async {
        // Step 1 — A seeds the remote with "hello".
        fs::write(dir_a.join("f.txt"), "hello")?;
        let rep = run_sync(
            &dir_a,
            &url,
            &db_name,
            user.as_deref(),
            pass.as_deref(),
            &remote,
        )
        .await?;
        assert_eq!(rep.uploaded.0, 1, "A uploads f.txt on first sync");
        assert_eq!(rep.conflicts, 0);

        // Step 2 — B pulls it down; this establishes B's stored state with a
        // recent `last_sync_at` and the current revision R1. The first sync
        // bootstraps the existing in-scope remote file set (no checkpoint yet),
        // so f.txt is materialized on B's first sync and a later incremental
        // cycle only pulls changes since the checkpoint.
        let rep = run_sync(
            &dir_b,
            &url,
            &db_name,
            user.as_deref(),
            pass.as_deref(),
            &remote,
        )
        .await?;
        assert_eq!(
            rep.downloaded.0, 1,
            "B downloads f.txt on first sync (bootstrap)"
        );
        assert_eq!(fs::read_to_string(dir_b.join("f.txt"))?, "hello");

        // Step 3 — A edits f.txt but with a STALE/preserved mtime (simulating
        // cp -p / rsync -t / git checkout / touch -t that keep an old mtime).
        // A's upload stores that stale mtime on the remote doc (rev -> R2).
        fs::write(dir_a.join("f.txt"), "hello-A-v2")?;
        let stale = SystemTime::now() - Duration::from_hours(48);
        set_file_mtime(dir_a.join("f.txt"), FileTime::from_system_time(stale))?;
        let rep = run_sync(
            &dir_a,
            &url,
            &db_name,
            user.as_deref(),
            pass.as_deref(),
            &remote,
        )
        .await?;
        assert_eq!(rep.uploaded.0, 1, "A uploads its v2 edit");
        assert_eq!(rep.conflicts, 0, "A's own edit is not a conflict");

        // Step 4 — B edits f.txt locally (mtime now) and syncs. Both sides have
        // now changed to different content and the remote revision differs from
        // B's stored revision.
        fs::write(dir_b.join("f.txt"), "hello-B-v2")?;
        let rep = run_sync(
            &dir_b,
            &url,
            &db_name,
            user.as_deref(),
            pass.as_deref(),
            &remote,
        )
        .await?;

        // Finding #1: this must be a conflict. Because A's remote mtime is
        // stale (older than B's last_sync_at), `remote_is_newer` returns false
        // and B silently uploads its own edit over A's — losing A's change with
        // no conflict and no error.
        assert_eq!(
            rep.conflicts, 1,
            "both sides changed to different content at different revisions must \
             surface a conflict, not a silent local overwrite (finding #1)"
        );

        // The remote edit from A must have survived. With the bug it is gone.
        let couch = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote,
            30,
            3,
        )
        .await?;
        let content = couch
            .get_file_content(&couch.get_remote_path("f.txt"))
            .await?;
        assert_eq!(
            String::from_utf8_lossy(&content),
            "hello-A-v2",
            "A's remote edit must survive a conflict; it must not be silently \
             overwritten by B's local change"
        );

        Ok(())
    }
    .await;

    cleanup_remote(&url, &db_name, user.as_deref(), pass.as_deref(), &remote).await;
    test_result
}

// ── Config helpers (mirror tests/dry_run.rs conventions) ───────────────────

fn test_db_config() -> (String, String, Option<String>, Option<String>, String) {
    let url = env_or_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_URL", "COUCHFS_TEST_DB_URL"],
        "http://localhost:5984",
    );
    let db_name = env_or_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_NAME", "COUCHFS_TEST_DB_NAME"],
        "couchdb_file_sync_conflict_mtime_test",
    );
    let user = env_opt_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_USER", "COUCHFS_TEST_DB_USER"],
        Some("admin"),
    );
    let pass = env_opt_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_PASS", "COUCHFS_TEST_DB_PASS"],
        Some("password"),
    );
    let (user, pass) = match (user, pass) {
        (Some(u), Some(p)) => (Some(u), Some(p)),
        _ => (None, None),
    };
    // remote_path prefix is generated per-run by the caller.
    (url, db_name, user, pass, String::new())
}

fn env_or_first(keys: &[&str], default: &str) -> String {
    env_var_first(keys).unwrap_or_else(|| default.to_string())
}

fn env_var_first(keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Ok(v) = env::var(key) {
            return if v.is_empty() { None } else { Some(v) };
        }
    }
    None
}

fn env_opt_first(keys: &[&str], default: Option<&str>) -> Option<String> {
    env_var_first(keys).or_else(|| default.map(std::string::ToString::to_string))
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}
