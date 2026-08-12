use chrono::{DateTime, Utc};
use couch_rs::document::TypedCouchDocument;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Document type enum replacing arbitrary String
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum DocType {
    #[default]
    Plain,
    Leaf,
}

/// Non-empty `CouchDB` revision newtype
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CouchRev(String);

impl CouchRev {
    /// The default initial revision for newly created `CouchDB` documents.
    ///
    /// This literal is non-empty, so it is always a valid `CouchRev`.
    /// Construct it via [`CouchRev::default`] so call sites never need the
    /// panic-prone `CouchRev::new(Self::DEFAULT_REV).unwrap()` pattern.
    pub const DEFAULT_REV: &str = "1-";

    /// Create a new `CouchRev`. Returns None if the revision string is empty.
    #[must_use]
    pub fn new(rev: &str) -> Option<Self> {
        if rev.is_empty() {
            None
        } else {
            Some(Self(rev.to_string()))
        }
    }

    /// Return the revision string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for CouchRev {
    /// The single validated default revision, constructed directly from the
    /// known-valid `DEFAULT_REV` literal so this cannot panic.
    fn default() -> Self {
        Self(Self::DEFAULT_REV.to_string())
    }
}

impl std::ops::Deref for CouchRev {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CouchRev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Allow CouchRev to be stored in SQLite
impl rusqlite::types::ToSql for CouchRev {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0.as_str()))
    }
}

impl rusqlite::types::FromSql for CouchRev {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        value.as_str().and_then(|s| {
            Self::new(s)
                .ok_or_else(|| rusqlite::types::FromSqlError::Other(Box::new(std::fmt::Error)))
        })
    }
}

/// Timestamp in milliseconds since epoch
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TimestampMillis(u64);

impl TimestampMillis {
    #[must_use]
    pub const fn new(ms: u64) -> Self {
        Self(ms)
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn now() -> Self {
        Self(u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0))
    }

    pub fn to_datetime(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(i64::try_from(self.0).unwrap_or(i64::MAX))
            .unwrap_or_else(Utc::now)
    }
}

impl fmt::Display for TimestampMillis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// File metadata stored in `CouchDB` (matches Obsidian `LiveSync` format)
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
    pub ctime: TimestampMillis,
    /// Modification time in milliseconds
    #[serde(default)]
    pub mtime: TimestampMillis,
    /// Authoritative soft-delete time in milliseconds. Live files keep this at
    /// `0`. It is stamped when a file is soft-deleted so downstream clients
    /// arbitrate the deletion against the *deletion* time rather than the
    /// deleted file's possibly-stale preserved mtime. Older/external
    /// tombstones leave it `0`.
    #[serde(default)]
    pub deleted_at: TimestampMillis,
    /// File size in bytes
    #[serde(default)]
    pub size: u64,
    /// Document type: "plain" for files, "leaf" for chunks
    #[serde(rename = "type", default)]
    pub doc_type: DocType,
    /// Whether the file is deleted
    #[serde(default)]
    pub deleted: bool,
}

impl FileDoc {
    #[must_use]
    pub fn new(path: String, _hash: String, size: u64) -> Self {
        let now = TimestampMillis::now();
        Self {
            id: path.clone(),
            rev: None,
            children: Vec::new(),
            path,
            ctime: now,
            mtime: now,
            deleted_at: TimestampMillis::default(),
            size,
            doc_type: DocType::Plain,
            deleted: false,
        }
    }

    /// Check if this is a file document (not a chunk).
    ///
    /// Classification is type-based rather than an id-prefix heuristic: a
    /// document is a file iff its `type` is `plain`, and a chunk iff its `type`
    /// is `leaf`. The previous `h:`-prefix check could misclassify leaf docs
    /// whose ids do not carry the prefix.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        matches!(self.doc_type, DocType::Plain)
    }

    /// Get modification time as `DateTime`
    #[must_use]
    pub fn modified_at(&self) -> DateTime<Utc> {
        self.mtime.to_datetime()
    }

    /// The authoritative time a soft-delete tombstone was created.
    ///
    /// Tombstones written by this codebase stamp `deleted_at` at delete time;
    /// older or external tombstones may only carry the deleted file's preserved
    /// mtime, so this falls back to that when no `deleted_at` is recorded.
    #[must_use]
    pub fn delete_time(&self) -> DateTime<Utc> {
        if self.deleted_at.as_u64() != 0 {
            self.deleted_at.to_datetime()
        } else {
            self.mtime.to_datetime()
        }
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
    pub doc_type: DocType,
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
        self.id.clone_from(&other.id);
        self.rev.clone_from(&other.rev);
    }
}

/// Local file state for tracking
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub couch_rev: Option<CouchRev>,
    pub last_sync_at: DateTime<Utc>,
}

impl FileState {
    #[must_use]
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

/// Remote file state from `CouchDB`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteState {
    pub hash: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub couch_rev: CouchRev,
    #[serde(default)]
    pub deleted: bool,
}

