use anyhow::Result;
use couchdb_file_sync::models::SyncDirPath;
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
        let path = base.join(format!("{prefix}-{}", unique_suffix()));
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

/// Story #6350: exercise directory moves/renames and two-way file/directory
/// transitions end-to-end against a real `CouchDB`.
///
/// This mirrors `remote_move.rs`. It drives the periodic full-scan
/// reconciliation that a live directory-scoped watcher event routes into
/// (`Reconcile::mark_full_rescan`), verifying that moving a populated
/// directory removes the old remote child paths and uploads the new ones, that
/// a remote file over an empty leftover local directory converges, and that a
/// nonempty untracked local directory is preserved rather than removed.
#[tokio::test]
#[ignore = "requires a running CouchDB server (see COUCHDB_FILE_SYNC_TEST_DB_* env vars)"]
#[allow(clippy::too_many_lines)] // end-to-end scenario mirrors remote_move.rs
async fn directory_move_and_type_transitions_converge() -> Result<()> {
    let test_dir = TestDir::new("dir-moves")?;
    let state_dir = test_dir.join(".couchdb-file-sync");
    fs::create_dir_all(&state_dir)?;
    let state_db = state_dir.join("state.db");

    let (url, db_name, user, pass, remote_path) = test_db_config();
    let old_child = format!("{remote_path}olddir/a.txt");
    let new_child = format!("{remote_path}newdir/a.txt");
    let empty_dir_file = format!("{remote_path}converge.txt");
    let preserved_dir_file = format!("{remote_path}preserve.txt");
    let cleanup_docs = vec![
        old_child.clone(),
        new_child.clone(),
        empty_dir_file.clone(),
        preserved_dir_file.clone(),
    ];
    let mut cleanup_chunks: Vec<String> = Vec::new();

    let couchdb = CouchDb::new(
        &url,
        user.as_deref(),
        pass.as_deref(),
        &db_name,
        &remote_path,
        30,
        3,
    )
    .await?;
    assert!(couchdb.ping().await?, "CouchDB ping failed");

    let test_result: Result<()> = async {
        // 1. First sync uploads a populated local directory.
        fs::create_dir_all(test_dir.join("olddir"))?;
        fs::write(test_dir.join("olddir/a.txt"), "hello\n")?;

        let mut engine = SyncEngine::new(
            make_couch(
                &url,
                user.as_deref(),
                pass.as_deref(),
                &db_name,
                &remote_path,
            )
            .await?,
            LocalDb::open(&state_db)?,
            SyncDirPath::new(&test_dir.path).unwrap(),
        );
        engine.sync().await?;

        // 2. Rename the directory and rescan (the reconciliation a live
        //    directory event triggers): the old remote child must be removed
        //    and the new one uploaded.
        fs::rename(test_dir.join("olddir"), test_dir.join("newdir"))?;
        let mut engine = SyncEngine::new(
            make_couch(
                &url,
                user.as_deref(),
                pass.as_deref(),
                &db_name,
                &remote_path,
            )
            .await?,
            LocalDb::open(&state_db)?,
            SyncDirPath::new(&test_dir.path).unwrap(),
        );
        engine.sync().await?;

        assert!(
            !test_dir.join("olddir").exists(),
            "old local directory should be gone after the move"
        );
        assert!(
            test_dir.join("newdir/a.txt").is_file(),
            "new local child should exist after the move"
        );

        // The remote docs must reflect the move: the old child is deleted and
        // the new child exists with the content.
        let couchdb = make_couch(
            &url,
            user.as_deref(),
            pass.as_deref(),
            &db_name,
            &remote_path,
        )
        .await?;
        let old_doc = couchdb.get_file(&old_child).await?;
        assert!(
            old_doc.is_some_and(|d| d.deleted),
            "old remote child must be tombstoned after the directory move"
        );
        let new_doc = couchdb.get_file(&new_child).await?;
        assert!(
            new_doc.as_ref().is_some_and(|d| !d.deleted),
            "new remote child must be present after the directory move"
        );
        if let Some(doc) = &new_doc {
            cleanup_chunks.extend(doc.children.clone());
        }

        // 3. Directory-to-file transition: a remote file landing on an empty
        //    leftover local directory must converge to a local file (not be
        //    silently skipped and checkpointed).
        fs::create_dir_all(test_dir.join("converge.txt"))?;
        let mut remote_doc =
            couchdb_file_sync::models::FileDoc::new(empty_dir_file.clone(), String::new(), 3);
        remote_doc.path = empty_dir_file.clone();
        let content = b"abc".to_vec();
        let chunk_ids = couchdb.upload_file_content(content.clone()).await?;
        remote_doc.children = chunk_ids.clone();
        remote_doc.size = content.len() as u64;
        couchdb.save_file(&mut remote_doc).await?;
        cleanup_chunks.extend(chunk_ids);

        let mut engine = SyncEngine::new(
            make_couch(
                &url,
                user.as_deref(),
                pass.as_deref(),
                &db_name,
                &remote_path,
            )
            .await?,
            LocalDb::open(&state_db)?,
            SyncDirPath::new(&test_dir.path).unwrap(),
        );
        engine.sync().await?;

        assert!(
            test_dir.join("converge.txt").is_file(),
            "the empty leftover directory must be replaced by the remote file"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("converge.txt"))?,
            "abc",
            "the converged file must hold the remote content"
        );

        // 4. Untracked nonempty local content must be preserved, not removed:
        //    a remote file landing on a nonempty local directory leaves the
        //    directory and its untracked children intact.
        fs::create_dir_all(test_dir.join("preserve.txt"))?;
        fs::write(test_dir.join("preserve.txt/child.txt"), b"untracked")?;
        let mut remote_doc =
            couchdb_file_sync::models::FileDoc::new(preserved_dir_file.clone(), String::new(), 3);
        remote_doc.path = preserved_dir_file.clone();
        let chunk_ids = couchdb.upload_file_content(b"xyz".to_vec()).await?;
        remote_doc.children = chunk_ids.clone();
        remote_doc.size = 3;
        couchdb.save_file(&mut remote_doc).await?;
        cleanup_chunks.extend(chunk_ids);

        let mut engine = SyncEngine::new(
            make_couch(
                &url,
                user.as_deref(),
                pass.as_deref(),
                &db_name,
                &remote_path,
            )
            .await?,
            LocalDb::open(&state_db)?,
            SyncDirPath::new(&test_dir.path).unwrap(),
        );
        engine.sync().await?;

        assert!(
            test_dir.join("preserve.txt").is_dir(),
            "the nonempty local directory must be preserved"
        );
        assert_eq!(
            fs::read_to_string(test_dir.join("preserve.txt/child.txt"))?,
            "untracked",
            "untracked content inside the directory must survive the sync"
        );

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
        eprintln!("cleanup failed: {err}");
    }

    test_result
}

