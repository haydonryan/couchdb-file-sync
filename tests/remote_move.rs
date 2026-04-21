use anyhow::Result;
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
async fn remote_move_deletes_old_local_path() -> Result<()> {
    let test_dir = TestDir::new("remote-move")?;
    let state_dir = test_dir.join(".couchdb-file-sync");
    fs::create_dir_all(&state_dir)?;

    let state_db = state_dir.join("state.db");

    let old_local = test_dir.join("old.txt");
    fs::write(&old_local, "hello\n")?;

    let (url, db_name, user, pass, remote_path) = test_db_config();
    let old_remote = format!("{}old.txt", remote_path);
    let new_remote = format!("{}new.txt", remote_path);
    let cleanup_docs = vec![old_remote.clone(), new_remote.clone()];
    let mut cleanup_chunks: Vec<String> = Vec::new();

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
        let mut engine = SyncEngine::new(couchdb, local_db, test_dir.path.clone());
        engine.sync().await?;

        // Simulate a remote move: copy doc to new ID and mark old ID deleted.
        let couchdb = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;

        let mut old_doc = couchdb
            .get_file(&old_remote)
            .await?
            .expect("missing remote doc for old.txt");

        let now = now_ms();
        let mut new_doc = old_doc.clone();
        new_doc.id = new_remote.clone();
        new_doc.rev = None;
        new_doc.path = new_remote.clone();
        new_doc.mtime = now;
        new_doc.deleted = false;
        couchdb.save_file(&mut new_doc).await?;

        cleanup_chunks.extend(old_doc.children.clone());
        cleanup_chunks.extend(new_doc.children.clone());

        old_doc.deleted = true;
        old_doc.mtime = now;
        couchdb.save_file(&mut old_doc).await?;

        let local_db = LocalDb::open(&state_db)?;
        let couchdb = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;
        let mut engine = SyncEngine::new(couchdb, local_db, test_dir.path.clone());
        engine.sync().await?;

        let new_local = test_dir.join("new.txt");
        assert!(!old_local.exists(), "old local file should be removed");
        assert!(new_local.exists(), "new local file should exist");
        assert_eq!(fs::read_to_string(&new_local)?, "hello\n");

        Ok(())
    }
    .await;

    let cleanup_docs = dedup_strings(cleanup_docs);
    let cleanup_chunks = dedup_strings(cleanup_chunks);
    if let Err(err) = cleanup_remote(
        &url,
        &db_name,
        user.as_deref(),
        pass.as_deref(),
        &remote_path,
        &cleanup_docs,
        &cleanup_chunks,
    )
    .await
    {
        eprintln!("cleanup failed: {}", err);
    }

    test_result
}

#[tokio::test]
#[ignore = "requires a running CouchDB server (see COUCHDB_FILE_SYNC_TEST_DB_* env vars)"]
async fn local_edit_after_upload_should_upload_again_not_conflict() -> Result<()> {
    let test_dir = TestDir::new("local-reupload")?;
    let state_dir = test_dir.join(".couchdb-file-sync");
    fs::create_dir_all(&state_dir)?;

    let state_db = state_dir.join("state.db");
    let local_file = test_dir.join("note.md");
    fs::write(&local_file, "first line\n")?;

    let remote_id = {
        let (_, _, _, _, remote_path) = test_db_config();
        format!("{}note.md", remote_path)
    };

    let test_result: Result<()> = async {
        let (url, db_name, user, pass, remote_path) = test_db_config();

        let local_db = LocalDb::open(&state_db)?;
        let couchdb = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;
        let mut engine = SyncEngine::new(couchdb, local_db, test_dir.path.clone());

        let first_report = engine.sync().await?;
        assert_eq!(first_report.uploaded, 1, "initial file should upload");

        fs::write(&local_file, "second line\n")?;

        let local_db = LocalDb::open(&state_db)?;
        let couchdb = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;
        let mut engine = SyncEngine::new(couchdb, local_db, test_dir.path.clone());

        let second_report = engine.sync().await?;
        assert_eq!(
            second_report.conflicts, 0,
            "local edit after a successful upload should be re-uploaded, not conflicted",
        );
        assert_eq!(
            second_report.uploaded, 1,
            "local edit after a successful upload should upload one file",
        );

        let local_db = LocalDb::open(&state_db)?;
        let conflicts = local_db.get_conflicts()?;
        assert!(
            conflicts.is_empty(),
            "reupload path should not leave conflict entries"
        );

        let couchdb = CouchDb::new(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;
        let remote_content = couchdb.get_file_content(&remote_id).await?;
        assert_eq!(String::from_utf8(remote_content)?, "second line\n");

        Ok(())
    }
    .await;

    let (url, db_name, user, pass, remote_path) = test_db_config();
    if let Ok(couchdb) = CouchDb::new(
        &url,
        user.as_deref(),
        pass.as_deref(),
        &db_name,
        &remote_path,
    )
    .await
    {
        if let Ok(Some(doc)) = couchdb.get_file(&remote_id).await {
            if let Err(err) = couchdb.delete_file(&remote_id).await {
                eprintln!("cleanup delete {} failed: {}", remote_id, err);
            }

            if !doc.children.is_empty()
                && let Err(err) = couchdb.delete_chunks(&doc.children).await
            {
                eprintln!("cleanup delete chunks for {} failed: {}", remote_id, err);
            }
        }
    } else {
        eprintln!("cleanup failed: could not connect to CouchDB");
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
        "couchdb_file_sync_move_test",
    );
    let mut remote_path = env_var_first(&[
        "COUCHDB_FILE_SYNC_TEST_REMOTE_PATH",
        "COUCHFS_TEST_REMOTE_PATH",
    ])
    .unwrap_or_else(|| format!("remote-move-test-{}", unique_suffix()));
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn dedup_strings(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

async fn cleanup_remote(
    url: &str,
    db_name: &str,
    user: Option<&str>,
    pass: Option<&str>,
    remote_path: &str,
    doc_ids: &[String],
    chunk_ids: &[String],
) -> Result<()> {
    let couchdb = CouchDb::new(url, user, pass, db_name, remote_path).await?;
    if !chunk_ids.is_empty() {
        couchdb.delete_chunks(chunk_ids).await?;
    }
    for doc_id in doc_ids {
        let _ = couchdb.delete_file(doc_id).await;
    }
    Ok(())
}
