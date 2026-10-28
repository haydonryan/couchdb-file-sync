use anyhow::{Result, anyhow, bail};
use base64::Engine;
use chrono::{DateTime, Utc};
use couch_rs::document::TypedCouchDocument;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Official Obsidian `LiveSync` marks LZ-compressed chunk data with this leading
/// marker (`\u000eLZ\u001d`). This tool does not implement the decompressor, so
/// any chunk carrying it is an unsupported binary format and must fail rather
/// than be silently reinterpreted as text or base64.
const LIVESYNC_COMPRESSED_MARKER: &[u8] = &[0x0E, 0x4C, 0x5A, 0x1D];

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
    /// Content hash (SHA-256 hex) of the file bytes, persisted on upload so
    /// downstream downloads can verify integrity against the remote. Older or
    /// externally-created docs may leave this empty.
    #[serde(default)]
    pub hash: String,
}

impl FileDoc {
    #[must_use]
    pub fn new(path: String, hash: String, size: u64) -> Self {
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
            hash,
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

/// Content codec for a chunk document's `data` string.
///
/// Every chunk written by this client carries an explicit codec so its
/// `data` field is never ambiguous. Records lacking a codec are legacy
/// (either old v0.5 base64 chunks or official `LiveSync` plain-text chunks);
/// their interpretation is governed by [`LegacyChunkData`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChunkCodec {
    /// Native base64-encoded arbitrary bytes (this project's encoded form).
    #[default]
    Base64,
    /// Official `LiveSync` plain-text chunk: `data` is a raw UTF-8 string.
    Plain,
}

/// Policy for interpreting a legacy chunk `data` string that carries no
/// explicit `codec`.
///
/// A legacy record is inherently ambiguous: it could be a v0.5 base64-encoded
/// chunk written by this tool, or a raw plain-text chunk written by official
/// `LiveSync`. The two cannot be told apart, so the operator picks the policy
/// that matches the database they are syncing. New chunks written by this
/// client always carry a codec and are unaffected by this policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LegacyChunkData {
    /// Migration mode: an untagged `data` string is base64-encoded bytes
    /// (v0.5 records written by earlier versions of this tool).
    #[default]
    Base64,
    /// `LiveSync` plain-text import mode: an untagged `data` string is raw
    /// text and is NOT base64-reinterpreted.
    PlainText,
}

/// Chunk document containing actual file content
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkDoc {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The content as it appears on the wire: a JSON string. For a native
    /// record (`codec: base64`) this is base64-encoded bytes; for a `LiveSync`
    /// plain chunk it is raw text. Use [`ChunkDoc::content_bytes`] with a
    /// legacy policy to decode it.
    #[serde(default)]
    pub data: String,
    /// Document type: "leaf" for chunks
    #[serde(rename = "type", default)]
    pub doc_type: DocType,
    /// Creation time in milliseconds since epoch, stamped when a chunk is
    /// uploaded. Lets the orphan collector distinguish a long-orphaned chunk
    /// from one that is merely mid-publication (a chunk written by this or a
    /// concurrent client whose metadata document has not landed yet), so
    /// in-flight content is never collected. Older or externally-created
    /// chunks leave this `0`.
    #[serde(default)]
    pub created_at: TimestampMillis,
    /// Explicit content codec for `data`. Absent on legacy (v0.5 / `LiveSync`)
    /// records; [`ChunkDoc::content_bytes`] then applies the configured
    /// [`LegacyChunkData`] policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<ChunkCodec>,
}

impl ChunkDoc {
    /// Build a native chunk whose content is base64-encoded on the wire with
    /// an explicit `base64` codec, so it is never mistaken for raw text on
    /// read-back.
    #[must_use]
    pub fn new_encoded(id: String, content: Vec<u8>) -> Self {
        Self {
            id,
            rev: None,
            data: base64::engine::general_purpose::STANDARD.encode(content),
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::now(),
            codec: Some(ChunkCodec::Base64),
        }
    }

