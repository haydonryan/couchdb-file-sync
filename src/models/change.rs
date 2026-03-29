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

    // =========================================================================
    // Tests for ChangeType Display
    // =========================================================================

    #[test]
    fn change_type_display_created() {
        assert_eq!(format!("{}", ChangeType::Created), "created");
    }

    #[test]
    fn change_type_display_modified() {
        assert_eq!(format!("{}", ChangeType::Modified), "modified");
    }

    #[test]
    fn change_type_display_deleted() {
        assert_eq!(format!("{}", ChangeType::Deleted), "deleted");
    }

    // =========================================================================
    // Tests for local Change factory methods
    // =========================================================================

    #[test]
    fn change_local_created_sets_correct_fields() {
        let change = Change::local_created("test.txt".to_string(), "abc123".to_string(), 100);

        assert_eq!(change.path, "test.txt");
        assert_eq!(change.change_type, ChangeType::Created);
        assert!(matches!(change.source, ChangeSource::Local));
        assert_eq!(change.hash, Some("abc123".to_string()));
        assert_eq!(change.size, Some(100));
        assert!(change.mtime.is_none());
        assert!(change.rev.is_none());
    }

    #[test]
    fn change_local_modified_sets_correct_fields() {
        let change = Change::local_modified("doc.md".to_string(), "def456".to_string(), 200);

        assert_eq!(change.path, "doc.md");
        assert_eq!(change.change_type, ChangeType::Modified);
        assert!(matches!(change.source, ChangeSource::Local));
        assert_eq!(change.hash, Some("def456".to_string()));
        assert_eq!(change.size, Some(200));
        assert!(change.mtime.is_none());
        assert!(change.rev.is_none());
    }

    #[test]
    fn change_local_deleted_sets_correct_fields() {
        let change = Change::local_deleted("old.txt".to_string());

        assert_eq!(change.path, "old.txt");
        assert_eq!(change.change_type, ChangeType::Deleted);
        assert!(matches!(change.source, ChangeSource::Local));
        assert!(change.hash.is_none());
        assert!(change.size.is_none());
        assert!(change.mtime.is_none());
        assert!(change.rev.is_none());
    }

    // =========================================================================
    // Tests for remote Change factory methods
    // =========================================================================

    #[test]
    fn change_remote_created_sets_correct_fields() {
        let mtime = Utc::now();
        let change = Change::remote_created(
            "remote.txt".to_string(),
            "hash789".to_string(),
            300,
            mtime,
            "1-abc".to_string(),
        );

        assert_eq!(change.path, "remote.txt");
        assert_eq!(change.change_type, ChangeType::Created);
        assert!(matches!(change.source, ChangeSource::Remote));
        assert_eq!(change.hash, Some("hash789".to_string()));
        assert_eq!(change.size, Some(300));
        assert_eq!(change.mtime, Some(mtime));
        assert_eq!(change.rev, Some("1-abc".to_string()));
    }

    #[test]
    fn change_remote_modified_sets_correct_fields() {
        let mtime = Utc::now();
        let change = Change::remote_modified(
            "modified.txt".to_string(),
            "hash000".to_string(),
            400,
            mtime,
            "2-def".to_string(),
        );

        assert_eq!(change.path, "modified.txt");
        assert_eq!(change.change_type, ChangeType::Modified);
        assert!(matches!(change.source, ChangeSource::Remote));
        assert_eq!(change.hash, Some("hash000".to_string()));
        assert_eq!(change.size, Some(400));
        assert_eq!(change.mtime, Some(mtime));
        assert_eq!(change.rev, Some("2-def".to_string()));
    }

    #[test]
    fn change_remote_deleted_sets_correct_fields() {
        let change = Change::remote_deleted("deleted-remote.txt".to_string());

        assert_eq!(change.path, "deleted-remote.txt");
        assert_eq!(change.change_type, ChangeType::Deleted);
        assert!(matches!(change.source, ChangeSource::Remote));
        assert!(change.hash.is_none());
        assert!(change.size.is_none());
        assert!(change.mtime.is_none());
        assert!(change.rev.is_none());
    }

    // =========================================================================
    // Tests for ChangeBatch operations
    // =========================================================================

    #[test]
    fn change_batch_new_is_empty() {
        let batch = ChangeBatch::new();
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
    }

    #[test]
    fn change_batch_default_is_empty() {
        let batch: ChangeBatch = Default::default();
        assert!(batch.is_empty());
    }

    #[test]
    fn change_batch_push_increases_len() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created(
            "a.txt".to_string(),
            "h1".to_string(),
            10,
        ));
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 1);

        batch.push(Change::local_modified(
            "b.txt".to_string(),
            "h2".to_string(),
            20,
        ));
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn change_batch_local_changes_filters_correctly() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created(
            "local.txt".to_string(),
            "h1".to_string(),
            10,
        ));
        batch.push(Change::remote_created(
            "remote.txt".to_string(),
            "h2".to_string(),
            20,
            Utc::now(),
            "1-abc".to_string(),
        ));
        batch.push(Change::local_deleted("deleted.txt".to_string()));

        let local = batch.local_changes();
        assert_eq!(local.len(), 2);
        assert!(
            local
                .iter()
                .all(|c| matches!(c.source, ChangeSource::Local))
        );
    }

    #[test]
    fn change_batch_remote_changes_filters_correctly() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created(
            "local.txt".to_string(),
            "h1".to_string(),
            10,
        ));
        batch.push(Change::remote_modified(
            "remote.txt".to_string(),
            "h2".to_string(),
            20,
            Utc::now(),
            "1-abc".to_string(),
        ));
        batch.push(Change::remote_deleted("deleted.txt".to_string()));

        let remote = batch.remote_changes();
        assert_eq!(remote.len(), 2);
        assert!(
            remote
                .iter()
                .all(|c| matches!(c.source, ChangeSource::Remote))
        );
    }

    #[test]
    fn change_batch_empty_filters_return_empty() {
        let batch = ChangeBatch::new();
        assert!(batch.local_changes().is_empty());
        assert!(batch.remote_changes().is_empty());
    }
}
