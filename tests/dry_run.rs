use anyhow::Result;
use couchdb_file_sync::config::AppConfig;
use couchdb_file_sync::models::{IgnoreMatcher, SyncDirPath};
use couchdb_file_sync::{CouchDb, LocalDb, SyncEngine, cli};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
#[allow(clippy::too_many_lines)]
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
        30,
        3,
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
            SyncDirPath::new(&test_dir.path).unwrap(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync_dry_run().await?;

        // The dry-run still performed triage and reports what would happen.
        assert_eq!(report.uploaded.0, 1, "a.txt would be uploaded");
        assert_eq!(report.deleted_remote, 1, "gone.txt would delete remote");

        // The state DB must be unchanged: no file states saved for a.txt,
        // the gone.txt state must still be present, no conflicts, no checkpoint.
        assert!(
            engine.get_file_state("a.txt").await?.is_none(),
            "dry run must not record new file states"
        );
        assert!(
            engine.get_file_state("gone.txt").await?.is_some(),
            "dry run must not delete existing file states"
        );
        assert!(
            engine.get_conflicts().await?.is_empty(),
            "dry run must not store conflicts"
        );
        assert!(
            engine.get_checkpoint().await?.is_none(),
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
            30,
            3,
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
        30,
        3,
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

// ─────────────────────────────────────────────────────────────────────────────
// In-process fake CouchDB server for dry-run contract tests
// ─────────────────────────────────────────────────────────────────────────────

/// A `CouchDB`-shaped response body for the database-info endpoint (`GET
/// /<db>`), used by both the sync-identity UUID fetch and the bootstrap
/// update-sequence probe.
const DB_INFO_OK: &str = r#"{"db_name":"test_db","update_seq":"0-1","uuid":"fake-uuid-6368","instance_start_time":"0","disk_format_version":6,"committed_update_seq":"0-1","compact_running":false,"doc_count":0,"doc_del_count":0,"purge_seq":"0","cluster":{"n":1,"q":2,"r":1,"w":1},"sizes":{"file":0,"external":0,"active":0},"props":{}}"#;

/// A `CouchDB`-shaped "database does not exist" body.
const DB_MISSING: &str = r#"{"error":"not_found","reason":"Database does not exist."}"#;

/// Serve one fake-`CouchDB` request, recording the method into `methods`.
///
/// When `existing` is false the database is absent, so the database-info
/// endpoint (and everything else) 404s — the client must surface the absence
/// explicitly and never create the database. When `existing` is true an empty
/// database is served: info on `GET /test_db`, an empty `_all_docs`, an empty
/// `_changes` feed, and empty `_find`/`_index` responses.
#[allow(clippy::unused_async)]
async fn handle_fake_req(
    req: hyper::Request<hyper::body::Incoming>,
    existing: bool,
    methods: Arc<Mutex<Vec<String>>>,
) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
    use hyper::Method;
    use hyper::StatusCode;

    let (parts, _body) = req.into_parts();
    let method = parts.method.to_string();
    // couch_rs percent-encodes the database name (e.g. `test_db` -> `test%5Fdb`),
    // so normalize the path before matching.
    let path = parts.uri.path().replace("%5F", "_");
    methods
        .lock()
        .expect("fake couch methods lock")
        .push(format!("{method} {path}"));

    let mut response = hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));

    if parts.method == Method::GET && path == "/test_db" {
        if existing {
            *response.body_mut() =
                http_body_util::Full::new(hyper::body::Bytes::from_static(DB_INFO_OK.as_bytes()));
        } else {
            *response.status_mut() = StatusCode::NOT_FOUND;
            *response.body_mut() =
                http_body_util::Full::new(hyper::body::Bytes::from_static(DB_MISSING.as_bytes()));
        }
        return Ok(response);
    }

    if !existing {
        *response.status_mut() = StatusCode::NOT_FOUND;
        return Ok(response);
    }

    match path.as_str() {
        "/test_db/_all_docs" => {
            *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                br#"{"total_rows":0,"offset":0,"rows":[]}"#,
            ));
        }
        "/test_db/_changes" => {
            *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                br#"{"results":[],"last_seq":"0-1"}"#,
            ));
        }
        "/test_db/_find" => {
            *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                br#"{"docs":[],"bookmark":"nil"}"#,
            ));
        }
        "/test_db/_index" => {
            *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                br#"{"ok":true,"result":"created"}"#,
            ));
        }
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        }
    }
    Ok(response)
}

