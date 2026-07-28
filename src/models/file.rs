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
    use chrono::Utc;

    #[test]
    fn test_file_doc_new() {
        let doc = FileDoc::new("/path/to/file.txt".into(), "hash1".into(), 1024);
        assert_eq!(doc.id, "/path/to/file.txt");
        assert_eq!(doc.path, "/path/to/file.txt");
        assert_eq!(doc.size, 1024);
        assert_eq!(doc.doc_type, "plain");
        assert!(!doc.deleted);
        assert!(doc.rev.is_none());
        assert!(doc.children.is_empty());
        // ctime and mtime should be set to approximately now
        let now_ms = Utc::now().timestamp_millis() as u64;
        assert!(doc.ctime > 0 && (now_ms - doc.ctime) < 5000);
        assert!(doc.mtime > 0 && (now_ms - doc.mtime) < 5000);
    }

    #[test]
    fn test_file_doc_is_file_plain_type() {
        let doc = FileDoc {
            id: "/path/to/file.txt".into(),
            rev: None,
            children: vec![],
            path: "/path/to/file.txt".into(),
            ctime: 0,
            mtime: 0,
            size: 100,
            doc_type: "plain".into(),
            deleted: false,
        };
        assert!(doc.is_file());
    }

    #[test]
    fn test_file_doc_is_file_empty_type_not_chunk() {
        let doc = FileDoc {
            id: "/path/to/file.txt".into(),
            rev: None,
            children: vec![],
            path: "/path/to/file.txt".into(),
            ctime: 0,
            mtime: 0,
            size: 100,
            doc_type: "".into(),
            deleted: false,
        };
        assert!(doc.is_file());
    }

    #[test]
    fn test_file_doc_is_file_chunk_prefix() {
        let doc = FileDoc {
            id: "h:abc123".into(),
            rev: None,
            children: vec![],
            path: "h:abc123".into(),
            ctime: 0,
            mtime: 0,
            size: 100,
            doc_type: "leaf".into(),
            deleted: false,
        };
        assert!(!doc.is_file());
    }

    #[test]
    fn test_file_doc_modified_at() {
        let mtime_ms = 1722153600000u64; // 2024-07-28
        let doc = FileDoc {
            id: "/path/to/file.txt".into(),
            rev: None,
            children: vec![],
            path: "/path/to/file.txt".into(),
            ctime: 0,
            mtime: mtime_ms,
            size: 100,
            doc_type: "plain".into(),
            deleted: false,
        };
        let modified = doc.modified_at();
        assert_eq!(modified.timestamp_millis() as u64, mtime_ms);
    }

    #[test]
    fn test_file_state_new() {
        let now = Utc::now();
        let state = FileState::new("/path/to/file.txt".into(), "hash1".into(), 2048, now);
        assert_eq!(state.path, "/path/to/file.txt");
        assert_eq!(state.hash, "hash1");
        assert_eq!(state.size, 2048);
        assert_eq!(state.modified_at, now);
        assert!(state.couch_rev.is_none());
        // last_sync_at should be approximately now
        let diff = Utc::now() - state.last_sync_at;
        assert!(diff.num_seconds() < 5, "last_sync_at should be recent");
    }

    #[test]
    fn test_remote_state_from_file_doc() {
        let mtime_ms = 1722153600000u64;
        let doc = FileDoc {
            id: "/remote/path.txt".into(),
            rev: Some("1-abc123".into()),
            children: vec![],
            path: "/remote/path.txt".into(),
            ctime: 0,
            mtime: mtime_ms,
            size: 512,
            doc_type: "plain".into(),
            deleted: false,
        };
        let remote: RemoteState = doc.into();
        assert_eq!(remote.path, "/remote/path.txt");
        assert_eq!(remote.size, 512);
        assert_eq!(remote.couch_rev, "1-abc123");
        assert!(!remote.deleted);
        assert_eq!(remote.hash, ""); // Hash not stored in CouchDB
    }

    #[test]
    fn test_remote_state_from_file_doc_deleted() {
        let doc = FileDoc {
            id: "/remote/deleted.txt".into(),
            rev: Some("2-def456".into()),
            children: vec![],
            path: "/remote/deleted.txt".into(),
            ctime: 0,
            mtime: 0,
            size: 0,
            doc_type: "plain".into(),
            deleted: true,
        };
        let remote: RemoteState = doc.into();
        assert_eq!(remote.path, "/remote/deleted.txt");
        assert!(remote.deleted);
        assert_eq!(remote.couch_rev, "2-def456");
    }

    #[test]
    fn test_file_doc_typdoc_couch_trait() {
        let mut doc = FileDoc {
            id: "doc1".into(),
            rev: Some("3-ghi789".into()),
            children: vec![],
            path: "doc1".into(),
            ctime: 0,
            mtime: 0,
            size: 100,
            doc_type: "plain".into(),
            deleted: false,
        };
        // Test TypedCouchDocument trait methods
        assert_eq!(doc.get_id(), "doc1");
        assert_eq!(doc.get_rev(), "3-ghi789");
        doc.set_rev("4-jkl012");
        assert_eq!(doc.rev, Some("4-jkl012".into()));
        doc.set_id("new-id");
        assert_eq!(doc.id, "new-id");
    }

    #[test]
    fn test_chunk_doc_struct() {
        let chunk = ChunkDoc {
            id: "h:chunk1".into(),
            rev: Some("1-rev".into()),
            data: "file content here".into(),
            doc_type: "leaf".into(),
        };
        assert_eq!(chunk.id, "h:chunk1");
        assert_eq!(chunk.data, "file content here");
        assert_eq!(chunk.doc_type, "leaf");
    }
    #[test]
    fn test_file_doc_merge_ids() {
        let mut doc = FileDoc {
            id: "doc1".into(),
            rev: Some("1-abc".into()),
            children: vec![],
            path: "doc1".into(),
            ctime: 0,
            mtime: 0,
            size: 100,
            doc_type: "plain".into(),
            deleted: false,
        };
        let other = FileDoc {
            id: "doc2".into(),
            rev: Some("2-def".into()),
            children: vec!["h:chunk1".into()],
            path: "doc2".into(),
            ctime: 1000,
            mtime: 2000,
            size: 200,
            doc_type: "plain".into(),
            deleted: false,
        };
        doc.merge_ids(&other);
        assert_eq!(doc.id, "doc2");
        assert_eq!(doc.rev, Some("2-def".into()));
        // Other fields should remain from doc
        assert_eq!(doc.path, "doc1");
        assert_eq!(doc.size, 100);
    }
}
