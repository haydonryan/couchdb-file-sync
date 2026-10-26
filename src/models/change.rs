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
            Self::Created => write!(f, "created"),
            Self::Modified => write!(f, "modified"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

/// Source of the change
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeSource {
    Local,
    Remote,
}

/// Remote revision a local publication is authorized against.
///
/// Carried from the planning/triage decision into metadata publication so a
/// stale baseline is rejected instead of overwriting a concurrent writer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpectedRemote {
    /// The remote doc should not exist yet (create). Presence of a doc is a
    /// concurrent-creation race and must not be overwritten.
    Absent,
    /// The remote doc should be at exactly this revision (conditional
    /// overwrite). Any other revision means a concurrent write happened.
    At(String),
    /// No expectation: overwrite whatever is on the remote. Only set by
    /// explicit force paths (conflict resolution, remote rebuild).
    Force,
}

/// A change record for sync operations - per-variant sum type preventing invalid combinations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Change {
    LocalCreated {
        path: String,
        hash: String,
        size: u64,
        expected_rev: ExpectedRemote,
    },
    LocalModified {
        path: String,
        hash: String,
        size: u64,
        expected_rev: ExpectedRemote,
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
        /// The `CouchDB` revision of the deletion (soft-delete tombstone or the
        /// hard-delete row's winning rev), when available. Carrying the
        /// monotonic server-assigned revision lets delete ordering be judged
        /// independently of cross-host wall-clock skew, and is absent only for
        /// legacy/external tombstones or hard deletes without a parseable rev.
        rev: Option<String>,
    },
}