async fn make_couch(
    url: &str,
    user: Option<&str>,
    pass: Option<&str>,
    db_name: &str,
    remote_path: &str,
) -> Result<CouchDb> {
    CouchDb::new(url, user, pass, db_name, remote_path, 30, 3).await
}

fn test_db_config() -> (String, String, Option<String>, Option<String>, String) {
    let url = env_or_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_URL", "COUCHFS_TEST_DB_URL"],
        "http://localhost:5984",
    );
    let db_name = env_or_first(
        &["COUCHDB_FILE_SYNC_TEST_DB_NAME", "COUCHFS_TEST_DB_NAME"],
        "couchdb_file_sync_dir_moves_test",
    );
    let mut remote_path = env_var_first(&[
        "COUCHDB_FILE_SYNC_TEST_REMOTE_PATH",
        "COUCHFS_TEST_REMOTE_PATH",
    ])
    .unwrap_or_else(|| format!("dir-moves-test-{}", unique_suffix()));
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
    env_var_first(keys).or_else(|| default.map(std::string::ToString::to_string))
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

async fn cleanup_remote(
    url: &str,
    db_name: &str,
    user: Option<&str>,
    pass: Option<&str>,
    remote_path: &str,
    doc_ids: &[String],
    chunk_ids: &[String],
) -> Result<()> {
    let couchdb = CouchDb::new(url, user, pass, db_name, remote_path, 30, 3).await?;
    if !chunk_ids.is_empty() {
        couchdb.delete_chunks(chunk_ids).await?;
    }
    for doc_id in doc_ids {
        let _ = couchdb.delete_file(doc_id).await;
    }
    Ok(())
}