    /// Decode the chunk content to raw bytes.
    ///
    /// An explicit `codec` is authoritative. When it is absent (a
    /// legacy/ambiguous record), the supplied `legacy` policy decides whether
    /// the string is base64 (v0.5 migration) or raw plain text (`LiveSync`
    /// import). Invalid base64, an unsupported codec, or official `LiveSync`
    /// compressed data yields an error rather than silently corrupting the
    /// bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when a base64 record contains invalid base64, or when
    /// the data carries the official `LiveSync` LZ-compressed marker (an
    /// unsupported binary format).
    pub fn content_bytes(&self, legacy: LegacyChunkData) -> Result<Vec<u8>> {
        // Official LiveSync marks LZ-compressed chunks with a leading marker
        // (`\u000eLZ\u001d`). Treating those bytes as text or base64 would
        // silently corrupt the file, so fail per-chunk instead.
        if self.data.as_bytes().starts_with(LIVESYNC_COMPRESSED_MARKER) {
            bail!(
                "chunk {} uses unsupported LiveSync compressed format; refusing to decode",
                self.id
            );
        }
        let use_base64 = match self.codec {
            Some(ChunkCodec::Plain) => false,
            Some(ChunkCodec::Base64) => true,
            None => matches!(legacy, LegacyChunkData::Base64),
        };
        if use_base64 {
            base64::engine::general_purpose::STANDARD
                .decode(&self.data)
                .map_err(|e| {
                    anyhow!(
                        "chunk {} has invalid base64 data (unsupported or corrupt format): {e}",
                        self.id
                    )
                })
        } else {
            Ok(self.data.as_bytes().to_vec())
        }
    }
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

impl TypedCouchDocument for ChunkDoc {
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

/// A kernel-managed, per-inode change fingerprint.
///
/// On Unix this is the `st_ctime` change time: it advances whenever the
/// inode changes, including any content write, and userspace tools cannot set
/// it (only the kernel bumps it). It is therefore reliable change evidence
/// that a stored hash is still valid even when a tool (`cp -p`, `rsync -t`,
/// `git checkout`, `touch -t`) preserved the file's mtime and size. Platforms
/// without an equivalent (e.g. Windows) yield `None` and fall back to the
/// conservative last-sync guard in the scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeFingerprint {
    /// Whole seconds since the Unix epoch of the change time.
    pub secs: i64,
    /// Sub-second nanosecond component of the change time.
    pub nanos: i64,
}

impl ChangeFingerprint {
    /// Read the change fingerprint from a file's metadata, or `None` on
    /// platforms without reliable change evidence (no `ctime` equivalent).
    #[cfg(unix)]
    #[must_use]
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            secs: metadata.ctime(),
            nanos: metadata.ctime_nsec(),
        })
    }

    /// Read the change fingerprint from a file's metadata. No reliable change
    /// evidence exists on this platform, so this always yields `None`.
    #[cfg(not(unix))]
    #[must_use]
    pub fn from_metadata(_metadata: &std::fs::Metadata) -> Option<Self> {
        None
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
    /// Reliable platform change evidence captured at scan time (the on-disk
    /// `ctime` on Unix). `None` on platforms without such evidence. Used by
    /// the scanner to decide whether the stored hash can be reused without a
    /// content re-read.
    #[serde(default)]
    pub change_fingerprint: Option<ChangeFingerprint>,
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
            change_fingerprint: None,
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
            hash: doc.hash,
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
        assert_eq!(doc.children, [] as [String; 0]);
        assert_eq!(doc.hash, "hash1");
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
            hash: String::new(),
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
            hash: String::new(),
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
            hash: String::new(),
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
            hash: String::new(),
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
            hash: String::new(),
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
            hash: String::new(),
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
            hash: "abc123".into(),
        };
        let remote: RemoteState = doc.into();
        assert_eq!(remote.hash, "abc123");
        assert_eq!(remote.size, 512);
        assert_eq!(remote.couch_rev.as_str(), "1-abc123");
        assert!(!remote.deleted);
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
            hash: String::new(),
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
            hash: String::new(),
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
            data: "ZmlsZSBjb250ZW50IGhlcmU=".into(), // base64("file content here")
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::new(1234),
            codec: Some(ChunkCodec::Base64),
        };
        assert_eq!(chunk.id, "h:chunk1");
        assert_eq!(chunk.data, "ZmlsZSBjb250ZW50IGhlcmU=");
        assert_eq!(chunk.doc_type, DocType::Leaf);
        assert_eq!(
            chunk.content_bytes(LegacyChunkData::Base64).unwrap(),
            b"file content here"
        );
    }

    #[test]
    fn test_new_encoded_writes_explicit_base64_codec() {
        let chunk = ChunkDoc::new_encoded("h:c".into(), b"arbitrary \xFF\x00 bytes".to_vec());
        assert_eq!(chunk.codec, Some(ChunkCodec::Base64));
        // The wire form is base64 and decodes back byte-identically regardless
        // of the legacy policy, because the explicit codec is authoritative.
        assert_eq!(
            chunk.content_bytes(LegacyChunkData::PlainText).unwrap(),
            b"arbitrary \xFF\x00 bytes"
        );
        assert_eq!(
            chunk.content_bytes(LegacyChunkData::Base64).unwrap(),
            b"arbitrary \xFF\x00 bytes"
        );
    }

    #[test]
    fn test_explicit_plain_codec_is_never_base64_reinterpreted() {
        // "YWJj" is raw text (4 bytes) but is also valid base64 for "abc".
        // With an explicit Plain codec it must decode to the raw text bytes.
        let chunk = ChunkDoc {
            id: "h:p".into(),
            rev: None,
            data: "YWJj".into(),
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::new(0),
            codec: Some(ChunkCodec::Plain),
        };
        assert_eq!(
            chunk.content_bytes(LegacyChunkData::Base64).unwrap(),
            b"YWJj",
            "explicit Plain codec must win over the base64 legacy policy"
        );
    }

    #[test]
    fn test_legacy_untagged_data_respects_migration_mode() {
        // v0.5 untagged base64 record: decodes as base64 in migration mode,
        // and (documented tradeoff) as raw text in plain-text import mode.
        let chunk = ChunkDoc {
            id: "h:v05".into(),
            rev: None,
            data: "YWJj".into(), // base64("abc")
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::new(0),
            codec: None,
        };
        assert_eq!(
            chunk.content_bytes(LegacyChunkData::Base64).unwrap(),
            b"abc",
            "v0.5 migration mode decodes untagged data as base64"
        );
        assert_eq!(
            chunk.content_bytes(LegacyChunkData::PlainText).unwrap(),
            b"YWJj",
            "plain-text import mode reads untagged data as raw text"
        );
    }

    #[test]
    fn test_invalid_base64_untagged_fails_instead_of_corrupting() {
        let chunk = ChunkDoc {
            id: "h:bad".into(),
            rev: None,
            data: "!!!not base64!!!".into(),
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::new(0),
            codec: None,
        };
        let err = chunk
            .content_bytes(LegacyChunkData::Base64)
            .expect_err("invalid base64 must error, not corrupt");
        assert!(err.to_string().contains("invalid base64"), "{err}");
    }

    #[test]
    fn test_unsupported_codec_fails_deserialization() {
        // An unknown codec value (e.g. a future binary/encrypted format) must
        // fail per-record rather than being silently reinterpreted.
        let raw = r#"{"_id":"h:x","type":"leaf","data":"abc","codec":"encrypted"}"#;
        let err = serde_json::from_str::<ChunkDoc>(raw)
            .expect_err("unknown codec must fail deserialization");
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }

    #[test]
    fn test_unsupported_metadata_type_fails_deserialization() {
        // A chunk whose `type` (metadata) we do not support must fail rather
        // than be treated as plain/leaf content.
        let raw = r#"{"_id":"h:x","type":"encrypted","data":"abc"}"#;
        let err = serde_json::from_str::<ChunkDoc>(raw)
            .expect_err("unknown doc_type must fail deserialization");
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }

    #[test]
    fn test_livesync_compressed_chunk_fails_in_both_modes() {
        // Official LiveSync LZ-compressed chunks carry a marker prefix. They
        // are unsupported and must fail in either legacy mode rather than
        // being silently read as text/base64.
        let chunk = ChunkDoc {
            id: "h:z".into(),
            rev: None,
            data: "\u{000E}LZ\u{001D}<lz-bytes>".into(),
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::new(0),
            codec: None,
        };
        for legacy in [LegacyChunkData::Base64, LegacyChunkData::PlainText] {
            let err = chunk
                .content_bytes(legacy)
                .expect_err("compressed chunk must be rejected");
            assert!(
                err.to_string().contains("unsupported LiveSync compressed"),
                "{err}"
            );
        }
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
            hash: String::new(),
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
            hash: String::new(),
        };
        doc.merge_ids(&other);
        assert_eq!(doc.id, "doc2");
        assert_eq!(doc.rev, Some("2-def".into()));
        // Other fields should remain from doc
        assert_eq!(doc.path, "doc1");
        assert_eq!(doc.size, 100);
    }
}
