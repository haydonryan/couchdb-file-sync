use crate::models::{Change, FileDoc, RemoteState};
use anyhow::Result;
use couch_rs::database::Database;
use couch_rs::Client;
use tracing::debug;

/// CouchDB client wrapper
pub struct CouchDb {
    #[allow(dead_code)]
    client: Client,
    db: Database,
    #[allow(dead_code)]
    db_name: String,
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

        Ok(Self {
            client,
            db,
            db_name: db_name.to_string(),
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

    /// Get all documents (non-deleted)
    pub async fn get_all_files(&self) -> Result<Vec<FileDoc>> {
        let collection = self.db.get_all::<FileDoc>().await?;
        Ok(collection.rows.into_iter().filter(|d| !d.deleted).collect())
    }

    /// Get changes by comparing local state with remote
    /// For simplicity, this just returns all remote files as "modified"
    /// In production, you'd use the _changes feed with proper sequence tracking
    pub async fn get_changes(&self, _since: Option<&str>) -> Result<(Vec<Change>, String)> {
        let files = self.get_all_files().await?;
        let changes: Vec<Change> = files
            .into_iter()
            .map(|doc| crate::models::Change::remote_modified(doc.id, doc.hash, doc.size))
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
}

/// Helper to create CouchDB URL from components
pub fn build_couch_url(host: &str, port: u16) -> String {
    format!("http://{}:{}", host, port)
}