impl From<FileDoc> for RemoteState {
    fn from(doc: FileDoc) -> Self {
        let modified_at = doc.modified_at();
        let couch_rev = doc
            .rev
            .as_deref()
            .and_then(CouchRev::new)
            .unwrap_or_default();
        Self {
            hash: String::new(), // Hash not stored in CouchDB, computed locally
            size: doc.size,
            modified_at,
            couch_rev,
            deleted: doc.deleted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_couch_rev_default() {
        assert_eq!(CouchRev::DEFAULT_REV, "1-");
        assert_eq!(CouchRev::default().as_str(), "1-");
    }

    #[test]
    fn test_couch_rev_new_empty() {
        assert!(CouchRev::new("").is_none());
        assert!(CouchRev::new("1-abc").is_some());
    }

    #[test]
    fn test_file_doc_new() {
        let doc = FileDoc::new("/path/to/file.txt".into(), "hash1".into(), 1024);
        assert_eq!(doc.id, "/path/to/file.txt");
        assert_eq!(doc.path, "/path/to/file.txt");
        assert_eq!(doc.size, 1024);
        assert_eq!(doc.doc_type, DocType::Plain);
        assert!(!doc.deleted);
        assert!(doc.rev.is_none());
        assert!(doc.children.is_empty());
        // ctime and mtime should be set to approximately now
        let now_ms = u64::try_from(Utc::now().timestamp_millis()).unwrap_or(0);
        assert!(doc.ctime.as_u64() > 0 && (now_ms - doc.ctime.as_u64()) < 5000);
        assert!(doc.mtime.as_u64() > 0 && (now_ms - doc.mtime.as_u64()) < 5000);
    }

    #[test]
    fn test_file_doc_is_file_plain_type() {
        let doc = FileDoc {
            id: "/path/to/file.txt".into(),
            rev: None,
            children: vec![],
            path: "/path/to/file.txt".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Plain,
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
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Plain,
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
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Leaf,
            deleted: false,
        };
        assert!(!doc.is_file());
    }

    #[test]
    fn test_file_doc_is_file_leaf_type_ignores_id_prefix() {
        // A leaf chunk whose id does not carry the old "h:" prefix must still
        // be classified as a chunk. The type-based check no longer depends on
        // the id-prefix heuristic, which used to misclassify such docs.
        let doc = FileDoc {
            id: "chunk/without/prefix".into(),
            rev: None,
            children: vec![],
            path: "chunk/without/prefix".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Leaf,
            deleted: false,
        };
        assert!(!doc.is_file());
    }

    #[test]
    fn test_file_doc_is_file_plain_type_ignores_h_prefix() {
        // A plain file whose id happens to start with "h:" is still a file.
        let doc = FileDoc {
            id: "h:not-a-chunk.txt".into(),
            rev: None,
            children: vec![],
            path: "h:not-a-chunk.txt".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Plain,
            deleted: false,
        };
        assert!(doc.is_file());
    }

    #[test]
    fn test_file_doc_modified_at() {
        let mtime_ms: u64 = 1_722_153_600_000; // 2024-07-28
        let doc = FileDoc {
            id: "/path/to/file.txt".into(),
            rev: None,
            children: vec![],
            path: "/path/to/file.txt".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(mtime_ms),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Plain,
            deleted: false,
        };
        let modified = doc.modified_at();
        assert_eq!(
            u64::try_from(modified.timestamp_millis()).unwrap_or(0),
            mtime_ms
        );
    }

    #[test]
    fn test_file_doc_delete_time_uses_deleted_at_over_preserved_mtime() {
        // A tombstone stamps deleted_at at delete time even when the deleted
        // file's own mtime is old/preserved; delete_time must prefer it.
        let mut doc = FileDoc {
            id: "/path/to/file.txt".into(),
            rev: None,
            children: vec![],
            path: "/path/to/file.txt".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(1000),
            deleted_at: TimestampMillis::new(9000),
            size: 100,
            doc_type: DocType::Plain,
            deleted: true,
        };
        assert_eq!(
            doc.delete_time().timestamp_millis(),
            9000,
            "delete_time must prefer the authoritative deleted_at"
        );

        // Legacy/external tombstones without deleted_at fall back to mtime.
        doc.deleted_at = TimestampMillis::default();
        assert_eq!(
            doc.delete_time().timestamp_millis(),
            1000,
            "delete_time must fall back to mtime when deleted_at is unset"
        );
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
        let mtime_ms = 1_722_153_600_000u64;
        let doc = FileDoc {
            id: "/remote/path.txt".into(),
            rev: Some("1-abc123".into()),
            children: vec![],
            path: "/remote/path.txt".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(mtime_ms),
            deleted_at: TimestampMillis::default(),
            size: 512,
            doc_type: DocType::Plain,
            deleted: false,
        };
        let remote: RemoteState = doc.into();
        assert_eq!(remote.hash, "");
        assert_eq!(remote.size, 512);
        assert_eq!(remote.couch_rev.as_str(), "1-abc123");
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
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 0,
            doc_type: DocType::Plain,
            deleted: true,
        };
        let remote: RemoteState = doc.into();

        assert!(remote.deleted);
        assert_eq!(remote.couch_rev.as_str(), "2-def456");
    }

    #[test]
    fn test_file_doc_typdoc_couch_trait() {
        let mut doc = FileDoc {
            id: "doc1".into(),
            rev: Some("3-ghi789".into()),
            children: vec![],
            path: "doc1".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Plain,
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
            doc_type: DocType::Leaf,
        };
        assert_eq!(chunk.id, "h:chunk1");
        assert_eq!(chunk.data, "file content here");
        assert_eq!(chunk.doc_type, DocType::Leaf);
    }
    #[test]
    fn test_file_doc_merge_ids() {
        let mut doc = FileDoc {
            id: "doc1".into(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: "doc1".into(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis(0),
            deleted_at: TimestampMillis::default(),
            size: 100,
            doc_type: DocType::Plain,
            deleted: false,
        };
        let other = FileDoc {
            id: "doc2".into(),
            rev: Some("2-def".to_string()),
            children: vec!["h:chunk1".into()],
            path: "doc2".into(),
            ctime: TimestampMillis(1000),
            mtime: TimestampMillis(2000),
            deleted_at: TimestampMillis::default(),
            size: 200,
            doc_type: DocType::Plain,
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
