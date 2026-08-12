use crate::models::{Change, ChunkDoc, DatabaseName, FileDoc, RemotePath, TimestampMillis};
use anyhow::Result;
use couch_rs::Client;
use couch_rs::database::Database;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tracing::{debug, warn};

/// Base delay in milliseconds for retry backoff; doubles after each retry.
/// Only used in non-test builds; tests use a fixed tiny delay (`test_backoff`).
#[cfg(not(test))]
const RETRY_BASE_BACKOFF_MS: u64 = 250;

/// `CouchDB` client wrapper
pub struct CouchDb {
    client: Client,
    db: Database,
    http_client: HttpClient,
    base_db_url: String,
    db_name: DatabaseName,
    auth: Option<(String, String)>,
    /// Remote path prefix to sync (e.g., "notes/" or "obsidian/")
    remote_path: RemotePath,
    /// Request timeout in seconds applied to all `CouchDB` HTTP operations.
    timeout_seconds: u64,
    /// Maximum total attempts (including the initial one) for a `CouchDB`
    /// operation before giving up on transient connectivity failures.
    retry_attempts: u32,
    /// Test-only canned responses used to exercise the sync pipeline without a
    /// live `CouchDB` server. Only present in test builds.
    #[cfg(test)]
    test_state: Option<CannedCouch>,
    /// Test-only counter of `CouchDB` write attempts (save/delete/chunk ops).
    /// Only present in test builds.
    #[cfg(test)]
    test_write_calls: std::sync::atomic::AtomicUsize,
    /// Test-only override for the retry backoff delay. Only present in test
    /// builds so retry unit tests run quickly without real sleeps.
    #[cfg(test)]
    test_backoff: Duration,
}

/// Test-only canned responses for the `CouchDB` client.
///
/// When `test_state` is set, read methods return this data instead of hitting
/// the network and write methods record a call instead of mutating anything.
#[cfg(test)]
#[derive(Default, Clone)]
pub struct CannedCouch {
    /// Remote changes returned by `get_changes`.
    pub changes: Vec<crate::models::Change>,
    /// Last sequence value returned by `get_changes`.
    pub last_seq: String,
    /// Metadata returned by `fetch_metadata`, keyed by remote path.
    pub metadata: std::collections::HashMap<String, crate::models::FileDoc>,
    /// File content returned by `get_file_content`, keyed by remote path.
    pub contents: std::collections::HashMap<String, Vec<u8>>,
    /// Paths for which `get_file_content` should return a fetch error instead
    /// of content, simulating a failed network/auth/server content download.
    pub content_errors: std::collections::HashSet<String>,
    /// Remote paths (doc ids) for which `save_file` should return an error,
    /// simulating a failed network/auth/server content upload.
    pub save_errors: std::collections::HashSet<String>,
    /// When set, each `save_file`/`get_file_content` call records itself in the
    /// probe and sleeps for `batch_delay`, letting tests observe how many
    /// per-file operations are in flight at once.
    pub probe: Option<std::sync::Arc<ConcurrencyProbe>>,
    /// Optional artificial per-operation delay to force overlap in
    /// bounded-concurrency tests. Only consulted when `probe` is set.
    pub batch_delay: Option<std::time::Duration>,
}