/// An owned in-process fake `CouchDB` server plus its method recorder.
struct FakeCouch {
    base_url: String,
    methods: Arc<Mutex<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl FakeCouch {
    fn start(existing: bool) -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake couchdb server");
        let addr = listener.local_addr().expect("fake couchdb addr");
        let base_url = format!("http://{addr}");
        let shutdown = Arc::new(AtomicBool::new(false));
        let methods: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown_thread = shutdown.clone();
        let methods_thread = methods.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake couchdb runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake couchdb nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("fake couchdb tokio listener");
                loop {
                    if shutdown_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let methods_thread = methods_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let methods_thread = methods_thread.clone();
                                async move { handle_fake_req(req, existing, methods_thread).await }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        Self {
            base_url,
            methods,
            handle: Some(handle),
            shutdown,
        }
    }

    /// Every HTTP method observed, in order.
    fn methods(&self) -> Vec<String> {
        self.methods
            .lock()
            .expect("fake couch methods lock")
            .clone()
    }

    /// Only mutating methods (`PUT`/`DELETE`), which a dry run must never
    /// issue against the remote. `POST`/`GET` are reads here (`_all_docs`,
    /// `_find`, `_changes`, the DB-info probe).
    fn write_methods(&self) -> Vec<String> {
        self.methods()
            .into_iter()
            .filter(|m| m.starts_with("PUT ") || m.starts_with("DELETE "))
            .collect()
    }
}

impl Drop for FakeCouch {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// An `AppConfig` pointed at the fake server's `test_db` (empty remote path).
fn dry_run_config(base_url: &str) -> AppConfig {
    let mut config = AppConfig::default();
    config.couchdb.url = base_url.to_string();
    config.couchdb.database = "test_db".to_string();
    config.couchdb.remote_path = String::new();
    config
}

// ─────────────────────────────────────────────────────────────────────────────
// Dry-run no-write contract (no live CouchDB required)
// ─────────────────────────────────────────────────────────────────────────────

/// With no local state DB, a dry run must use an ephemeral baseline: it must
/// not create the `.couchdb-file-sync` state directory or `state.db`, must not
/// issue any mutating remote request, and still reports what would happen.
#[tokio::test]
async fn dry_run_absent_state_creates_no_state_and_no_remote_writes() -> Result<()> {
    let fake = FakeCouch::start(true);
    let test_dir = TestDir::new("dry-run-absent")?;
    fs::write(test_dir.join("a.txt"), "hello\n")?;

    let report = cli::sync(test_dir.path.clone(), dry_run_config(&fake.base_url), true).await?;
    assert_eq!(report.uploaded.0, 1, "a.txt would be uploaded");

    assert!(
        !test_dir.join(".couchdb-file-sync").exists(),
        "dry run must not create a state directory"
    );
    assert!(
        !test_dir.join(".couchfs").exists(),
        "dry run must not create a legacy state directory"
    );
    assert!(
        fake.write_methods().is_empty(),
        "dry run must not issue any PUT/POST/DELETE to the remote, saw: {:?}",
        fake.write_methods()
    );
    Ok(())
}

/// With an existing (legacy) state DB, a dry run must open it read-only: the
/// file must be byte-identical afterwards and no remote write may be issued.
#[tokio::test]
async fn dry_run_existing_state_is_used_read_only_and_unchanged() -> Result<()> {
    let fake = FakeCouch::start(true);
    let test_dir = TestDir::new("dry-run-legacy")?;
    fs::write(test_dir.join("a.txt"), "hello\n")?;

    let state_dir = test_dir.join(".couchdb-file-sync");
    fs::create_dir_all(&state_dir)?;
    let state_db = state_dir.join("state.db");
    {
        let db = LocalDb::open(&state_db)?;
        db.save_file_state(&couchdb_file_sync::models::FileState::new(
            "old.txt".to_string(),
            "oldhash".to_string(),
            5,
            chrono::Utc::now(),
        ))?;
    }
    let before = fs::read(&state_db)?;

    let report = cli::sync(test_dir.path.clone(), dry_run_config(&fake.base_url), true).await?;
    let after = fs::read(&state_db)?;

    assert_eq!(report.uploaded.0, 1, "a.txt would be uploaded");
    assert_eq!(
        before, after,
        "dry run must open the existing state DB read-only and never modify it"
    );
    assert!(
        fake.write_methods().is_empty(),
        "dry run must not issue any PUT/POST/DELETE, saw: {:?}",
        fake.write_methods()
    );
    Ok(())
}

/// With an absent remote database, a dry run must surface the absence
/// explicitly (an error, no silent write to create it) and issue no mutating
/// requests.
#[tokio::test]
async fn dry_run_absent_remote_db_errors_explicitly_without_writes() -> Result<()> {
    let fake = FakeCouch::start(false);
    let test_dir = TestDir::new("dry-run-noremote")?;
    fs::write(test_dir.join("a.txt"), "hello\n")?;

    let err = cli::sync(test_dir.path.clone(), dry_run_config(&fake.base_url), true)
        .await
        .expect_err("absent remote DB must fail the dry run explicitly");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not exist"),
        "expected an explicit missing-database error, got: {msg}"
    );
    assert!(
        !test_dir.join(".couchdb-file-sync").exists(),
        "dry run must not create state when the remote DB is absent"
    );
    assert!(
        fake.write_methods().is_empty(),
        "absent remote DB must not trigger any PUT/POST/DELETE, saw: {:?}",
        fake.write_methods()
    );
    Ok(())
}

/// A dry-run sync with `logging.file` configured must not create the log file,
/// while the same non-dry-run command does (proving the dry-run routing is
/// what suppresses it).
#[test]
fn dry_run_with_configured_file_logging_creates_no_log_file() -> Result<()> {
    use std::process::Command;

    let fake = FakeCouch::start(true);
    let dir = tempfile::TempDir::new()?;
    let sync_root = dir.path().join("root");
    fs::create_dir_all(&sync_root)?;
    fs::write(sync_root.join("a.txt"), "hello\n")?;

    let log_path = dir.path().join("sync.log");
    let config_path = dir.path().join("config.yaml");
    fs::write(
        &config_path,
        format!(
            "couchdb:\n  url: {}\n  database: test_db\n  remote_path: \"\"\nlogging:\n  file: {}\n",
            fake.base_url,
            log_path.display()
        ),
    )?;

    let bin = env!("CARGO_BIN_EXE_couchdb-file-sync");

    // Control: the same command without --dry-run enables file logging, so the
    // log file is created by init_logging before dispatch (the sync itself may
    // then fail against the minimal fake server — irrelevant to this check).
    Command::new(bin)
        .args([
            "-c",
            config_path.to_str().expect("config path"),
            "sync",
            sync_root.to_str().expect("sync root"),
        ])
        .status()?;
    assert!(
        log_path.exists(),
        "non-dry-run should create the configured log file"
    );
    fs::remove_file(&log_path)?;

    // Dry-run must not create the log file.
    let status = Command::new(bin)
        .args([
            "-c",
            config_path.to_str().expect("config path"),
            "sync",
            sync_root.to_str().expect("sync root"),
            "--dry-run",
        ])
        .status()?;
    assert!(status.success(), "dry-run sync should exit successfully");
    assert!(
        !log_path.exists(),
        "dry-run must never create the configured log file"
    );
    Ok(())
}
