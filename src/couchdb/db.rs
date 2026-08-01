use crate::models::{Change, ChunkDoc, DatabaseName, FileDoc, RemotePath, RemoteState};
use anyhow::Result;
use couch_rs::database::Database;
use couch_rs::Client;
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

/// CouchDB client wrapper
pub struct CouchDb {
    client: Client,
    db: Database,
    http_client: HttpClient,
    base_db_url: String,
    db_name: DatabaseName,
    auth: Option<(String, String)>,
    /// Remote path prefix to sync (e.g., "notes/" or "obsidian/")
    remote_path: RemotePath,
    /// Request timeout in seconds applied to all CouchDB HTTP operations.
    timeout_seconds: u64,
    /// Maximum total attempts (including the initial one) for a CouchDB
    /// operation before giving up on transient connectivity failures.
    retry_attempts: u32,
    /// Test-only canned responses used to exercise the sync pipeline without a
    /// live CouchDB server. Only present in test builds.
    #[cfg(test)]
    test_state: Option<CannedCouch>,
    /// Test-only counter of CouchDB write attempts (save/delete/chunk ops).
    /// Only present in test builds.
    #[cfg(test)]
    test_write_calls: std::sync::atomic::AtomicUsize,
    /// Test-only override for the retry backoff delay. Only present in test
    /// builds so retry unit tests run quickly without real sleeps.
    #[cfg(test)]
    test_backoff: Duration,
}

/// Test-only canned responses for the CouchDB client.
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
}

