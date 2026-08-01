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

/// A change record for sync operations - per-variant sum type preventing invalid combinations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Change {
    LocalCreated {
        path: String,
        hash: String,
        size: u64,
    },
    LocalModified {
        path: String,
        hash: String,
        size: u64,
    },
    LocalDeleted {
        path: String,
    },
    RemoteCreated {
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    },
    RemoteModified {
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    },
    RemoteDeleted {
        path: String,
        mtime: Option<DateTime<Utc>>,
    },
}

impl Change {
    pub fn path(&self) -> &str {
        match self {
            Change::LocalCreated { path, .. }
            | Change::LocalModified { path, .. }
            | Change::LocalDeleted { path }
            | Change::RemoteCreated { path, .. }
            | Change::RemoteModified { path, .. }
            | Change::RemoteDeleted { path, .. } => path,
        }
    }

    pub fn change_type(&self) -> ChangeType {
        match self {
            Change::LocalCreated { .. } | Change::RemoteCreated { .. } => ChangeType::Created,
            Change::LocalModified { .. } | Change::RemoteModified { .. } => ChangeType::Modified,
            Change::LocalDeleted { .. } | Change::RemoteDeleted { .. } => ChangeType::Deleted,
        }
    }

    pub fn source(&self) -> ChangeSource {
        match self {
            Change::LocalCreated { .. }
            | Change::LocalModified { .. }
            | Change::LocalDeleted { .. } => ChangeSource::Local,
            Change::RemoteCreated { .. }
            | Change::RemoteModified { .. }
            | Change::RemoteDeleted { .. } => ChangeSource::Remote,
        }
    }

    pub fn hash(&self) -> Option<&str> {
        match self {
            Change::LocalCreated { hash, .. }
            | Change::LocalModified { hash, .. }
            | Change::RemoteCreated { hash, .. }
            | Change::RemoteModified { hash, .. } => Some(hash),
            Change::LocalDeleted { .. } | Change::RemoteDeleted { .. } => None,
        }
    }

    pub fn size(&self) -> Option<u64> {
        match self {
            Change::LocalCreated { size, .. }
            | Change::LocalModified { size, .. }
            | Change::RemoteCreated { size, .. }
            | Change::RemoteModified { size, .. } => Some(*size),
            Change::LocalDeleted { .. } | Change::RemoteDeleted { .. } => None,
        }
    }

    pub fn mtime(&self) -> Option<&DateTime<Utc>> {
        match self {
            Change::RemoteCreated { mtime, .. } | Change::RemoteModified { mtime, .. } => {
                Some(mtime)
            }
            Change::LocalCreated { .. }
            | Change::LocalModified { .. }
            | Change::LocalDeleted { .. } => None,
            Change::RemoteDeleted { mtime, .. } => mtime.as_ref(),
        }
    }

    pub fn rev(&self) -> Option<&str> {
        match self {
            Change::RemoteCreated { rev, .. } | Change::RemoteModified { rev, .. } => Some(rev),
            Change::LocalCreated { .. }
            | Change::LocalModified { .. }
            | Change::LocalDeleted { .. }
            | Change::RemoteDeleted { .. } => None,
        }
    }

    pub fn local_created(path: String, hash: String, size: u64) -> Self {
        Change::LocalCreated { path, hash, size }
    }

    pub fn local_modified(path: String, hash: String, size: u64) -> Self {
        Change::LocalModified { path, hash, size }
    }

    pub fn local_deleted(path: String) -> Self {
        Change::LocalDeleted { path }
    }

    pub fn remote_created(
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    ) -> Self {
        Change::RemoteCreated {
            path,
            hash,
            size,
            mtime,
            rev,
        }
    }

    pub fn remote_modified(
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    ) -> Self {
        Change::RemoteModified {
            path,
            hash,
            size,
            mtime,
            rev,
        }
    }

    pub fn remote_deleted(path: String, mtime: Option<DateTime<Utc>>) -> Self {
        Change::RemoteDeleted { path, mtime }
    }
}

/// A batch of changes for sync operations
#[derive(Debug, Clone, Default)]
pub struct ChangeBatch {
    changes: Vec<Change>,
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

    pub fn iter(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter()
    }

    pub fn local_changes(&self) -> Vec<Change> {
        self.changes
            .iter()
            .filter(|c| c.source() == ChangeSource::Local)
            .cloned()
            .collect()
    }

    pub fn remote_changes(&self) -> Vec<Change> {
        self.changes
            .iter()
            .filter(|c| c.source() == ChangeSource::Remote)
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
        assert_eq!(c.path(), "/path/to/file.txt");
        assert_eq!(c.change_type(), ChangeType::Created);
        assert_eq!(c.source(), ChangeSource::Local);
        assert_eq!(c.hash(), Some("abc123"));
        assert_eq!(c.size(), Some(1024));
        assert!(c.mtime().is_none());
        assert!(c.rev().is_none());
    }

    #[test]
    fn test_change_local_modified() {
        let c = Change::local_modified("/path/to/file.txt".into(), "def456".into(), 2048);
        assert_eq!(c.path(), "/path/to/file.txt");
        assert_eq!(c.change_type(), ChangeType::Modified);
        assert_eq!(c.source(), ChangeSource::Local);
        assert_eq!(c.hash(), Some("def456"));
        assert_eq!(c.size(), Some(2048));
    }

    #[test]
    fn test_change_local_deleted() {
        let c = Change::local_deleted("/path/to/file.txt".into());
        assert_eq!(c.path(), "/path/to/file.txt");
        assert_eq!(c.change_type(), ChangeType::Deleted);
        assert_eq!(c.source(), ChangeSource::Local);
        assert!(c.hash().is_none());
        assert!(c.size().is_none());
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
        assert_eq!(c.path(), "/remote/path.txt");
        assert_eq!(c.change_type(), ChangeType::Created);
        assert_eq!(c.source(), ChangeSource::Remote);
        assert_eq!(c.hash(), Some("hash1"));
        assert_eq!(c.size(), Some(512));
        assert_eq!(c.mtime(), Some(&mtime));
        assert_eq!(c.rev(), Some("1-abc"));
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
        assert_eq!(c.path(), "/remote/path.txt");
        assert_eq!(c.change_type(), ChangeType::Modified);
        assert_eq!(c.source(), ChangeSource::Remote);
        assert_eq!(c.rev(), Some("2-def"));
    }

    #[test]
    fn test_change_remote_deleted() {
        let c = Change::remote_deleted("/remote/path.txt".into(), None);
        assert_eq!(c.path(), "/remote/path.txt");
        assert_eq!(c.change_type(), ChangeType::Deleted);
        assert_eq!(c.source(), ChangeSource::Remote);
        assert!(c.hash().is_none());
        assert!(c.size().is_none());
        assert!(c.mtime().is_none());
        assert!(c.rev().is_none());
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
        batch.push(Change::remote_deleted("b.txt".into(), None));
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_change_batch_local_changes_filter() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into(), None));
        batch.push(Change::local_deleted("c.txt".into()));
        let locals = batch.local_changes();
        assert_eq!(locals.len(), 2);
        assert!(locals.iter().all(|c| c.source() == ChangeSource::Local));
    }

    #[test]
    fn test_change_batch_remote_changes_filter() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into(), None));
        batch.push(Change::remote_created(
            "c.txt".into(),
            "h2".into(),
            200,
            Utc::now(),
            "1-rev".into(),
        ));
        let remotes = batch.remote_changes();
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().all(|c| c.source() == ChangeSource::Remote));
    }

    #[test]
    fn test_change_batch_default_is_empty() {
        let batch = ChangeBatch::default();
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
    }
}
