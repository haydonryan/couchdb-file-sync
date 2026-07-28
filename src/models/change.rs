use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Type of change detected
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    Created,
    Modified,
    Deleted,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeType::Created => write!(f, "created"),
            ChangeType::Modified => write!(f, "modified"),
            ChangeType::Deleted => write!(f, "deleted"),
        }
    }
}

/// Source of the change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeSource {
    Local,
    Remote,
}

/// A change record for sync operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub change_type: ChangeType,
    pub source: ChangeSource,
    pub timestamp: DateTime<Utc>,
    pub hash: Option<String>,
    pub size: Option<u64>,
    /// Remote modification time (for comparing with local state)
    pub mtime: Option<DateTime<Utc>>,
    /// Remote CouchDB revision
    pub rev: Option<String>,
}

impl Change {
    pub fn new(
        path: String,
        change_type: ChangeType,
        source: ChangeSource,
        hash: Option<String>,
        size: Option<u64>,
        mtime: Option<DateTime<Utc>>,
        rev: Option<String>,
    ) -> Self {
        Self {
            path,
            change_type,
            source,
            timestamp: Utc::now(),
            hash,
            size,
            mtime,
            rev,
        }
    }

    pub fn local_created(path: String, hash: String, size: u64) -> Self {
        Self::new(
            path,
            ChangeType::Created,
            ChangeSource::Local,
            Some(hash),
            Some(size),
            None,
            None,
        )
    }

    pub fn local_modified(path: String, hash: String, size: u64) -> Self {
        Self::new(
            path,
            ChangeType::Modified,
            ChangeSource::Local,
            Some(hash),
            Some(size),
            None,
            None,
        )
    }

    pub fn local_deleted(path: String) -> Self {
        Self::new(
            path,
            ChangeType::Deleted,
            ChangeSource::Local,
            None,
            None,
            None,
            None,
        )
    }

    pub fn remote_created(
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    ) -> Self {
        Self::new(
            path,
            ChangeType::Created,
            ChangeSource::Remote,
            Some(hash),
            Some(size),
            Some(mtime),
            Some(rev),
        )
    }

    pub fn remote_modified(
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    ) -> Self {
        Self::new(
            path,
            ChangeType::Modified,
            ChangeSource::Remote,
            Some(hash),
            Some(size),
            Some(mtime),
            Some(rev),
        )
    }

    pub fn remote_deleted(path: String) -> Self {
        Self::new(
            path,
            ChangeType::Deleted,
            ChangeSource::Remote,
            None,
            None,
            None,
            None,
        )
    }
}

/// A batch of changes for sync operations
#[derive(Debug, Clone, Default)]
pub struct ChangeBatch {
    pub changes: Vec<Change>,
}

impl ChangeBatch {
    pub fn new() -> Self {
        Self {
            changes: Vec::new(),
        }
    }

