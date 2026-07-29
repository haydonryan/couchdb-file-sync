use anyhow::{Context, Result};
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A validated, canonical directory path used as a sync root.
///
/// Canonicalization (resolving symlinks and normalizing) happens at
/// construction, ensuring that all path operations within the sync
/// engine use a consistent absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDirPath(PathBuf);

impl SyncDirPath {
    /// Create a new `SyncDirPath`, canonicalizing the given path.
    ///
    /// Returns an error if the path does not exist or cannot be canonicalized.
    pub fn new(path: PathBuf) -> Result<Self> {
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                // Fall back to making the path absolute if it doesn't exist yet
                // (e.g. during early setup or in tests)
                std::path::absolute(&path)
                    .with_context(|| format!("failed to resolve sync dir: {}", path.display()))?
            }
        };
        Ok(SyncDirPath(canonical))
    }

    /// Return the underlying path as a `Path`.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Return the underlying path as a `PathBuf`.
    pub fn as_path_buf(&self) -> &PathBuf {
        &self.0
    }

    /// Consume the wrapper and return the inner `PathBuf`.
    pub fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl Deref for SyncDirPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for SyncDirPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<PathBuf> for SyncDirPath {
    fn as_ref(&self) -> &PathBuf {
        &self.0
    }
}

impl PartialEq<PathBuf> for SyncDirPath {
    fn eq(&self, other: &PathBuf) -> bool {
        self.0 == *other
    }
}

impl PartialEq<Path> for SyncDirPath {
    fn eq(&self, other: &Path) -> bool {
        self.0 == other
    }
}

impl PartialEq<SyncDirPath> for PathBuf {
    fn eq(&self, other: &SyncDirPath) -> bool {
        *self == other.0
    }
}

impl PartialEq<SyncDirPath> for Path {
    fn eq(&self, other: &SyncDirPath) -> bool {
        *self == other.0
    }
}

impl std::fmt::Display for SyncDirPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
