use serde::{Deserialize, Serialize};
use std::ops::Deref;
use std::str::FromStr;

/// A file size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileSize(u64);

impl FileSize {
    #[must_use]
    pub const fn new(size: u64) -> Self {
        Self(size)
    }

    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for FileSize {
    fn from(size: u64) -> Self {
        Self(size)
    }
}

impl From<FileSize> for u64 {
    fn from(s: FileSize) -> Self {
        s.0
    }
}

impl Deref for FileSize {
    type Target = u64;
    fn deref(&self) -> &u64 {
        &self.0
    }
}

impl rusqlite::types::ToSql for FileSize {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let v: i64 = i64::try_from(self.0).unwrap_or(i64::MAX);
        Ok(rusqlite::types::ToSqlOutput::from(v))
    }
}

impl rusqlite::types::FromSql for FileSize {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        i64::column_result(value).map(|v| Self(u64::try_from(v).unwrap_or(0)))
    }
}

/// A `CouchDB` database name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseName(String);

impl DatabaseName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DatabaseName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Deref for DatabaseName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl FromStr for DatabaseName {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

/// A remote path prefix for `CouchDB` documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePath(String);

impl RemotePath {
    /// Build a remote path prefix from raw configuration input.
    ///
    /// # Documented contract
    ///
    /// The empty string and `"/"` both mean the whole database (no remote
    /// prefix). Any other value is normalized to a single, leading-slash-free,
    /// trailing-slash form (`"notes"` → `"notes/"`, `"/notes/"` → `"notes/"`),
    /// so equivalent spellings always compare equal and never silently select
    /// a different sync scope.
    pub fn new(path: impl Into<String>) -> Self {
        let mut p = path.into().trim().to_string();
        // Whole-database: empty and "/" are the same documented contract.
        if p.is_empty() || p == "/" || p == "\\" {
            p.clear();
        } else {
            // Strip any leading slashes so the prefix is relative to the DB root.
            while p.starts_with('/') {
                p.remove(0);
            }
            if !p.ends_with('/') {
                p.push('/');
            }
        }
        Self(p)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The validated sync mapping that a local state database is bound to.
///
/// Stored state (file states, conflicts, and the change-feed checkpoint) is
/// only ever reused when the mapping that produced it matches the mapping the
/// engine is about to sync. The identity is the canonical local root, the
/// remote database name, the remote database UUID (which changes when the
/// database is recreated), and the normalized remote prefix. Changing any of
/// these means the stored state belongs to a different sync scope and must not
/// be reused without an explicit reset/rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncIdentity {
    /// Canonical (symlink-resolved) local sync root.
    pub root: String,
    /// Remote `CouchDB` database name.
    pub database: String,
    /// Remote `CouchDB` database UUID; changes when the database is recreated.
    pub db_uuid: String,
    /// Normalized remote path prefix (empty = whole database).
    pub prefix: String,
}

impl SyncIdentity {
    #[must_use]
    pub fn new(
        root: impl Into<String>,
        database: impl Into<String>,
        db_uuid: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Self {
        Self {
            root: root.into(),
            database: database.into(),
            db_uuid: db_uuid.into(),
            prefix: prefix.into(),
        }
    }
}

impl std::fmt::Display for RemotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Deref for RemotePath {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// A time bucket for touch tracking (5-second intervals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchBucket(i64);

impl TouchBucket {
    #[must_use]
    pub const fn new(bucket: i64) -> Self {
        Self(bucket)
    }

    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl From<i64> for TouchBucket {
    fn from(v: i64) -> Self {
        Self(v)
    }
}

/// Number of files uploaded in a sync operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadCount(pub usize);

impl UploadCount {
    #[must_use]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for UploadCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Number of files downloaded in a sync operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DownloadCount(pub usize);

impl DownloadCount {
    #[must_use]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for DownloadCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A sync checkpoint with the last sequence ID and timestamp.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub last_seq: String,
    pub last_sync_at: chrono::DateTime<chrono::Utc>,
}

impl Checkpoint {
    #[must_use]
    pub const fn new(last_seq: String, last_sync_at: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            last_seq,
            last_sync_at,
        }
    }
}

#[cfg(test)]
mod compile_tests {
    use super::*;

    /// Verify that domain primitives are distinct types and prevent accidental swaps.
    /// This test should always compile; if it fails, the type system is working.
    #[test]
    fn test_types_are_distinct() {
        let _size = FileSize::new(1024);
        let _bucket = TouchBucket::new(42);
        let _upload = UploadCount::new(5);
        let _download = DownloadCount::new(3);
        let _db = DatabaseName::new("mydb");
        let _remote = RemotePath::new("notes/");
        let _checkpoint = Checkpoint::new("seq-1".into(), chrono::Utc::now());

        // Verify conversions work
        let _: u64 = FileSize::new(100).into();
        let _: i64 = TouchBucket::new(7).as_i64();
        let _: usize = UploadCount::new(2).as_usize();
        let _: usize = DownloadCount::new(1).as_usize();

        // Verify Display
        assert_eq!(format!("{}", UploadCount::new(42)), "42");
        assert_eq!(format!("{}", DownloadCount::new(7)), "7");
        assert_eq!(format!("{:?}", FileSize::new(2048)), "FileSize(2048)");

        // Each type has a different inner type:
        // FileSize wraps u64
        // TouchBucket wraps i64
        // UploadCount/DownloadCount wrap usize
        // DatabaseName/RemotePath wrap String
        // So accidental assignment across these will fail to compile.
    }

    #[test]
    fn remote_path_documented_contract() {
        // Whole-database: empty and "/" are the same documented contract.
        assert_eq!(RemotePath::new("").as_str(), "");
        assert_eq!(RemotePath::new("/").as_str(), "");
        assert_eq!(RemotePath::new("  ").as_str(), "");
        assert_eq!(RemotePath::new(" / ").as_str(), "");

        // Non-root prefixes normalize to a leading-slash-free, trailing-slash
        // form so equivalent spellings always compare equal.
        assert_eq!(RemotePath::new("notes").as_str(), "notes/");
        assert_eq!(RemotePath::new("notes/").as_str(), "notes/");
        assert_eq!(RemotePath::new("/notes").as_str(), "notes/");
        assert_eq!(RemotePath::new("/notes/").as_str(), "notes/");
        assert_eq!(RemotePath::new("obsidian/sub").as_str(), "obsidian/sub/");
    }

    #[test]
    fn sync_identity_roundtrips_through_serde() {
        let id = SyncIdentity::new("/srv/sync-root", "files_db", "a1b2c3-uuid", "notes/");
        let raw = serde_json::to_string(&id).unwrap();
        let back: SyncIdentity = serde_json::from_str(&raw).unwrap();
        assert_eq!(id, back);
        assert_eq!(back.root, "/srv/sync-root");
        assert_eq!(back.database, "files_db");
        assert_eq!(back.db_uuid, "a1b2c3-uuid");
        assert_eq!(back.prefix, "notes/");
    }

    #[test]
    fn sync_identity_equality_catches_every_scope_change() {
        let base = SyncIdentity::new("/root", "db", "uuid-1", "old/");
        let new_root = SyncIdentity::new("/other", "db", "uuid-1", "old/");
        let new_db = SyncIdentity::new("/root", "other_db", "uuid-1", "old/");
        let new_uuid = SyncIdentity::new("/root", "db", "uuid-2", "old/");
        let new_prefix = SyncIdentity::new("/root", "db", "uuid-1", "new/");
        assert_ne!(base, new_root);
        assert_ne!(base, new_db);
        assert_ne!(base, new_uuid);
        assert_ne!(base, new_prefix);
    }
}