    pub fn push(&mut self, change: Change) {
        self.changes.push(change);
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn local_changes(&self) -> Vec<Change> {
        self.changes
            .iter()
            .filter(|c| matches!(c.source, ChangeSource::Local))
            .cloned()
            .collect()
    }

    pub fn remote_changes(&self) -> Vec<Change> {
        self.changes
            .iter()
            .filter(|c| matches!(c.source, ChangeSource::Remote))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn test_change_type_display() {
        assert_eq!(format!("{}", ChangeType::Created), "created");
        assert_eq!(format!("{}", ChangeType::Modified), "modified");
        assert_eq!(format!("{}", ChangeType::Deleted), "deleted");
    }

    #[test]
    fn test_change_source_enum_matches() {
        let local = ChangeSource::Local;
        let remote = ChangeSource::Remote;
        assert!(matches!(local, ChangeSource::Local));
        assert!(matches!(remote, ChangeSource::Remote));
        assert!(!matches!(local, ChangeSource::Remote));
        assert!(!matches!(remote, ChangeSource::Local));
    }

    #[test]
    fn test_change_local_created() {
        let c = Change::local_created("/path/to/file.txt".into(), "abc123".into(), 1024);
        assert_eq!(c.path, "/path/to/file.txt");
        assert_eq!(c.change_type, ChangeType::Created);
        assert_eq!(c.source, ChangeSource::Local);
        assert_eq!(c.hash, Some("abc123".into()));
        assert_eq!(c.size, Some(1024));
        assert!(c.mtime.is_none());
        assert!(c.rev.is_none());
    }

    #[test]
    fn test_change_local_modified() {
        let c = Change::local_modified("/path/to/file.txt".into(), "def456".into(), 2048);
        assert_eq!(c.path, "/path/to/file.txt");
        assert_eq!(c.change_type, ChangeType::Modified);
        assert_eq!(c.source, ChangeSource::Local);
        assert_eq!(c.hash, Some("def456".into()));
        assert_eq!(c.size, Some(2048));
    }

    #[test]
    fn test_change_local_deleted() {
        let c = Change::local_deleted("/path/to/file.txt".into());
        assert_eq!(c.path, "/path/to/file.txt");
        assert_eq!(c.change_type, ChangeType::Deleted);
        assert_eq!(c.source, ChangeSource::Local);
        assert!(c.hash.is_none());
        assert!(c.size.is_none());
    }

    #[test]
    fn test_change_remote_created() {
        let mtime = Utc.with_ymd_and_hms(2026, 7, 28, 12, 0, 0).unwrap();
        let c = Change::remote_created(
            "/remote/path.txt".into(),
            "hash1".into(),
            512,
            mtime,
            "1-abc".into(),
        );
        assert_eq!(c.path, "/remote/path.txt");
        assert_eq!(c.change_type, ChangeType::Created);
        assert_eq!(c.source, ChangeSource::Remote);
        assert_eq!(c.hash, Some("hash1".into()));
        assert_eq!(c.size, Some(512));
        assert_eq!(c.mtime, Some(mtime));
        assert_eq!(c.rev, Some("1-abc".into()));
    }

    #[test]
    fn test_change_remote_modified() {
        let mtime = Utc.with_ymd_and_hms(2026, 7, 28, 13, 0, 0).unwrap();
        let c = Change::remote_modified(
            "/remote/path.txt".into(),
            "hash2".into(),
            256,
            mtime,
            "2-def".into(),
        );
        assert_eq!(c.path, "/remote/path.txt");
        assert_eq!(c.change_type, ChangeType::Modified);
        assert_eq!(c.source, ChangeSource::Remote);
        assert_eq!(c.rev, Some("2-def".into()));
    }

    #[test]
    fn test_change_remote_deleted() {
        let c = Change::remote_deleted("/remote/path.txt".into());
        assert_eq!(c.path, "/remote/path.txt");
        assert_eq!(c.change_type, ChangeType::Deleted);
        assert_eq!(c.source, ChangeSource::Remote);
        assert!(c.hash.is_none());
        assert!(c.size.is_none());
        assert!(c.mtime.is_none());
        assert!(c.rev.is_none());
    }

    #[test]
    fn test_change_batch_new_and_empty() {
        let batch = ChangeBatch::new();
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
    }

    #[test]
    fn test_change_batch_push_and_len() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into()));
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_change_batch_local_changes_filter() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into()));
        batch.push(Change::local_deleted("c.txt".into()));
        let locals = batch.local_changes();
        assert_eq!(locals.len(), 2);
        assert!(locals.iter().all(|c| matches!(c.source, ChangeSource::Local)));
    }

    #[test]
    fn test_change_batch_remote_changes_filter() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into()));
        batch.push(Change::remote_created("c.txt".into(), "h2".into(), 200, Utc::now(), "1-rev".into()));
        let remotes = batch.remote_changes();
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().all(|c| matches!(c.source, ChangeSource::Remote)));
    }

    #[test]
    fn test_change_batch_default_is_empty() {
        let batch = ChangeBatch::default();
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
    }
}
