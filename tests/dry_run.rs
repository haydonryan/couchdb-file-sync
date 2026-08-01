use anyhow::Result;
use couchdb_file_sync::models::{IgnoreMatcher, SyncDirPath};
use couchdb_file_sync::{CouchDb, LocalDb, SyncEngine};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(prefix: &str) -> Result<Self> {
        let cwd = env::current_dir()?;
        let base = cwd.join("_testdata");
        fs::create_dir_all(&base)?;
        let path = base.join(format!("{}-{}", prefix, unique_suffix()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
#[ignore = "requires a running CouchDB server (see COUCHDB_FILE_SYNC_TEST_DB_* env vars)"]
async fn dry_run_does_not_modify_state_db_or_remote_couchdb() -> Result<()> {
    let test_dir = TestDir::new("dry-run")?;
    let state_dir = test_dir.join(".couchdb-file-sync");
    fs::create_dir_all(&state_dir)?;
    let state_db = state_dir.join("state.db");

    // Local file that would be uploaded, and a tracked-but-missing file that
    // would be deleted from the remote.
    fs::write(test_dir.join("a.txt"), "hello\n")?;
    let (url, db_name, user, pass, remote_path) = test_db_config();
    let couchdb = CouchDb::new(
        &url,
        user.as_deref(),
        pass.as_deref(),
        &db_name,
        &remote_path,
    )
    .await?;
    assert!(couchdb.ping().await?, "CouchDB ping failed");

    let test_result: Result<()> = async {
        let local_db = LocalDb::open(&state_db)?;
        local_db.save_file_state(&couchdb_file_sync::models::FileState::new(
            "gone.txt".to_string(),
            "stalehash".to_string(),
            4,
            chrono::Utc::now(),
        ))?;

        let mut engine = SyncEngine::with_ignore(
            couchdb,
            local_db,
            SyncDirPath::new(test_dir.path.clone()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync_dry_run().await?;

        // The dry-run still performed triage and reports what would happen.
        assert_eq!(report.uploaded.0, 1, "a.txt would be uploaded");
        assert_eq!(report.deleted_remote, 1, "gone.txt would delete remote");

        // The state DB must be unchanged: no file states saved for a.txt,
        // the gone.txt state must still be present, no conflicts, no checkpoint.
        assert!(
            engine.get_file_state("a.txt")?.is_none(),
            "dry run must not record new file states"
        );
        assert!(
            engine.get_file_state("gone.txt")?.is_some(),
            "dry run must not delete existing file states"
        );
        assert!(
            engine.get_conflicts()?.is_empty(),
            "dry run must not store conflicts"
        );
        assert!(
            engine.get_checkpoint()?.is_none(),
            "dry run must not advance the checkpoint"
        );

        // Local filesystem unchanged.
        assert!(test_dir.join("a.txt").exists(), "local file must remain");
        assert!(
            !test_dir.join("gone.txt").exists(),
            "dry run must not touch the local filesystem"
        );

        // Remote CouchDB unchanged: no documents were created, updated, or deleted.
        let verify = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;
        let remote_docs = verify.get_all_files().await?;
        assert!(
            remote_docs.is_empty(),
            "dry run must not modify remote CouchDB (found {} docs)",
            remote_docs.len()
        );

        Ok(())
    }
    .await;

    // Best-effort cleanup of anything the test may have left behind.
    let cleanup_couchdb = CouchDb::new(
        &url,
        user.as_deref(),
        pass.as_deref(),
        &db_name,
        &remote_path,
    )
    .await?;
    let cleanup_docs = cleanup_couchdb
        .get_all_files()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|doc| doc.id)
        .collect::<Vec<_>>();
    let cleanup_chunks = cleanup_couchdb
        .get_all_files()
        .await
        .unwrap_or_default()
        .into_iter()
        .flat_map(|doc| doc.children)
        .collect::<Vec<_>>();
    if !cleanup_chunks.is_empty() {
        let _ = cleanup_couchdb.delete_chunks(&cleanup_chunks).await;
    }
    for doc_id in dedup_strings(cleanup_docs) {
        let _ = cleanup_couchdb.delete_file(&doc_id).await;
    }

    test_result
}

fn test_db_config() -> (String, String, Option<String>, Option<String>, String) {
    let url = env_or_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_URL", "COUCHFS_TEST_DB_URL"],
        "http://localhost:5984",
    );
    let db_name = env_or_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_NAME", "COUCHFS_TEST_DB_NAME"],
        "couchdb_file_sync_dry_run_test",
    );
    let mut remote_path = env_var_first(&[
        "COUCHDB_FILE_SYNC_TEST_REMOTE_PATH",
        "COUCHFS_TEST_REMOTE_PATH",
    ])
    .unwrap_or_else(|| format!("dry-run-test-{}", unique_suffix()));
    if !remote_path.is_empty() && !remote_path.ends_with('/') {
        remote_path.push('/');
    }

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

    (url, db_name, user, pass, remote_path)
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
    env_var_first(keys).or_else(|| default.map(|d| d.to_string()))
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn dedup_strings(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}