impl Change {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::LocalCreated { path, .. }
            | Self::LocalModified { path, .. }
            | Self::LocalDeleted { path }
            | Self::RemoteCreated { path, .. }
            | Self::RemoteModified { path, .. }
            | Self::RemoteDeleted { path, .. } => path,
        }
    }

    #[must_use]
    pub const fn change_type(&self) -> ChangeType {
        match self {
            Self::LocalCreated { .. } | Self::RemoteCreated { .. } => ChangeType::Created,
            Self::LocalModified { .. } | Self::RemoteModified { .. } => ChangeType::Modified,
            Self::LocalDeleted { .. } | Self::RemoteDeleted { .. } => ChangeType::Deleted,
        }
    }

    #[must_use]
    pub const fn source(&self) -> ChangeSource {
        match self {
            Self::LocalCreated { .. } | Self::LocalModified { .. } | Self::LocalDeleted { .. } => {
                ChangeSource::Local
            }
            Self::RemoteCreated { .. }
            | Self::RemoteModified { .. }
            | Self::RemoteDeleted { .. } => ChangeSource::Remote,
        }
    }

    #[must_use]
    pub fn hash(&self) -> Option<&str> {
        match self {
            Self::LocalCreated { hash, .. }
            | Self::LocalModified { hash, .. }
            | Self::RemoteCreated { hash, .. }
            | Self::RemoteModified { hash, .. } => Some(hash),
            Self::LocalDeleted { .. } | Self::RemoteDeleted { .. } => None,
        }
    }

    #[must_use]
    pub const fn size(&self) -> Option<u64> {
        match self {
            Self::LocalCreated { size, .. }
            | Self::LocalModified { size, .. }
            | Self::RemoteCreated { size, .. }
            | Self::RemoteModified { size, .. } => Some(*size),
            Self::LocalDeleted { .. } | Self::RemoteDeleted { .. } => None,
        }
    }

    #[must_use]
    pub const fn mtime(&self) -> Option<&DateTime<Utc>> {
        match self {
            Self::RemoteCreated { mtime, .. } | Self::RemoteModified { mtime, .. } => Some(mtime),
            Self::LocalCreated { .. } | Self::LocalModified { .. } | Self::LocalDeleted { .. } => {
                None
            }
            Self::RemoteDeleted { mtime, .. } => mtime.as_ref(),
        }
    }

    #[must_use]
    pub fn rev(&self) -> Option<&str> {
        match self {
            Self::RemoteCreated { rev, .. } | Self::RemoteModified { rev, .. } => Some(rev),
            Self::RemoteDeleted { rev, .. } => rev.as_deref(),
            Self::LocalCreated { .. } | Self::LocalModified { .. } | Self::LocalDeleted { .. } => {
                None
            }
        }
    }

    #[must_use]
    pub const fn local_created(path: String, hash: String, size: u64) -> Self {
        Self::LocalCreated {
            path,
            hash,
            size,
            expected_rev: ExpectedRemote::Force,
        }
    }

    #[must_use]
    pub const fn local_modified(path: String, hash: String, size: u64) -> Self {
        Self::LocalModified {
            path,
            hash,
            size,
            expected_rev: ExpectedRemote::Force,
        }
    }

    #[must_use]
    pub const fn local_deleted(path: String) -> Self {
        Self::LocalDeleted { path }
    }

    #[must_use]
    pub const fn remote_created(
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    ) -> Self {
        Self::RemoteCreated {
            path,
            hash,
            size,
            mtime,
            rev,
        }
    }

    #[must_use]
    pub const fn remote_modified(
        path: String,
        hash: String,
        size: u64,
        mtime: DateTime<Utc>,
        rev: String,
    ) -> Self {
        Self::RemoteModified {
            path,
            hash,
            size,
            mtime,
            rev,
        }
    }

    #[must_use]
    pub const fn remote_deleted(
        path: String,
        mtime: Option<DateTime<Utc>>,
        rev: Option<String>,
    ) -> Self {
        Self::RemoteDeleted { path, mtime, rev }
    }

    /// The expected remote revision carried by a local upload decision. Only
    /// local create/modify changes carry a real expectation; every other
    /// change (deletes, remote-origin changes) has no publication baseline and
    /// defaults to [`ExpectedRemote::Force`].
    #[must_use]
    pub fn expected_rev(&self) -> ExpectedRemote {
        match self {
            Self::LocalCreated { expected_rev, .. } | Self::LocalModified { expected_rev, .. } => {
                expected_rev.clone()
            }
            _ => ExpectedRemote::Force,
        }
    }

    /// Override the expected remote revision for this local upload decision.
    /// No-op for changes that are not local create/modify.
    #[must_use]
    pub fn with_expected_rev(mut self, expected_rev: ExpectedRemote) -> Self {
        match &mut self {
            Self::LocalCreated {
                expected_rev: slot, ..
            }
            | Self::LocalModified {
                expected_rev: slot, ..
            } => *slot = expected_rev,
            _ => {}
        }
        self
    }
}

/// A batch of changes for sync operations
#[derive(Debug, Clone, Default)]
pub struct ChangeBatch {
    changes: Vec<Change>,
}

impl ChangeBatch {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            changes: Vec::new(),
        }
    }

    pub fn push(&mut self, change: Change) {
        self.changes.push(change);
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.changes.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter()
    }

    #[must_use]
    pub fn local_changes(&self) -> Vec<Change> {
        self.changes
            .iter()
            .filter(|c| c.source() == ChangeSource::Local)
            .cloned()
            .collect()
    }

    #[must_use]
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
        let c = Change::remote_deleted("/remote/path.txt".into(), None, None);
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
        batch.push(Change::remote_deleted("b.txt".into(), None, None));
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_change_batch_local_changes_filter() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into(), None, None));
        batch.push(Change::local_deleted("c.txt".into()));
        let locals = batch.local_changes();
        assert_eq!(locals.len(), 2);
        assert!(locals.iter().all(|c| c.source() == ChangeSource::Local));
    }

    #[test]
    fn test_change_batch_remote_changes_filter() {
        let mut batch = ChangeBatch::new();
        batch.push(Change::local_created("a.txt".into(), "h1".into(), 100));
        batch.push(Change::remote_deleted("b.txt".into(), None, None));
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
}
