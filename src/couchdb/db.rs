use crate::models::{Change, ChunkDoc, FileDoc, RemoteState};
use anyhow::Result;
use couch_rs::database::Database;
use couch_rs::Client;
use reqwest::Client as HttpClient;
use tracing::{debug, warn};

/// CouchDB client wrapper
pub struct CouchDb {
    #[allow(dead_code)]
    client: Client,
    db: Database,
    db_name: String,
    http_client: HttpClient,
    base_url: String,
    auth: Option<(String, String)>,
}

impl CouchDb {
    /// Create a new CouchDB client
    pub async fn new(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        db_name: &str,
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

        Ok(Self {
            client,
            db,
            db_name: db_name.to_string(),
            http_client: HttpClient::new(),
            base_url: url.to_string(),
            auth,
        })
    }

    /// Get a document by ID
    pub async fn get_file(&self, path: &str) -> Result<Option<FileDoc>> {
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

    /// Get all documents (non-deleted, files only - not chunks)
    pub async fn get_all_files(&self) -> Result<Vec<FileDoc>> {
        let collection = self.db.get_all::<FileDoc>().await?;
        Ok(collection
            .rows
            .into_iter()
            .filter(|d| !d.deleted && d.is_file())
            .collect())
    }

    /// Get changes by comparing local state with remote
    /// Returns all remote files with their mtime for comparison
    pub async fn get_changes(&self, _since: Option<&str>) -> Result<(Vec<Change>, String)> {
        let files = self.get_all_files().await?;
        let changes: Vec<Change> = files
            .into_iter()
            .map(|doc| {
                let mtime = doc.modified_at();
                crate::models::Change::remote_modified(doc.id, String::new(), doc.size, mtime)
            })
            .collect();

        // Return a simple sequence number (timestamp)
        let seq = chrono::Utc::now().timestamp().to_string();
        Ok((changes, seq))
    }

    /// Get remote state for comparison
    pub async fn get_remote_state(&self, path: &str) -> Result<Option<RemoteState>> {
        match self.get_file(path).await? {
            Some(doc) => Ok(Some(RemoteState::from(doc))),
            None => Ok(None),
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
        // Note: base_url may already include the database path
        let url = format!("{}/{}/{}", self.base_url, doc_id, attachment_name);

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
        // Note: base_url may already include the database path (e.g., /obsidian)
        // so we try without db_name first
        let url = format!("{}/{}", self.base_url, chunk_id);

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
}

/// Helper to create CouchDB URL from components
pub fn build_couch_url(host: &str, port: u16) -> String {
    format!("http://{}:{}", host, port)
}
