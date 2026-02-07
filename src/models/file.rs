use chrono::{DateTime, Utc};
use couch_rs::document::TypedCouchDocument;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// File metadata stored in CouchDB (matches Obsidian LiveSync format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDoc {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Chunk IDs that make up the file content
    #[serde(default)]
    pub children: Vec<String>,
    /// File path (same as id for files)
    #[serde(default)]
    pub path: String,
    /// Creation time in milliseconds
    #[serde(default)]
    pub ctime: u64,
    /// Modification time in milliseconds
    #[serde(default)]
    pub mtime: u64,
    /// File size in bytes
    #[serde(default)]
    pub size: u64,
    /// Document type: "plain" for files, "leaf" for chunks
    #[serde(rename = "type", default)]
    pub doc_type: String,
    /// Whether the file is deleted
    #[serde(default)]
    pub deleted: bool,
}

impl FileDoc {
    pub fn new(path: String, _hash: String, size: u64) -> Self {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        Self {
            id: path.clone(),
            rev: None,
            children: Vec::new(),
            path,
            ctime: now,
            mtime: now,
            size,
            doc_type: "plain".to_string(),
            deleted: false,
        }
    }

    /// Check if this is a file document (not a chunk)
    pub fn is_file(&self) -> bool {
        // Files have type "plain" and IDs that don't start with "h:"
        self.doc_type == "plain" || (!self.id.starts_with("h:") && self.doc_type.is_empty())
    }

    /// Get modification time as DateTime
    pub fn modified_at(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.mtime as i64).unwrap_or_else(Utc::now)
    }
}

/// Chunk document containing actual file content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDoc {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The actual content data
    #[serde(default)]
    pub data: String,
    /// Document type: "leaf" for chunks
    #[serde(rename = "type", default)]
    pub doc_type: String,
}

impl TypedCouchDocument for FileDoc {
    fn get_id(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.id)
    }

    fn get_rev(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.rev.as_deref().unwrap_or(""))
    }

    fn set_rev(&mut self, rev: &str) {
        self.rev = Some(rev.to_string());
    }

    fn set_id(&mut self, id: &str) {
        self.id = id.to_string();
    }

    fn merge_ids(&mut self, other: &Self) {
        self.id = other.id.clone();
        self.rev = other.rev.clone();
    }
}

/// Local file state for tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub couch_rev: Option<String>,
    pub last_sync_at: DateTime<Utc>,
}

impl FileState {
    pub fn new(path: String, hash: String, size: u64, modified_at: DateTime<Utc>) -> Self {
        Self {
            path,
            hash,
            size,
            couch_rev: None,
            last_sync_at: Utc::now(),
            modified_at,
        }
    }
}

/// Remote file state from CouchDB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteState {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub couch_rev: String,
    #[serde(default)]
    pub deleted: bool,
}

impl From<FileDoc> for RemoteState {
    fn from(doc: FileDoc) -> Self {
        let modified_at = doc.modified_at();
        Self {
            path: doc.id,
            hash: String::new(), // Hash not stored in CouchDB, computed locally
            size: doc.size,
            modified_at,
            couch_rev: doc.rev.unwrap_or_default(),
            deleted: doc.deleted,
        }
    }
}
