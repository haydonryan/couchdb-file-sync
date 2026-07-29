use std::ops::Deref;
use std::str::FromStr;

/// A file size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileSize(u64);

impl FileSize {
    pub fn new(size: u64) -> Self {
        FileSize(size)
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for FileSize {
    fn from(size: u64) -> Self {
        FileSize(size)
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
        let v: i64 = self.0 as i64;
        Ok(rusqlite::types::ToSqlOutput::from(v))
    }
}

impl rusqlite::types::FromSql for FileSize {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        i64::column_result(value).map(|v| FileSize(v as u64))
    }
}

/// A CouchDB database name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseName(String);

impl DatabaseName {
    pub fn new(name: impl Into<String>) -> Self {
        DatabaseName(name.into())
    }

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
        Ok(DatabaseName(s.to_string()))
    }
}

/// A remote path prefix for CouchDB documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn new(path: impl Into<String>) -> Self {
        let mut p = path.into();
        // Normalize: ensure it ends with / if not empty
        if !p.is_empty() && !p.ends_with('/') {
            p.push('/');
        }
        RemotePath(p)
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
    pub fn new(bucket: i64) -> Self {
        TouchBucket(bucket)
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl From<i64> for TouchBucket {
    fn from(v: i64) -> Self {
        TouchBucket(v)
    }
}

/// Number of files uploaded in a sync operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UploadCount(pub usize);

impl UploadCount {
    pub fn new(count: usize) -> Self {
        UploadCount(count)
    }

    pub fn as_usize(self) -> usize {
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
    pub fn new(count: usize) -> Self {
        DownloadCount(count)
    }

    pub fn as_usize(self) -> usize {
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
    pub fn new(last_seq: String, last_sync_at: chrono::DateTime<chrono::Utc>) -> Self {
        Checkpoint {
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
}