/// Entry from a CouchDB changes feed
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
    /// Create a new CouchDB client.
    ///
    /// `timeout_seconds` is applied as the request timeout for every CouchDB
    /// HTTP operation (both the direct reqwest client and the `couch_rs`
    /// client). `retry_attempts` caps the total number of attempts made for an
    /// operation when it fails with a transient connectivity error (see
    /// [`Self::retry_transient`]); the operation is attempted at most
    /// `retry_attempts` times in total, with exponential backoff between
    /// attempts.
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
        let base_db_url = if base.ends_with(&format!("/{}", db_name)) {
            base.to_string()
        } else {
            format!("{}/{}", base, db_name)
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

    /// Create a CouchDb instance for testing without connecting to a real server.
    /// Only fields needed for path conversion and sync metadata are populated.
    /// Panics if any method requiring actual CouchDB access is called.
    #[cfg(test)]
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
    pub fn for_test_with_canned(remote_path: &str, canned: CannedCouch) -> Self {
        let mut client = Self::for_test(remote_path);
        client.test_state = Some(canned);
        client
    }

    /// Number of write attempts issued against this test client.
    #[cfg(test)]
    pub fn test_write_calls(&self) -> usize {
        self.test_write_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Fetch changes from CouchDB using the _changes feed (longpoll)
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

            let doc = match row.doc {
                Some(doc) => doc,
                None => continue,
            };

            if !doc.is_file() {
                continue;
            }

            if doc.deleted {
                entries.push(ChangeFeedEntry {
                    change: Change::remote_deleted(doc.id.clone(), Some(doc.modified_at())),
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
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if self.remote_path.is_empty() {
            true
        } else {
            path.starts_with(self.remote_path.as_str())
                || path == self.remote_path.as_str().trim_end_matches('/')
        }
    }

    /// Get the normalized remote path prefix used for this sync scope.
    pub fn remote_prefix(&self) -> &str {
        &self.remote_path
    }

    /// Configured request timeout in seconds applied to CouchDB operations.
    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    /// Maximum total attempts (including the initial one) made for a CouchDB
    /// operation when it fails with a transient connectivity error.
    pub fn retry_attempts(&self) -> u32 {
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
    fn backoff(&self, attempts: u32) -> Duration {
        #[cfg(test)]
        {
            let _ = attempts;
            self.test_backoff
        }
        #[cfg(not(test))]
        {
            Duration::from_millis(
                RETRY_BASE_BACKOFF_MS.saturating_mul(1u64 << (attempts - 1).min(6)),
            )
        }
    }

    /// Whether `err` represents a transient connectivity failure that is safe
    /// to retry (connection refused/reset, timeouts, DNS failures, and CouchDB
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
    pub fn get_remote_path(&self, local_path: &str) -> String {
        if self.remote_path.is_empty() {
            local_path.to_string()
        } else {
            // Combine remote path prefix with local path
            format!("{}{}", self.remote_path, local_path)
        }
    }

    /// Get the local path from a remote path (strips the remote prefix)
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
    pub async fn save_file(&self, doc: &mut FileDoc) -> Result<()> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }

        if let Some(mut doc) = self.get_file(path).await? {
            doc.deleted = true;
            self.save_file(&mut doc).await?;
            debug!("Marked file as deleted in CouchDB: {}", path);
        }
        Ok(())
    }

    /// Get all documents (files only - not chunks, including deleted)
    /// Filtered by the configured remote path
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

    /// Get changes since the last checkpoint
    /// Returns remote files within the configured remote path
    pub async fn get_changes(&self, since: Option<&str>) -> Result<(Vec<Change>, String)> {
        debug!("get_changes called with since = {:?}", since);

        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok((state.changes.clone(), state.last_seq.clone()));
        }

        let all_files = self.get_all_files().await?;
        debug!(
            "Total files in CouchDB (filtered by remote_path): {}",
            all_files.len()
        );

        // If no checkpoint exists (first run), return empty changes
        // The files will be handled as new files on the next sync
        if since.is_none() {
            debug!("No checkpoint found, returning empty changes list");
            let seq = self.get_update_seq().await?;
            return Ok((Vec::new(), seq));
        }

        debug!("Checkpoint found: {}, returning changes", since.unwrap());

        // Return all files (including deleted) as potential changes (sync will compare revs)
        let changes: Vec<Change> = all_files
            .into_iter()
            .map(|doc| {
                let mtime = doc.modified_at();
                let rev = doc.rev.clone().unwrap_or_default();
                if doc.deleted {
                    Change::remote_deleted(doc.id.clone(), Some(doc.modified_at()))
                } else {
                    crate::models::Change::remote_modified(
                        doc.id,
                        String::new(),
                        doc.size,
                        mtime,
                        rev,
                    )
                }
            })
            .collect();

        debug!("Returning {} changes", changes.len());

        // Return the CouchDB update sequence so live sync can resume safely.
        let seq = self.get_update_seq().await?;
        Ok((changes, seq))
    }

    /// Get remote state for comparison
    pub async fn get_remote_state(&self, path: &str) -> Result<Option<RemoteState>> {
        match self.get_file(path).await? {
            Some(doc) => Ok(Some(RemoteState::from(doc))),
            None => Ok(None),
        }
    }

    /// Fetch remote file metadata (without downloading chunks)
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

    /// Test connection to CouchDB
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
    pub async fn get_file_content(&self, path: &str) -> Result<Vec<u8>> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            if state.content_errors.contains(path) {
                anyhow::bail!("simulated content fetch failure for {path}");
            }
            return Ok(state.contents.get(path).cloned().unwrap_or_default());
        }

        // First get the file document to find chunk IDs
        let doc = match self.get_file(path).await? {
            Some(d) => d,
            None => anyhow::bail!("File not found: {}", path),
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

    /// Save a chunk document to CouchDB
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
    pub async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }

        for chunk_id in chunk_ids {
            if let Some(chunk) = self.get_chunk(chunk_id).await? {
                if let Some(rev) = chunk.rev {
                    let url = format!("{}/{}?rev={}", self.base_db_url, chunk_id, rev);
                    let mut request = self.http_client.delete(&url);
                    if let Some((username, password)) = &self.auth {
                        request = request.basic_auth(username, Some(password));
                    }
                    let _ = request.send().await;
                    debug!("Deleted old chunk: {}", chunk_id);
                }
            }
        }
        Ok(())
    }
}

/// Helper to create CouchDB URL from components
pub fn build_couch_url(host: &str, port: u16) -> String {
    format!("http://{}:{}", host, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a CouchDb instance for testing without a real CouchDB.
    /// The methods under test (is_path_allowed, get_remote_path, get_local_path)
    /// only depend on self.remote_path, so we use dummy couch_rs values.
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

    /// Test that get_file returns Ok(None) for paths outside the allowed prefix
    #[tokio::test]
    async fn get_file_returns_none_for_disallowed_path() {
        let db = test_couchdb("notes/");
        let result = db.get_file("other/outside.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    /// Test that fetch_metadata returns Ok(None) for paths outside the allowed prefix
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
}
