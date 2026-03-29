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
}
