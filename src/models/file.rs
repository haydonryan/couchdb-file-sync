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

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Tests for FileDoc
    // =========================================================================

    #[test]
    fn file_doc_new_sets_fields_correctly() {
        let doc = FileDoc::new("test.txt".to_string(), "ignored_hash".to_string(), 100);

        assert_eq!(doc.id, "test.txt");
        assert_eq!(doc.path, "test.txt");
        assert_eq!(doc.size, 100);
        assert_eq!(doc.doc_type, "plain");
        assert!(!doc.deleted);
        assert!(doc.rev.is_none());
        assert!(doc.children.is_empty());
        // ctime and mtime should be set to approximately now
        assert!(doc.ctime > 0);
        assert!(doc.mtime > 0);
    }

    #[test]
    fn file_doc_is_file_with_plain_type() {
        let doc = FileDoc::new("test.txt".to_string(), "hash".to_string(), 100);
        assert!(doc.is_file());
    }

    #[test]
    fn file_doc_is_file_with_chunk_id() {
        let mut doc = FileDoc::new("h:chunk123".to_string(), "hash".to_string(), 50);
        doc.doc_type = "".to_string();
        // IDs starting with "h:" are chunks, not files
        assert!(!doc.is_file());
    }

    #[test]
    fn file_doc_is_file_with_empty_type_and_plain_id() {
        let mut doc = FileDoc::new("test.txt".to_string(), "hash".to_string(), 100);
        doc.doc_type = "".to_string();
        // Empty type with non-chunk ID should be treated as file
        assert!(doc.is_file());
    }

    #[test]
    fn file_doc_modified_at_from_valid_mtime() {
        let now = Utc::now().timestamp_millis() as u64;
        let mut doc = FileDoc::new("test.txt".to_string(), "hash".to_string(), 100);
        doc.mtime = now;

        let modified = doc.modified_at();
        assert_eq!(modified.timestamp_millis() as u64, now);
    }

    #[test]
    fn file_doc_modified_at_with_zero() {
        // Zero is a valid timestamp (Unix epoch)
        let mut doc = FileDoc::new("test.txt".to_string(), "hash".to_string(), 100);
        doc.mtime = 0;

        let modified = doc.modified_at();
        assert_eq!(modified.timestamp(), 0);
    }

    // =========================================================================
    // Tests for FileState
    // =========================================================================

    #[test]
    fn file_state_new_sets_correct_fields() {
        use chrono::Utc;

        let modified = Utc::now();
        let state = FileState::new("test.txt".to_string(), "abc123".to_string(), 100, modified);

        assert_eq!(state.path, "test.txt");
        assert_eq!(state.hash, "abc123");
        assert_eq!(state.size, 100);
        assert_eq!(state.modified_at, modified);
        assert!(state.couch_rev.is_none());
        // last_sync_at should be set to approximately now
        let now = Utc::now();
        let diff = (now - state.last_sync_at).num_milliseconds().abs();
        assert!(diff < 1000); // Within 1 second
    }

    // =========================================================================
    // Tests for RemoteState
    // =========================================================================

    #[test]
    fn remote_state_from_file_doc_maps_fields() {
        use chrono::Utc;

        let now_ms = Utc::now().timestamp_millis() as u64;
        let doc = FileDoc {
            id: "test.txt".to_string(),
            rev: Some("1-abc".to_string()),
            children: vec!["chunk1".to_string()],
            path: "test.txt".to_string(),
            ctime: now_ms,
            mtime: now_ms,
            size: 200,
            doc_type: "plain".to_string(),
            deleted: false,
        };

        let remote: RemoteState = doc.into();

        assert_eq!(remote.path, "test.txt");
        assert_eq!(remote.size, 200);
        assert_eq!(remote.couch_rev, "1-abc");
        assert!(!remote.deleted);
        // Hash is always empty string (computed locally, not stored in CouchDB)
        assert_eq!(remote.hash, "");
        // mtime should be converted correctly
        assert_eq!(remote.modified_at.timestamp_millis() as u64, now_ms);
    }

    #[test]
    fn remote_state_from_file_doc_handles_deleted() {
        let mut doc = FileDoc::new("deleted.txt".to_string(), "hash".to_string(), 0);
        doc.deleted = true;
        doc.rev = Some("2-deleted".to_string());

        let remote: RemoteState = doc.into();

        assert!(remote.deleted);
        assert_eq!(remote.couch_rev, "2-deleted");
    }
}
