use crate::models::{Change, ChangeSource, ChangeType, ChunkDoc, FileDoc, RemoteState};
use anyhow::Result;
use couch_rs::Client;
use couch_rs::database::Database;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tracing::{debug, warn};

/// CouchDB client wrapper
pub struct CouchDb {
    #[allow(dead_code)]
    client: Client,
    db: Database,
    http_client: HttpClient,
    base_db_url: String,
    db_name: String,
    auth: Option<(String, String)>,
    /// Remote path prefix to sync (e.g., "notes/" or "obsidian/")
    remote_path: String,
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

#[derive(Debug, Deserialize)]
struct MetadataChangesResponse {
    results: Vec<MetadataChangeRow>,
    last_seq: Value,
}

#[derive(Debug, Deserialize)]
struct MetadataChangeRow {
    id: String,
    seq: Value,
    deleted: Option<bool>,
    #[serde(default)]
    changes: Vec<ChangeRev>,
}

#[derive(Debug, Deserialize)]
struct ChangeRev {
    rev: String,
}

fn seq_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn is_file_like_doc_id(id: &str) -> bool {
    !id.starts_with("h:") && !id.starts_with("_design/")
}

fn metadata_changes_to_changes(remote_path: &str, rows: Vec<MetadataChangeRow>) -> Vec<Change> {
    let mut latest_by_id: HashMap<String, Change> = HashMap::new();

    for row in rows {
        let _seq = seq_to_string(&row.seq);
        let id = row.id;
        if !(remote_path.is_empty()
            || id.starts_with(remote_path)
            || id == remote_path.trim_end_matches('/'))
            || !is_file_like_doc_id(&id)
        {
            continue;
        }

        let change = if row.deleted.unwrap_or(false) || row.changes.is_empty() {
            Change::remote_deleted(id.clone())
        } else {
            let rev = row
                .changes
                .last()
                .map(|c| c.rev.clone())
                .unwrap_or_default();
            Change::new(
                id.clone(),
                ChangeType::Modified,
                ChangeSource::Remote,
                None,
                None,
                None,
                Some(rev),
            )
        };

        latest_by_id.insert(id, change);
    }

    latest_by_id.into_values().collect()
}

impl CouchDb {
    /// Create a new CouchDB client
    pub async fn new(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        db_name: &str,
        remote_path: &str,
    ) -> Result<Self> {
        let client = match (username, password) {
            (Some(u), Some(p)) => Client::new(url, u, p)?,
            _ => Client::new_no_auth(url)?,
        };

        // Get or create database
        let db = client.db(db_name).await?;

        let auth = match (username, password) {
            (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
            _ => None,
        };

        // Normalize remote path - ensure it ends with / if not empty
        let remote_path = if remote_path.is_empty() || remote_path == "/" {
            String::new()
        } else {
            let mut path = remote_path.to_string();
            if !path.ends_with('/') {
                path.push('/');
            }
            path
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
            http_client: HttpClient::new(),
            base_db_url,
            db_name: db_name.to_string(),
            auth,
            remote_path,
        })
    }

    async fn get_update_seq(&self) -> Result<String> {
        let info = self.client.get_info(&self.db_name).await?;
        Ok(info.update_seq)
    }

    /// Fetch changes from CouchDB using the _changes feed (longpoll)
    pub async fn get_changes_feed(
        &self,
        since: &str,
        timeout_ms: u64,
    ) -> Result<(Vec<ChangeFeedEntry>, String)> {
        let url = format!("{}/_changes", self.base_db_url);

        let mut request = self.http_client.get(&url).query(&[
            ("since", since),
            ("include_docs", "true"),
            ("feed", "longpoll"),
            ("timeout", &timeout_ms.to_string()),
        ]);

        if let Some((username, password)) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }

        let response = request.send().await?.error_for_status()?;
        let body = response.json::<ChangesResponse<FileDoc>>().await?;