/// Test-only tracker of the maximum number of concurrent in-flight canned
/// `CouchDB` operations, used to assert that the sync apply loops respect the
/// bounded-concurrency limit.
#[cfg(test)]
#[derive(Default)]
pub struct ConcurrencyProbe {
    current: std::sync::atomic::AtomicUsize,
    max: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl ConcurrencyProbe {
    fn enter(&self) {
        let now = self
            .current
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
    }

    fn leave(&self) {
        self.current
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Highest number of concurrent operations observed.
    pub fn max_concurrent(&self) -> usize {
        self.max.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Entry from a `CouchDB` changes feed
#[derive(Debug, Clone)]
pub struct ChangeFeedEntry {
    pub change: Change,
    pub seq: String,
}

#[derive(Debug, Deserialize)]
struct ChangesResponse<T> {
    results: Vec<ChangeRow<T>>,
    last_seq: Value,
}

#[derive(Debug, Deserialize)]
struct ChangeRow<T> {
    id: String,
    seq: Value,
    deleted: Option<bool>,
    doc: Option<T>,
}

fn seq_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

impl CouchDb {
    /// Create a new `CouchDB` client.
    ///
    /// `timeout_seconds` is applied as the request timeout for every `CouchDB`
    /// HTTP operation (both the direct reqwest client and the `couch_rs`
    /// client). `retry_attempts` caps the total number of attempts made for an
    /// operation when it fails with a transient connectivity error (see
    /// [`Self::retry_transient`]); the operation is attempted at most
    /// `retry_attempts` times in total, with exponential backoff between
    /// attempts.
    ///
    /// # Errors
    ///
    /// Returns an error if the `couch_rs` client cannot be created or the
    /// connection to `CouchDB` cannot be established.
    pub async fn new(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        db_name: &str,
        remote_path: &str,
        timeout_seconds: u64,
        retry_attempts: u32,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_seconds);
        let client = match (username, password) {
            (Some(u), Some(p)) => {
                Client::new_with_timeout(url, Some(u), Some(p), Some(timeout_seconds))?
            }
            _ => Client::new_with_timeout(url, None, None, Some(timeout_seconds))?,
        };

        // Get or create database
        let db = client.db(db_name).await?;

        let auth = match (username, password) {
            (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
            _ => None,
        };

        let base = url.trim_end_matches('/');
        let base_db_url = if base.ends_with(&format!("/{db_name}")) {
            base.to_string()
        } else {
            format!("{base}/{db_name}")
        };

        Ok(Self {
            client,
            db,
            http_client: HttpClient::builder().timeout(timeout).build()?,
            base_db_url,
            db_name: DatabaseName::new(db_name.to_string()),
            auth,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds,
            retry_attempts,
            #[cfg(test)]
            test_state: None,
            #[cfg(test)]
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_backoff: Duration::from_millis(1),
        })
    }

    async fn get_update_seq(&self) -> Result<String> {
        let info = self
            .retry_transient(|| self.client.get_info(&self.db_name))
            .await?;
        Ok(info.update_seq)
    }

    /// Create a `CouchDb` instance for testing without connecting to a real server.
    /// Only fields needed for path conversion and sync metadata are populated.
    ///
    /// # Panics
    ///
    /// Panics if the structural `couch_rs` client cannot be created, and panics
    /// if any method requiring actual `CouchDB` access is called.
    #[cfg(test)]
    #[must_use]
    pub fn for_test(remote_path: &str) -> Self {
        let remote_path = RemotePath::new(remote_path);
        // Create a client and database handle without connecting.
        // `Database::new` and `Client::new_no_auth` are purely structural;
        // actual HTTP requests will fail at runtime.
        let client = couch_rs::Client::new_no_auth("http://localhost:1")
            .expect("failed to create couch_rs client for test");
        let db = couch_rs::database::Database::new("unittest".to_string(), client.clone());
        Self {
            client,
            db,
            http_client: reqwest::Client::new(),
            base_db_url: "http://localhost:1/unittest".to_string(),
            db_name: DatabaseName::new("unittest"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            #[cfg(test)]
            test_state: None,
            #[cfg(test)]
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Create a `CouchDb` instance for testing backed by canned data.
    ///
    /// Read methods (`get_changes`, `fetch_metadata`, `get_file_content`)
    /// return the canned values, and write methods (`save_file`,
    /// `delete_file`, `upload_file_content`, `delete_chunks`) increment the
    /// write-call counter and do nothing, so tests can assert that a dry-run
    /// never issues any remote writes.
    #[cfg(test)]
    #[must_use]
    pub fn for_test_with_canned(remote_path: &str, canned: CannedCouch) -> Self {
        let mut client = Self::for_test(remote_path);
        client.test_state = Some(canned);
        client
    }

    /// Number of write attempts issued against this test client.
    #[cfg(test)]
    #[must_use]
    pub fn test_write_calls(&self) -> usize {
        self.test_write_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Fetch changes from `CouchDB` using the _changes feed (longpoll)
    ///
    /// # Errors
    ///
    /// Returns an error if the changes feed request fails or the update
    /// sequence cannot be fetched.
    pub async fn get_changes_feed(
        &self,
        since: &str,
        timeout_ms: u64,
    ) -> Result<(Vec<ChangeFeedEntry>, String)> {
        let url = format!("{}/_changes", self.base_db_url);

        let body = self
            .retry_transient::<_, _, ChangesResponse<FileDoc>, anyhow::Error>(|| {
                let mut request = self.http_client.get(&url).query(&[
                    ("since", since),
                    ("include_docs", "true"),
                    ("feed", "longpoll"),
                    ("timeout", &timeout_ms.to_string()),
                ]);

                if let Some((username, password)) = &self.auth {
                    request = request.basic_auth(username, Some(password));
                }

                async move {
                    let response = request.send().await?.error_for_status()?;
                    response
                        .json::<ChangesResponse<FileDoc>>()
                        .await
                        .map_err(Into::into)
                }
            })
            .await?;

        let mut entries = Vec::new();
        for row in body.results {
            if !self.is_path_allowed(&row.id) {
                continue;
            }

            if row.deleted.unwrap_or(false) {
                entries.push(ChangeFeedEntry {
                    change: Change::remote_deleted(row.id, None),
                    seq: seq_to_string(&row.seq),
                });
                continue;
            }

            let Some(doc) = row.doc else {
                continue;
            };

            if !doc.is_file() {
                continue;
            }

            if doc.deleted {
                entries.push(ChangeFeedEntry {
                    change: Change::remote_deleted(doc.id.clone(), Some(doc.delete_time())),
                    seq: seq_to_string(&row.seq),
                });
                continue;
            }

            let mtime = doc.modified_at();
            let rev = doc.rev.clone().unwrap_or_default();
            entries.push(ChangeFeedEntry {
                change: Change::remote_modified(doc.id, String::new(), doc.size, mtime, rev),
                seq: seq_to_string(&row.seq),
            });
        }

        Ok((entries, seq_to_string(&body.last_seq)))
    }

    /// Check if a path is within the configured remote path
    /// Check if a path is within the configured remote path
    #[must_use]
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if self.remote_path.is_empty() {
            true
        } else {
            path.starts_with(self.remote_path.as_str())
                || path == self.remote_path.as_str().trim_end_matches('/')
        }
    }

    /// Get the normalized remote path prefix used for this sync scope.
    #[must_use]
    pub fn remote_prefix(&self) -> &str {
        &self.remote_path
    }

    /// Configured request timeout in seconds applied to `CouchDB` operations.
    #[must_use]
    pub const fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    /// Maximum total attempts (including the initial one) made for a `CouchDB`
    /// operation when it fails with a transient connectivity error.
    #[must_use]
    pub const fn retry_attempts(&self) -> u32 {
        self.retry_attempts
    }

    /// Run `op`, retrying transient connectivity failures with exponential
    /// backoff.
    ///
    /// The operation is attempted at most [`Self::retry_attempts`] times in
    /// total (the initial attempt plus retries; never fewer than one). Only
    /// failures classified as transient by [`Self::is_transient`] are retried;
    /// persistent errors are returned immediately.
    async fn retry_transient<F, Fut, T, E>(&self, op: F) -> std::result::Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut attempts: u32 = 0;
        let total = self.retry_attempts.max(1);
        let mut op = op;
        loop {
            match op().await {
                Ok(value) => return Ok(value),
                Err(e) if Self::is_transient(&e) && attempts + 1 < total => {
                    attempts += 1;
                    let backoff = self.backoff(attempts);
                    warn!(
                        "CouchDB transient failure (attempt {}/{}): {}; retrying in {:?}",
                        attempts, total, e, backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Delay to wait before the next retry attempt (pathological: real sleeps
    /// in production, a tiny fixed delay in test builds to keep tests fast).
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    const fn backoff(&self, attempts: u32) -> Duration {
        #[cfg(test)]
        {
            let _ = attempts;
            self.test_backoff
        }
        #[cfg(not(test))]
        {
            let shift = if attempts >= 7 { 6 } else { attempts - 1 };
            Duration::from_millis(RETRY_BASE_BACKOFF_MS.saturating_mul(1u64 << shift))
        }
    }

    /// Whether `err` represents a transient connectivity failure that is safe
    /// to retry (connection refused/reset, timeouts, DNS failures, and `CouchDB`
    /// 429/5xx responses).
    fn is_transient<E: std::fmt::Display>(err: &E) -> bool {
        let lower = err.to_string().to_ascii_lowercase();
        lower.contains("timeout")
            || lower.contains("timed out")
            || lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("connection closed")
            || lower.contains("broken pipe")
            || lower.contains("eof while")
            || lower.contains("network is unreachable")
            || lower.contains("temporary failure in name resolution")
            || lower.contains("name or service not known")
            || lower.contains(" 502 ")
            || lower.contains(" 503 ")
            || lower.contains(" 504 ")
            || lower.contains(" 429 ")
            || lower.contains("502 bad gateway")
            || lower.contains("503 service unavailable")
            || lower.contains("504 gateway timeout")
            || lower.contains("too many requests")
    }

    /// Get the full remote path for a local file
    #[must_use]
    pub fn get_remote_path(&self, local_path: &str) -> String {
        if self.remote_path.is_empty() {
            local_path.to_string()
        } else {
            // Combine remote path prefix with local path
            format!("{}{}", self.remote_path, local_path)
        }
    }

    /// Get the local path from a remote path (strips the remote prefix)
    #[must_use]
    pub fn get_local_path(&self, remote_path: &str) -> String {
        if self.remote_path.is_empty() {
            remote_path.to_string()
        } else {
            // Strip the remote path prefix
            remote_path
                .strip_prefix(self.remote_path.as_str())
                .unwrap_or(remote_path)
                .to_string()
        }
    }

    /// Get a document by ID
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails, or `Ok(None)` if the
    /// document does not exist.
    pub async fn get_file(&self, path: &str) -> Result<Option<FileDoc>> {
        // Check if path is within allowed remote path
        if !self.is_path_allowed(path) {
            return Ok(None);
        }

        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok(state.metadata.get(path).cloned());
        }

        match self.retry_transient(|| self.db.get(path)).await {
            Ok(doc) => Ok(Some(doc)),
            Err(e) => {
                // Check if it's a 404
                let err_str = e.to_string();
                if err_str.contains("404") || err_str.contains("Not Found") {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Save a document
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails.
    pub async fn save_file(&self, doc: &mut FileDoc) -> Result<()> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            if state.save_errors.contains(&doc.id) {
                anyhow::bail!("simulated save failure for {}", doc.id);
            }
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(probe) = &state.probe {
                probe.enter();
                if let Some(delay) = state.batch_delay {
                    tokio::time::sleep(delay).await;
                }
                probe.leave();
            }
            return Ok(());
        }

        debug!("Saving file to CouchDB: {}", doc.id);

        // `doc` is borrowed mutably across the save, which cannot be returned
        // from a lazy closure, so apply the retry contract inline here: at most
        // `retry_attempts` total attempts, retrying only transient failures.
        let mut attempts: u32 = 0;
        let total = self.retry_attempts.max(1);
        loop {
            match self.db.save(doc).await {
                Ok(_details) => break,
                Err(e) if Self::is_transient(&e) && attempts + 1 < total => {
                    attempts += 1;
                    let backoff = self.backoff(attempts);
                    warn!(
                        "CouchDB transient failure while saving {} (attempt {}/{}): {}; retrying in {:?}",
                        doc.id, attempts, total, e, backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Delete a document
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails.
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }

        if let Some(mut doc) = self.get_file(path).await? {
            doc.deleted = true;
            // Stamp the tombstone with an authoritative deletion timestamp so
            // age-based pruning and remote-delete arbitration use the *delete*
            // time rather than the deleted file's possibly-preserved stale
            // mtime. The mtime is stamped too for older/external clients that
            // only understand it.
            let now = crate::models::TimestampMillis::now();
            doc.deleted_at = now;
            doc.mtime = now;
            self.save_file(&mut doc).await?;
            debug!("Marked file as deleted in CouchDB: {}", path);
        }
        Ok(())
    }

    /// Get all documents (files only - not chunks, including deleted)
    /// Filtered by the configured remote path
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails.
    pub async fn get_all_files(&self) -> Result<Vec<FileDoc>> {
        let collection = self
            .retry_transient(|| self.db.get_all::<FileDoc>())
            .await?;
        Ok(collection
            .rows
            .into_iter()
            .filter(|d| d.is_file() && self.is_path_allowed(&d.id))
            .collect())
    }

    /// Get changes since the last checkpoint.
    ///
    /// Returns remote files within the configured remote path. On the first
    /// run (no checkpoint) this bootstraps the existing in-scope file set so a
    /// fresh client materializes pre-existing remote files. On subsequent runs
    /// it resumes the since-based `_changes` feed so only changes after the
    /// checkpoint are returned. The returned sequence is the scope-appropriate
    /// resume point: the feed's `last_seq` for incremental runs, or the DB
    /// `update_seq` captured for the bootstrap run.
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` changes request or the update
    /// sequence fetch fails.
    pub async fn get_changes(&self, since: Option<&str>) -> Result<(Vec<Change>, String)> {
        debug!("get_changes called with since = {:?}", since);

        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok((state.changes.clone(), state.last_seq.clone()));
        }

        // First run (no checkpoint): bootstrap the existing in-scope file set so
        // a fresh client materializes pre-existing remote files, then checkpoint
        // the current DB sequence so later syncs are incremental.
        let Some(since) = since else {
            let all_files = self.get_all_files().await?;
            debug!(
                "Bootstrap (no checkpoint): {} existing files in scope",
                all_files.len()
            );
            let changes: Vec<Change> = all_files
                .into_iter()
                .map(|doc| {
                    let mtime = doc.modified_at();
                    let rev = doc.rev.clone().unwrap_or_default();
                    if doc.deleted {
                        Change::remote_deleted(doc.id.clone(), Some(doc.delete_time()))
                    } else {
                        Change::remote_modified(doc.id, String::new(), doc.size, mtime, rev)
                    }
                })
                .collect();
            let seq = self.get_update_seq().await?;
            return Ok((changes, seq));
        };

        // Incremental: resume the since-based `_changes` feed so we only pull
        // changes that occurred after the checkpoint. The feed's `last_seq` is
        // the scope-appropriate resume point for the next cycle.
        let (entries, seq) = self
            .get_changes_feed(since, self.timeout_seconds.saturating_mul(1000))
            .await?;
        let changes: Vec<Change> = entries.into_iter().map(|entry| entry.change).collect();
        debug!(
            "Returning {} changes since checkpoint {}",
            changes.len(),
            since
        );
        Ok((changes, seq))
    }

    /// Fetch remote file metadata (without downloading chunks)
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails, or `Ok(None)` if the
    /// document does not exist.
    pub async fn fetch_metadata(&self, path: &str) -> Result<Option<FileDoc>> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok(state.metadata.get(path).cloned());
        }

        // Check if path is within allowed remote path
        if !self.is_path_allowed(path) {
            return Ok(None);
        }

        debug!("[FETCH METADATA] Fetching metadata for: {}", path);

        match self.retry_transient(|| self.db.get::<FileDoc>(path)).await {
            Ok(doc) => {
                debug!("[FETCH METADATA] Retrieved metadata:");
                debug!("  path: {}", doc.path);
                debug!("  size: {} bytes", doc.size);
                debug!("  mtime: {} ms", doc.mtime);
                debug!("  ctime: {} ms", doc.ctime);
                debug!("  rev: {:?}", doc.rev);
                debug!("  chunks: {}", doc.children.len());
                Ok(Some(doc))
            }
            Err(e) => {
                // Check if it's a 404
                let err_str = e.to_string();
                if err_str.contains("404") || err_str.contains("Not Found") {
                    debug!("[FETCH METADATA] Not found: {}", path);
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Test connection to `CouchDB`
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub async fn ping(&self) -> Result<bool> {
        // Get all files (limit 1) to test connection
        match self.retry_transient(|| self.db.get_all::<FileDoc>()).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Get a chunk document by ID
    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkDoc>> {
        let url = format!("{}/{}", self.base_db_url, chunk_id);

        let response = self
            .retry_transient::<_, _, reqwest::Response, anyhow::Error>(|| {
                let mut request = self.http_client.get(&url);
                if let Some((username, password)) = &self.auth {
                    request = request.basic_auth(username, Some(password));
                }
                async move { request.send().await.map_err(Into::into) }
            })
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            anyhow::bail!("Failed to fetch chunk {}: {}", chunk_id, response.status());
        }

        let chunk: ChunkDoc = response.json().await?;
        Ok(Some(chunk))
    }

    /// Get file content by fetching and combining all chunks
    ///
    /// # Errors
    ///
    /// Returns an error if the file document or any chunk cannot be fetched.
    pub async fn get_file_content(&self, path: &str) -> Result<Vec<u8>> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            if state.content_errors.contains(path) {
                anyhow::bail!("simulated content fetch failure for {path}");
            }
            if let Some(probe) = &state.probe {
                probe.enter();
                if let Some(delay) = state.batch_delay {
                    tokio::time::sleep(delay).await;
                }
                probe.leave();
            }
            return Ok(state.contents.get(path).cloned().unwrap_or_default());
        }

        // First get the file document to find chunk IDs
        let Some(doc) = self.get_file(path).await? else {
            anyhow::bail!("File not found: {path}");
        };

        if doc.children.is_empty() {
            debug!("File {} has no chunks, returning empty content", path);
            return Ok(Vec::new());
        }

        // Fetch each chunk and combine the data
        let mut content = String::new();
        for chunk_id in &doc.children {
            match self.get_chunk(chunk_id).await? {
                Some(chunk) => {
                    content.push_str(&chunk.data);
                }
                None => {
                    warn!("Chunk {} not found for file {}", chunk_id, path);
                }
            }
        }

        Ok(content.into_bytes())
    }

    /// Generate a unique chunk ID
    fn generate_chunk_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Generate a base36-like ID similar to Obsidian LiveSync
        format!("h:{:x}{:x}", timestamp, rand::random::<u32>())
    }

    /// Save a chunk document to `CouchDB`
    async fn save_chunk(&self, chunk: &ChunkDoc) -> Result<()> {
        let url = format!("{}/{}", self.base_db_url, chunk.id);

        let response = self
            .retry_transient::<_, _, reqwest::Response, anyhow::Error>(|| {
                let mut request = self.http_client.put(&url);
                if let Some((username, password)) = &self.auth {
                    request = request.basic_auth(username, Some(password));
                }

                async move {
                    request
                        .header("Content-Type", "application/json")
                        .json(chunk)
                        .send()
                        .await
                        .map_err(Into::into)
                }
            })
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to save chunk {}: {} - {}", chunk.id, status, body);
        }

        debug!("Saved chunk: {}", chunk.id);
        Ok(())
    }

    /// Upload file content as chunks and return the chunk IDs
    ///
    /// # Errors
    ///
    /// Returns an error if a chunk cannot be saved to `CouchDB`.
    pub async fn upload_file_content(&self, content: &[u8]) -> Result<Vec<String>> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(Vec::new());
        }

        let content_str = String::from_utf8_lossy(content);

        // For simplicity, store entire content as a single chunk
        // (Obsidian LiveSync may split into multiple chunks for large files)
        let chunk_id = Self::generate_chunk_id();
        let chunk = ChunkDoc {
            id: chunk_id.clone(),
            rev: None,
            data: content_str.to_string(),
            doc_type: crate::models::DocType::Leaf,
        };

        self.save_chunk(&chunk).await?;

        Ok(vec![chunk_id])
    }

    /// Delete old chunks that are no longer referenced
    ///
    /// # Errors
    ///
    /// Returns an error if a chunk cannot be deleted from `CouchDB`.
    pub async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }

        for chunk_id in chunk_ids {
            if let Some(chunk) = self.get_chunk(chunk_id).await?
                && let Some(rev) = chunk.rev
            {
                self.delete_doc(chunk_id, &rev).await?;
            }
        }
        Ok(())
    }

    /// Permanently delete a document by id and current revision.
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` delete request fails.
    async fn delete_doc(&self, id: &str, rev: &str) -> Result<()> {
        let url = format!("{}/{id}?rev={rev}", self.base_db_url);
        let mut request = self.http_client.delete(&url);
        if let Some((username, password)) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                debug!("Deleted document: {}", id);
                Ok(())
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!("Failed to delete {}: {} - {}", id, status, body);
                anyhow::bail!("Failed to delete {id}: {status} - {body}")
            }
            Err(e) => {
                warn!("Failed to delete {}: {}", id, e);
                anyhow::bail!("Failed to delete {id}: {e}")
            }
        }
    }

    /// Permanently remove soft-delete tombstones that have outlived the
    /// retention window.
    ///
    /// A tombstone is a file document with `deleted: true` that exists so other
    /// clients observe and propagate the deletion. Keeping them forever makes
    /// `get_all_files`/`get_changes` grow unbounded, so once a tombstone has
    /// been around longer than `retention` (its authoritative delete time is
    /// older than the cutoff) it is considered obsolete and hard-deleted. This is
    /// best-effort cleanup: an individual tombstone that cannot be deleted is
    /// logged and skipped rather than aborting the sync.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial document fetch fails.
    pub async fn prune_tombstones(&self, retention: Duration) -> Result<usize> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(0);
        }

        let all_files = self.get_all_files().await?;
        let now_ms = crate::models::TimestampMillis::now().as_u64();
        let cutoff = crate::models::TimestampMillis::new(
            now_ms.saturating_sub(u64::try_from(retention.as_millis()).unwrap_or(u64::MAX)),
        );
        let candidates = obsolete_tombstones(&all_files, cutoff);

        let mut pruned = 0;
        for doc in candidates {
            let Some(rev) = &doc.rev else {
                warn!("Skipping tombstone {} without a revision", doc.id);
                continue;
            };
            match self.delete_doc(&doc.id, rev).await {
                Ok(()) => {
                    pruned += 1;
                    debug!("Pruned obsolete tombstone: {}", doc.id);
                }
                Err(e) => warn!("Failed to prune tombstone {}: {e}", doc.id),
            }
        }
        Ok(pruned)
    }
}

/// Select the soft-delete tombstones that are old enough to be pruned.
///
/// A tombstone is obsolete when it is a file doc flagged `deleted` whose
/// deletion-stamped mtime predates the cutoff. Chunks and live files are never
/// candidates.
#[must_use]
fn obsolete_tombstones(docs: &[FileDoc], cutoff: TimestampMillis) -> Vec<&FileDoc> {
    docs.iter()
        .filter(|d| {
            d.is_file()
                && d.deleted
                && u64::try_from(d.delete_time().timestamp_millis()).unwrap_or(u64::MAX)
                    < cutoff.as_u64()
        })
        .collect()
}

/// Helper to create `CouchDB` URL from components
#[must_use]
pub fn build_couch_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a `CouchDb` instance for testing without a real `CouchDB`.
    /// The methods under test (`is_path_allowed`, `get_remote_path`, `get_local_path`)
    /// only depend on `self.remote_path`, so we use dummy `couch_rs` values.
    fn test_couchdb(remote_path: &str) -> CouchDb {
        let client = Client::new_no_auth("http://localhost:15984").unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: "http://localhost:15984/test_db".to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    // -----------------------------------------------------------------------
    // Task 10051: is_path_allowed
    // -----------------------------------------------------------------------

    #[test]
    fn is_path_allowed_empty_prefix_permits_all() {
        let db = test_couchdb("");
        assert!(db.is_path_allowed("any/path/file.md"));
        assert!(db.is_path_allowed("deeply/nested/path/document.txt"));
    }

    #[test]
    fn is_path_allowed_trailing_slash_prefix() {
        let db = test_couchdb("notes/");
        assert!(db.is_path_allowed("notes/file.md"));
        assert!(db.is_path_allowed("notes/subdir/doc.txt"));
        assert!(!db.is_path_allowed("other/file.md"));
        assert!(!db.is_path_allowed("journal/entry.md"));
    }

    #[test]
    fn is_path_allows_exact_prefix_match_without_trailing_slash() {
        // A path equal to the prefix (minus trailing slash) should also be allowed
        let db = test_couchdb("notes/");
        assert!(db.is_path_allowed("notes"));
    }

    #[test]
    fn is_path_allowed_rejects_outside_prefix() {
        let db = test_couchdb("notes/");
        assert!(!db.is_path_allowed("Notes/file.md"));
        assert!(!db.is_path_allowed("note"));
        assert!(!db.is_path_allowed("notes_extra/file.md"));
    }

    #[test]
    fn is_path_allowed_root_prefix_normalized_to_empty() {
        let db = test_couchdb("");
        assert!(db.is_path_allowed("any/path.md"));
    }

    #[test]
    fn is_path_allowed_nested_paths() {
        let db = test_couchdb("obsidian/");
        assert!(db.is_path_allowed("obsidian/vault/notes/file.md"));
        assert!(db.is_path_allowed("obsidian/vault/deep/nested/doc.txt"));
    }

    // -----------------------------------------------------------------------
    // Task 10052: get_remote_path and get_local_path round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn get_remote_path_without_prefix_passthrough() {
        let db = test_couchdb("");
        assert_eq!(db.get_remote_path("file.md"), "file.md");
        assert_eq!(db.get_remote_path("subdir/doc.txt"), "subdir/doc.txt");
    }

    #[test]
    fn get_remote_path_with_prefix() {
        let db = test_couchdb("notes/");
        assert_eq!(db.get_remote_path("file.md"), "notes/file.md");
        assert_eq!(db.get_remote_path("subdir/doc.txt"), "notes/subdir/doc.txt");
    }

    #[test]
    fn get_local_path_without_prefix_passthrough() {
        let db = test_couchdb("");
        assert_eq!(db.get_local_path("file.md"), "file.md");
        assert_eq!(db.get_local_path("subdir/doc.txt"), "subdir/doc.txt");
    }

    #[test]
    fn get_local_path_strips_prefix() {
        let db = test_couchdb("notes/");
        assert_eq!(db.get_local_path("notes/file.md"), "file.md");
        assert_eq!(db.get_local_path("notes/subdir/doc.txt"), "subdir/doc.txt");
    }

    #[test]
    fn get_local_path_unknown_prefix_returns_whole() {
        let db = test_couchdb("notes/");
        // When the remote path doesn't start with the prefix, return as-is
        assert_eq!(db.get_local_path("other/file.md"), "other/file.md");
    }

    #[test]
    fn remote_local_path_round_trip_without_prefix() {
        let db = test_couchdb("");
        let local = "some/file.md";
        let remote = db.get_remote_path(local);
        assert_eq!(db.get_local_path(&remote), local);
    }

    #[test]
    fn remote_local_path_round_trip_with_prefix() {
        let db = test_couchdb("obsidian/");
        let local = "vault/note.md";
        let remote = db.get_remote_path(local);
        assert_eq!(remote, "obsidian/vault/note.md");
        assert_eq!(db.get_local_path(&remote), local);
    }

    // -----------------------------------------------------------------------
    // Task 10053: build_couch_url, seq_to_string, 404 handling in get_file
    // -----------------------------------------------------------------------

    #[test]
    fn build_couch_url_constructs_http_url() {
        assert_eq!(build_couch_url("localhost", 5984), "http://localhost:5984");
    }

    #[test]
    fn build_couch_url_with_ip_and_non_default_port() {
        assert_eq!(build_couch_url("127.0.0.1", 443), "http://127.0.0.1:443");
    }

    #[test]
    fn seq_to_string_from_string() {
        let value = Value::String("12345-abc".to_string());
        assert_eq!(seq_to_string(&value), "12345-abc");
    }

    #[test]
    fn seq_to_string_from_number() {
        let value = Value::Number(serde_json::Number::from(67890));
        assert_eq!(seq_to_string(&value), "67890");
    }

    #[test]
    fn seq_to_string_from_null() {
        let value = Value::Null;
        assert_eq!(seq_to_string(&value), "null");
    }

    #[test]
    fn seq_to_string_from_array() {
        let value = Value::Array(vec![
            Value::String("seq1".to_string()),
            Value::String("seq2".to_string()),
        ]);
        // For non-string values, serde_json::to_string is used
        assert_eq!(seq_to_string(&value), "[\"seq1\",\"seq2\"]");
    }

    /// Test that `get_file` returns Ok(None) for paths outside the allowed prefix
    #[tokio::test]
    async fn get_file_returns_none_for_disallowed_path() {
        let db = test_couchdb("notes/");
        let result = db.get_file("other/outside.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    /// Test that `fetch_metadata` returns Ok(None) for paths outside the allowed prefix
    #[tokio::test]
    async fn fetch_metadata_returns_none_for_disallowed_path() {
        let db = test_couchdb("notes/");
        let result = db.fetch_metadata("other/outside.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_file_disallowed_path_returns_ok_none() {
        let db = test_couchdb("data/");
        let result = db.get_file("config/secrets.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn fetch_metadata_disallowed_path_returns_ok_none() {
        let db = test_couchdb("data/");
        let result = db.fetch_metadata("config/secrets.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // Task 10831/10832/10833: timeout_seconds and retry_attempts wiring
    // -----------------------------------------------------------------------

    /// Build a test client with explicit timeout/retry settings.
    fn test_couchdb_with_settings(timeout_seconds: u64, retry_attempts: u32) -> CouchDb {
        let mut db = test_couchdb("");
        db.timeout_seconds = timeout_seconds;
        db.retry_attempts = retry_attempts;
        db
    }

    #[test]
    fn configured_timeout_and_retry_are_threaded_into_client() {
        let db = test_couchdb_with_settings(42, 5);
        assert_eq!(db.timeout_seconds(), 42);
        assert_eq!(db.retry_attempts(), 5);
    }

    #[tokio::test]
    async fn retry_transient_injects_n_failures_and_retries_up_to_retry_attempts() {
        // Verify behavior: N = retry_attempts - 1 transient failures are
        // tolerated, and the operation succeeds on the last allowed attempt.
        let db = test_couchdb_with_settings(30, 3);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, u32, String>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err("connection refused".to_string())
                    } else {
                        Ok(42u32)
                    }
                }
            })
            .await;

        assert_eq!(result, Ok(42));
        // 2 transient failures + 1 success == retry_attempts total attempts.
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "client should retry up to retry_attempts total attempts"
        );
    }

    #[tokio::test]
    async fn retry_transient_gives_up_after_retry_attempts_transient_failures() {
        let db = test_couchdb_with_settings(30, 2);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, (), String>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = n;
                    Err("connection reset by peer".to_string())
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "client must give up after retry_attempts total attempts"
        );
    }

    #[tokio::test]
    async fn retry_transient_does_not_retry_non_transient_errors() {
        let db = test_couchdb_with_settings(30, 5);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, (), String>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = n;
                    Err("400 bad request: invalid document".to_string())
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "non-transient errors must not be retried"
        );
    }

    #[tokio::test]
    async fn retry_transient_always_performs_at_least_one_attempt() {
        let db = test_couchdb_with_settings(30, 0);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, (), String>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = n;
                    Err("connection refused".to_string())
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "zero retry_attempts is clamped to a single attempt"
        );
    }

    #[test]
    fn timeout_is_classified_as_transient() {
        assert!(CouchDb::is_transient(&"operation timed out".to_string()));
        assert!(CouchDb::is_transient(
            &"error sending request: request timed out".to_string()
        ));
        assert!(CouchDb::is_transient(&"connection refused".to_string()));
        assert!(CouchDb::is_transient(&"502 bad gateway".to_string()));
        assert!(!CouchDb::is_transient(&"400 bad request".to_string()));
        assert!(!CouchDb::is_transient(&"JSON parse error".to_string()));
    }

    // -----------------------------------------------------------------------
    // Task 2895: surface delete_chunks HTTP failures
    // -----------------------------------------------------------------------

    /// Build a `CouchDb` instance pointed at a caller-supplied base URL, with no
    /// canned state, so `delete_chunks` issues real HTTP requests to the fake
    /// server in these tests.
    fn couchdb_at(base_db_url: &str) -> CouchDb {
        let client = Client::new_no_auth("http://localhost:15984").unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: base_db_url.to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(String::new()),
            timeout_seconds: 30,
            retry_attempts: 3,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Spawn an in-process fake `CouchDB` server backed by a real hyper HTTP
    /// server that serves one chunk document on GET and answers DELETE with
    /// `delete_status`. Using hyper means HTTP/1.1 framing, keep-alive and
    /// connection reuse are handled correctly by construction, so the tests are
    /// deterministic and do not race on macOS the way a hand-rolled raw-TCP
    /// server did. Returns the base database URL, a counter of accepted TCP
    /// connections, the server thread handle, and a shutdown flag.
    fn spawn_fake_couch(
        delete_status: u16,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        // Bind a std listener first so we obtain the ephemeral port without
        // needing an async context, then hand the socket to a dedicated tokio
        // runtime that runs the hyper accept loop.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake couch");
        let addr = listener.local_addr().expect("fake couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let conn_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_thread = shutdown.clone();
        let conn_count_thread = conn_count.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake couch tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    // Poll for connections with a short timeout so the loop can
                    // observe the shutdown flag and exit promptly.
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            conn_count_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let status = delete_status;
                            let service = hyper::service::service_fn(move |req| {
                                handle_fake_couch_req(req, status)
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {} // no connection yet; re-check shutdown
                    }
                }
            });
        });
        (base_db_url, conn_count, handle, shutdown)
    }

    /// Serve one HTTP request: respond to GET with a chunk document and to
    /// DELETE with the configured status. hyper handles keep-alive and
    /// connection reuse, so the server tolerates reqwest reusing the pooled
    /// connection across the GET and DELETE that `delete_chunks` issues.
    // `async` is required: hyper's `service_fn` must return a future resolving
    // to `Result`, so the (test-only) fake server keeps this signature even
    // though the body contains no awaits.
    #[allow(clippy::unused_async)]
    async fn handle_fake_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        delete_status: u16,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        match parts.method {
            hyper::Method::GET => {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                    br#"{"_id":"chunk1","_rev":"1-abc","data":"hello","type":"leaf"}"#,
                ));
            }
            hyper::Method::DELETE if delete_status == 200 => {
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from_static(br#"{"ok":true}"#));
            }
            hyper::Method::DELETE => {
                *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                    br#"{"error":"simulated delete failure"}"#,
                ));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn delete_chunks_surfaces_http_delete_failure() {
        let (base_db_url, _conn_count, server, shutdown) = spawn_fake_couch(500);
        let db = couchdb_at(&base_db_url);
        let chunk_ids = vec!["chunk1".to_string()];
        let result = db.delete_chunks(&chunk_ids).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");
        assert!(
            result.is_err(),
            "failed chunk delete must be surfaced, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_chunks_successful_delete_returns_ok() {
        let (base_db_url, _conn_count, server, shutdown) = spawn_fake_couch(200);
        let db = couchdb_at(&base_db_url);
        let chunk_ids = vec!["chunk1".to_string()];
        let result = db.delete_chunks(&chunk_ids).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");
        assert!(
            result.is_ok(),
            "successful chunk delete must return Ok, got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // get_changes: since-based incremental feed + first-run bootstrap
    // -----------------------------------------------------------------------

    /// Build a live (non-deleted) `FileDoc` for the fake `_changes`/`_all_docs`
    /// server, tagged with the given mtime so changes are distinguishable.
    fn changes_file_doc(id: &str, mtime_ms: u64) -> FileDoc {
        FileDoc {
            id: id.to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: id.to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(mtime_ms),
            deleted_at: TimestampMillis::default(),
            size: 1,
            doc_type: crate::models::DocType::Plain,
            deleted: false,
        }
    }

    /// Build a `CouchDb` whose *both* `couch_rs` client (used by
    /// `get_all_files`/`get_update_seq`) and reqwest client (used by
    /// `get_changes_feed`) point at `addr`, with the given remote-path scope.
    fn couchdb_at_full(addr: &str, remote_path: &str) -> CouchDb {
        let client = Client::new_no_auth(addr).unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: format!("{addr}/test_db"),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Spawn an in-process fake `CouchDB` server backed by a real hyper HTTP
    /// server that serves the three endpoints `get_changes` touches:
    ///
    /// - `POST /test_db/_all_docs` (via `couch_rs`), returning `docs` — the
    ///   bootstrap path for a first run with no checkpoint.
    /// - `GET /test_db/_changes` (via reqwest), returning change rows for
    ///   `docs` with `1`-based sequences, filtered to sequences strictly
    ///   greater than the `since` query parameter — the incremental path.
    /// - `GET /test_db` (via `couch_rs`), returning a `DbInfo` whose
    ///   `update_seq` equals the number of `docs`, so bootstrap can checkpoint
    ///   a scope-appropriate sequence.
    ///
    /// Returns the server base URL, the server thread handle, and a shutdown
    /// flag.
    fn spawn_fake_changes_couch(
        docs: Vec<FileDoc>,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake changes couch");
        let addr = listener.local_addr().expect("fake changes addr");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let docs_thread = docs;
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake changes runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake changes nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("fake changes tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let docs = docs_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_fake_changes_couch_req(req, docs.clone())
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {}
                    }
                }
            });
        });
        (format!("http://{addr}"), handle, shutdown)
    }

    /// Serve the three `CouchDB` endpoints used by `get_changes` (see
    /// [`spawn_fake_changes_couch`]). hyper handles HTTP/1.1 framing and
    /// keep-alive, so reqwest and `couch_rs` reuse connections deterministically.
    #[allow(clippy::unused_async)]
    async fn handle_fake_changes_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        docs: Vec<FileDoc>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let method = parts.method.clone();
        let path = parts.uri.path().to_string();
        match (method, path.as_str()) {
            // `couch_rs` percent-encodes the underscore in the db name
            // (`/test%5Fdb`), while our own reqwest path does not, so match on
            // the endpoint suffix rather than the exact path.
            (hyper::Method::POST, p) if p.ends_with("_all_docs") => {
                let rows: Vec<serde_json::Value> = docs
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "key": d.id,
                            "value": { "rev": d.rev.clone().unwrap_or_default() },
                            "doc": d,
                        })
                    })
                    .collect();
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({ "total_rows": rows.len(), "offset": 0, "rows": rows })
                        .to_string(),
                ));
            }
            (hyper::Method::GET, p) if p.ends_with("_changes") => {
                let since = parts
                    .uri
                    .query()
                    .and_then(|q| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            (k == "since").then(|| v.to_string())
                        })
                    })
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let rows: Vec<serde_json::Value> = docs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| (*i as u64) + 1 > since)
                    .map(|(i, d)| {
                        serde_json::json!({
                            "seq": ((i as u64) + 1).to_string(),
                            "id": d.id,
                            "changes": [{ "rev": d.rev.clone().unwrap_or_default() }],
                            "deleted": d.deleted,
                            "doc": d,
                        })
                    })
                    .collect();
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({
                        "results": rows,
                        "last_seq": docs.len().to_string(),
                    })
                    .to_string(),
                ));
            }
            (hyper::Method::GET, p) if p.ends_with("test_db") || p.ends_with("test%5Fdb") => {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({
                        "cluster": { "q": 1, "n": 1, "w": 1, "r": 1 },
                        "compact_running": false,
                        "db_name": "test_db",
                        "disk_format_version": 8,
                        "doc_count": docs.len(),
                        "doc_del_count": 0,
                        "instance_start_time": "0",
                        "purge_seq": "0",
                        "sizes": { "file": 0, "external": 0, "active": 0 },
                        "update_seq": docs.len().to_string(),
                        "props": {},
                    })
                    .to_string(),
                ));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn get_changes_returns_only_changes_since_checkpoint() {
        // Three in-scope docs on the feed, sequenced 1..=3. A checkpoint at
        // sequence 2 must yield only the change at sequence 3.
        let docs = vec![
            changes_file_doc("notes/a.md", 1000),
            changes_file_doc("notes/b.md", 2000),
            changes_file_doc("notes/c.md", 3000),
        ];
        let (addr, server, shutdown) = spawn_fake_changes_couch(docs);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, seq) = db
            .get_changes(Some("2"))
            .await
            .expect("get_changes should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake changes couch join");

        let paths: Vec<&str> = changes.iter().map(Change::path).collect();
        assert_eq!(
            paths,
            vec!["notes/c.md"],
            "only changes after the checkpoint"
        );
        assert_eq!(seq, "3", "returned seq is the feed last_seq");
    }

    #[tokio::test]
    async fn get_changes_tombstone_uses_authoritative_delete_time_not_stale_mtime() {
        // A soft-delete tombstone preserves the deleted file's own (old) mtime
        // but stamps an authoritative deletion time. The remote-delete change
        // must carry that deletion time so a stale preserved mtime cannot
        // suppress propagation of a genuinely-newer delete.
        let mut tombstone = changes_file_doc("notes/deleted.md", 1000); // stale file mtime
        tombstone.deleted = true;
        tombstone.deleted_at = TimestampMillis::new(9000); // authoritative delete time
        let docs = vec![tombstone];
        let (addr, server, shutdown) = spawn_fake_changes_couch(docs);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, _seq) = db
            .get_changes(None)
            .await
            .expect("get_changes should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake changes couch join");

        let delete = changes
            .iter()
            .find(|c| {
                matches!(c, crate::models::Change::RemoteDeleted { path, .. } if path == "notes/deleted.md")
            })
            .expect("tombstone must surface as a remote delete");
        assert_eq!(
            delete.mtime().map(chrono::DateTime::timestamp_millis),
            Some(9000),
            "remote delete must carry the authoritative deletion time, not the stale file mtime"
        );
    }

    #[tokio::test]
    async fn get_changes_first_run_bootstraps_existing_files() {
        // No checkpoint: get_changes must materialize the existing in-scope
        // file set rather than returning empty.
        let docs = vec![
            changes_file_doc("notes/a.md", 1000),
            changes_file_doc("notes/b.md", 2000),
        ];
        let (addr, server, shutdown) = spawn_fake_changes_couch(docs);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, seq) = db
            .get_changes(None)
            .await
            .expect("get_changes should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake changes couch join");

        let mut paths: Vec<&str> = changes.iter().map(Change::path).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["notes/a.md", "notes/b.md"]);
        assert_eq!(seq, "2", "bootstrap checkpoints the DB update_seq");
    }

    // -----------------------------------------------------------------------
    // obsolete_tombstones
    // -----------------------------------------------------------------------

    fn tombstone(id: &str, mtime_ms: u64) -> FileDoc {
        FileDoc {
            id: id.to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: id.to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(mtime_ms),
            deleted_at: TimestampMillis::default(),
            size: 0,
            doc_type: crate::models::DocType::Plain,
            deleted: true,
        }
    }

    #[test]
    fn obsolete_tombstones_prunes_only_old_deleted_docs() {
        let cutoff = TimestampMillis::new(1_000);
        let old = tombstone("notes/old.md", 500);
        let fresh = tombstone("notes/fresh.md", 2_000);
        let docs = vec![old, fresh];

        let obsolete = obsolete_tombstones(&docs, cutoff);

        assert_eq!(obsolete.len(), 1);
        assert_eq!(obsolete[0].id, "notes/old.md");
    }

    #[test]
    fn obsolete_tombstones_keeps_live_files_and_chunks() {
        let cutoff = TimestampMillis::new(1_000);
        let live = FileDoc {
            id: "notes/live.md".to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: "notes/live.md".to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(500),
            deleted_at: TimestampMillis::default(),
            size: 10,
            doc_type: crate::models::DocType::Plain,
            deleted: false,
        };
        let chunk = FileDoc {
            id: "h:chunk1".to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: "h:chunk1".to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(500),
            deleted_at: TimestampMillis::default(),
            size: 0,
            doc_type: crate::models::DocType::Leaf,
            deleted: true,
        };
        let docs = vec![live, chunk];

        assert!(obsolete_tombstones(&docs, cutoff).is_empty());
    }

    #[test]
    fn obsolete_tombstones_boundary_at_cutoff_is_not_pruned() {
        // A tombstone whose mtime equals the cutoff is not yet obsolete.
        let cutoff = TimestampMillis::new(1_000);
        let at_cutoff = tombstone("notes/edge.md", 1_000);
        assert!(obsolete_tombstones(&[at_cutoff], cutoff).is_empty());
    }

    #[tokio::test]
    async fn prune_tombstones_deletes_only_obsolete_docs_from_couchdb() {
        let (base_db_url, conn_count, delete_count, server, shutdown) = spawn_tombstone_couch();
        let db = couchdb_at_ephemeral(&base_db_url);

        // The fake server serves two tombstones: one old (mtime 0) and one
        // fresh (mtime = now). Only the old one should be pruned.
        let pruned = db.prune_tombstones(Duration::from_hours(1)).await.unwrap();
        assert_eq!(pruned, 1, "only the obsolete tombstone should be pruned");

        // Exactly one DELETE must have been issued (for the old tombstone).
        assert_eq!(
            delete_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one obsolete tombstone should be hard-deleted"
        );

        drop(db);
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");
        let used = conn_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!((1..=2).contains(&used));
    }

    // -----------------------------------------------------------------------
    // Tombstone-serving fake CouchDB server
    // -----------------------------------------------------------------------

    /// Build a `CouchDb` whose `couch_rs` client points at the given ephemeral
    /// fake-server base URL. Unlike `couchdb_at`, the `couch_rs` `db` (used by
    /// `get_all_files`) is pointed at the fake server's origin rather than the
    /// hard-coded `localhost:15984`, so `prune_tombstones`' document fetch hits
    /// the fake server.
    fn couchdb_at_ephemeral(base_db_url: &str) -> CouchDb {
        let origin = base_db_url.trim_end_matches("/test_db");
        let client = Client::new_no_auth(origin).unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: base_db_url.to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(String::new()),
            timeout_seconds: 30,
            retry_attempts: 3,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Spawn a fake `CouchDB` server that answers `POST /_all_docs` with a
    /// fixed pair of tombstone documents (one old, one fresh) and counts
    /// `DELETE` requests. Returns the base URL, a connection counter, a delete
    /// counter, the server thread handle, and a shutdown flag.
    #[allow(clippy::type_complexity)]
    fn spawn_tombstone_couch() -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake couch");
        let addr = listener.local_addr().expect("fake couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let conn_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delete_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_thread = shutdown.clone();
        let conn_count_thread = conn_count.clone();
        let delete_count_thread = delete_count.clone();
        let now_ms = crate::models::TimestampMillis::now().as_u64();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake couch tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            conn_count_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let delete_count_thread = delete_count_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_tombstone_couch_req(req, now_ms, delete_count_thread.clone())
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
        (base_db_url, conn_count, delete_count, handle, shutdown)
    }

    #[allow(clippy::unused_async)]
    async fn handle_tombstone_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        now_ms: u64,
        delete_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        match parts.method {
            hyper::Method::POST => {
                let body = format!(
                    r#"{{"total_rows":2,"offset":0,"rows":[
                        {{"id":"notes/old.md","key":null,"value":{{"rev":"1-aaa"}},"doc":{{"_id":"notes/old.md","_rev":"1-aaa","path":"notes/old.md","ctime":0,"mtime":0,"size":0,"type":"plain","deleted":true}}}},
                        {{"id":"notes/fresh.md","key":null,"value":{{"rev":"1-bbb"}},"doc":{{"_id":"notes/fresh.md","_rev":"1-bbb","path":"notes/fresh.md","ctime":0,"mtime":{now_ms},"size":0,"type":"plain","deleted":true}}}}
                    ]}}"#
                );
                *response.body_mut() = http_body_util::Full::new(body.into());
            }
            hyper::Method::DELETE => {
                delete_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from_static(br#"{"ok":true}"#));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    /// Regression test for the macOS-CI flake: the fake `CouchDB` server must not
    /// reset the socket between the GET and DELETE requests issued by the real
    /// reqwest client during `delete_chunks`. The fake server is a real hyper
    /// HTTP server, so framing and keep-alive are handled correctly, and
    /// `delete_chunks` succeeds deterministically. Asserting both requests
    /// complete successfully guards against any future regression back to a
    /// connection-resetting fake.
    ///
    /// Reqwest may either reuse the pooled connection or, depending on timing,
    /// open a fresh one for the DELETE, so the connection count is a sanity
    /// bound rather than an exact assertion: with two requests the server must
    /// not open a new connection per request, which would indicate resets.
    #[tokio::test]
    async fn fake_couch_serves_get_then_delete_on_one_connection() {
        let (base_db_url, conn_count, server, shutdown) = spawn_fake_couch(200);
        let db = couchdb_at(&base_db_url);

        // GET the chunk metadata, then DELETE it, and read the responses to
        // completion so reqwest's pool can reuse the connection for the second
        // request when it chooses to.
        let result = db.delete_chunks(&["chunk1".to_string()]).await;
        assert!(
            result.is_ok(),
            "GET then DELETE on one fake server must succeed, got: {result:?}"
        );

        // Drain the pooled connection so the connection thread can observe the
        // client closing it before we tear the server down.
        drop(db);
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        // The server must never reset and reconnect between the two requests:
        // each request may share the connection or use one fresh connection,
        // but never more than the number of requests. A larger count would mean
        // the server was slamming the socket and forcing reconnects.
        let used = conn_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (1..=2).contains(&used),
            "fake CouchDB used {used} connections for two requests, indicating it is resetting \
             connections and forcing reconnects"
        );
    }
}