        let mut entries = Vec::new();
        for row in body.results {
            if !self.is_path_allowed(&row.id) {
                continue;
            }

            if row.deleted.unwrap_or(false) {
                entries.push(ChangeFeedEntry {
                    change: Change::remote_deleted(row.id),
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
                    change: Change::remote_deleted(doc.id),
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
            path.starts_with(&self.remote_path) || path == self.remote_path.trim_end_matches('/')
        }
    }

    /// Get the normalized remote path prefix used for this sync scope.
    pub fn remote_prefix(&self) -> &str {
        &self.remote_path
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
                .strip_prefix(&self.remote_path)
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

        match self.db.get(path).await {
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
        debug!("Saving file to CouchDB: {}", doc.id);
        let _details = self.db.save(doc).await?;
        Ok(())
    }

    /// Delete a document
    pub async fn delete_file(&self, path: &str) -> Result<()> {
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
        let collection = self.db.get_all::<FileDoc>().await?;
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

        // If no checkpoint exists (first run), return empty changes
        // The files will be handled as new files on the next sync
        if since.is_none() {
            debug!("No checkpoint found, returning empty changes list");
            let seq = self.get_update_seq().await?;
            return Ok((Vec::new(), seq));
        }

        let url = format!("{}/_changes", self.base_db_url);
        let mut request = self
            .http_client
            .get(&url)
            .query(&[("since", since.unwrap())]);

        if let Some((username, password)) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }

        let response = request.send().await?.error_for_status()?;
        let body = response.json::<MetadataChangesResponse>().await?;
        let changes = metadata_changes_to_changes(&self.remote_path, body.results);

        debug!(
            "Remote metadata changes fetched (filtered by remote_path): {}",
            changes.len()
        );

        Ok((changes, seq_to_string(&body.last_seq)))
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
        // Check if path is within allowed remote path
        if !self.is_path_allowed(path) {
            return Ok(None);
        }

        debug!("[FETCH METADATA] Fetching metadata for: {}", path);

        match self.db.get::<FileDoc>(path).await {
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
        match self.db.get_all::<FileDoc>().await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Get attachment content from a document
    /// Attachments are stored at /{db}/{docid}/{attname}
    #[allow(dead_code)]
    pub async fn get_attachment(&self, doc_id: &str, attachment_name: &str) -> Result<Vec<u8>> {
        let url = format!("{}/{}/{}", self.base_db_url, doc_id, attachment_name);

        let mut request = self.http_client.get(&url);

        if let Some((username, password)) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Failed to fetch attachment {}/{}: {}",
                doc_id,
                attachment_name,
                response.status()
            );
        }

        Ok(response.bytes().await?.to_vec())
    }

    /// Get a chunk document by ID
    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkDoc>> {
        let url = format!("{}/{}", self.base_db_url, chunk_id);

        let mut request = self.http_client.get(&url);
        if let Some((username, password)) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }

        let response = request.send().await?;

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

        let mut request = self.http_client.put(&url);
        if let Some((username, password)) = &self.auth {
            request = request.basic_auth(username, Some(password));
        }

        let response = request
            .header("Content-Type", "application/json")
            .json(chunk)
            .send()
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
        let content_str = String::from_utf8_lossy(content);

        // For simplicity, store entire content as a single chunk
        // (Obsidian LiveSync may split into multiple chunks for large files)
        let chunk_id = Self::generate_chunk_id();
        let chunk = ChunkDoc {
            id: chunk_id.clone(),
            rev: None,
            data: content_str.to_string(),
            doc_type: "leaf".to_string(),
        };

        self.save_chunk(&chunk).await?;

        Ok(vec![chunk_id])
    }

    /// Delete old chunks that are no longer referenced
    pub async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()> {
        for chunk_id in chunk_ids {
            if let Some(chunk) = self.get_chunk(chunk_id).await?
                && let Some(rev) = chunk.rev
            {
                let url = format!("{}/{}?rev={}", self.base_db_url, chunk_id, rev);
                let mut request = self.http_client.delete(&url);
                if let Some((username, password)) = &self.auth {
                    request = request.basic_auth(username, Some(password));
                }
                let _ = request.send().await;
                debug!("Deleted old chunk: {}", chunk_id);
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
    use crate::models::ChangeType;
    use serde_json::json;

    #[test]
    fn seq_to_string_from_string() {
        let value = Value::String("123-abc".to_string());
        assert_eq!(seq_to_string(&value), "123-abc");
    }

    #[test]
    fn seq_to_string_from_number() {
        let value = Value::Number(12345.into());
        assert_eq!(seq_to_string(&value), "12345");
    }

    #[test]
    fn seq_to_string_from_array() {
        let value = Value::Array(vec![
            Value::String("a".to_string()),
            Value::Number(1.into()),
        ]);
        assert_eq!(seq_to_string(&value), "[\"a\",1]");
    }

    #[test]
    fn build_couch_url_standard_port() {
        assert_eq!(build_couch_url("localhost", 5984), "http://localhost:5984");
    }

    #[test]
    fn build_couch_url_custom_port() {
        assert_eq!(
            build_couch_url("couch.example.com", 8080),
            "http://couch.example.com:8080"
        );
    }

    #[test]
    fn metadata_changes_only_keep_allowed_file_ids() {
        let changes = metadata_changes_to_changes(
            "Agents/",
            vec![
                MetadataChangeRow {
                    id: "Agents/file.md".into(),
                    seq: json!(1),
                    deleted: None,
                    changes: vec![ChangeRev { rev: "1-a".into() }],
                },
                MetadataChangeRow {
                    id: "Agents/sub/file.md".into(),
                    seq: json!(2),
                    deleted: Some(true),
                    changes: vec![],
                },
                MetadataChangeRow {
                    id: "h:chunk123".into(),
                    seq: json!(3),
                    deleted: None,
                    changes: vec![ChangeRev { rev: "1-b".into() }],
                },
                MetadataChangeRow {
                    id: "Other/file.py".into(),
                    seq: json!(4),
                    deleted: None,
                    changes: vec![ChangeRev { rev: "1-c".into() }],
                },
            ],
        );

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .any(|c| c.path == "Agents/file.md" && c.change_type == ChangeType::Modified)
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path == "Agents/sub/file.md" && c.change_type == ChangeType::Deleted)
        );
    }

    #[test]
    fn metadata_changes_deduplicate_to_latest_seq_per_id() {
        let changes = metadata_changes_to_changes(
            "",
            vec![
                MetadataChangeRow {
                    id: "file.md".into(),
                    seq: json!(1),
                    deleted: None,
                    changes: vec![ChangeRev { rev: "1-a".into() }],
                },
                MetadataChangeRow {
                    id: "file.md".into(),
                    seq: json!(2),
                    deleted: None,
                    changes: vec![ChangeRev { rev: "2-b".into() }],
                },
            ],
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].rev.as_deref(), Some("2-b"));
        assert_eq!(changes[0].change_type, ChangeType::Modified);
        assert!(changes[0].mtime.is_none());
        assert!(changes[0].size.is_none());
    }

    #[test]
    fn metadata_changes_treats_missing_deleted_marker_as_deleted() {
        let changes = metadata_changes_to_changes(
            "",
            vec![MetadataChangeRow {
                id: "Agents/BACKLOG.md".into(),
                seq: json!(1),
                deleted: None,
                changes: vec![],
            }],
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "Agents/BACKLOG.md");
        assert_eq!(changes[0].change_type, ChangeType::Deleted);
    }
}
