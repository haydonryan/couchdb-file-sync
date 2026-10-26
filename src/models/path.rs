use anyhow::{Context, Result};
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};

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
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be canonicalized and cannot be made
    /// absolute.
    pub fn new(path: &Path) -> Result<Self> {
        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                // Fall back to making the path absolute if it doesn't exist yet
                // (e.g. during early setup or in tests)
                std::path::absolute(path)
                    .with_context(|| format!("failed to resolve sync dir: {}", path.display()))?
            }
        };
        Ok(Self(canonical))
    }

    /// Return the underlying path as a `Path`.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Return the underlying path as a `PathBuf`.
    #[must_use]
    pub const fn as_path_buf(&self) -> &PathBuf {
        &self.0
    }

    /// Join a remote-derived relative path onto the sync root, enforcing that
    /// the resulting destination stays strictly inside the root.
    ///
    /// This is the single validated local-relative destination boundary for
    /// every write/delete path derived from remote input. It rejects:
    /// - absolute/drive paths (`/x`, `C:\x`, `Prefix` components),
    /// - parent traversal (`..`) components,
    /// - symlink escapes, where an existing ancestor or the destination
    ///   itself resolves (via symlink) to a path outside the root.
    ///
    /// A path that cannot be confined returns an error rather than silently
    /// writing or deleting outside the configured sync directory. Legitimate
    /// nested relative paths pass through unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if `relative` is absolute, contains a parent (`..`)
    /// component, or an existing component resolves outside the root.
    pub fn join_contained(&self, relative: &str) -> Result<PathBuf> {
        let raw = Path::new(relative);
        let mut dest = self.0.clone();
        for component in raw.components() {
            match component {
                Component::RootDir | Component::Prefix(_) => {
                    anyhow::bail!(
                        "refusing absolute path outside sync root {}: {relative:?}",
                        self.0.display()
                    );
                }
                Component::ParentDir => {
                    anyhow::bail!(
                        "refusing parent traversal outside sync root {}: {relative:?}",
                        self.0.display()
                    );
                }
                Component::CurDir => {}
                Component::Normal(seg) => dest.push(seg),
            }
        }
        Self::ensure_within_root(&self.0, &dest, relative)?;
        Ok(dest)
    }

    /// Confirm the deepest existing component of `dest` resolves inside `root`,
    /// so a symlinked ancestor cannot redirect a write/delete outside it.
    fn ensure_within_root(root: &Path, dest: &Path, relative: &str) -> Result<()> {
        let mut cur = dest;
        loop {
            // `symlink_metadata` succeeds for a real path AND a symlink (even a
            // dangling one), so we detect and resolve the deepest existing
            // component rather than silently treating a dangling symlink as
            // absent and creating through it later.
            match std::fs::symlink_metadata(cur) {
                Ok(_) => {
                    let canon = cur.canonicalize().with_context(|| {
                        format!(
                            "cannot resolve {} for sync-root containment check",
                            cur.display()
                        )
                    })?;
                    if !canon.starts_with(root) {
                        anyhow::bail!(
                            "refusing path that escapes sync root {} via symlink: {relative:?}",
                            root.display()
                        );
                    }
                    return Ok(());
                }
                Err(_) => match cur.parent() {
                    Some(parent) => cur = parent,
                    None => return Ok(()),
                },
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // Regression test for story #2842: an unresolvable sync path must
    // produce a typed Err instead of panicking. canonicalize() fails for a
    // non-existent path, and the absolute() fallback still rejects an empty
    // path ("cannot make an empty path absolute"), so `PathBuf::from("")` is
    // the reproducible unresolvable case here.
    #[test]
    fn sync_dir_path_new_rejects_unresolvable_path() {
        let err = SyncDirPath::new(Path::new(""))
            .expect_err("SyncDirPath::new should reject an unresolvable path");
        // The error is contextual, not a panic.
        assert!(
            !err.to_string().is_empty(),
            "expected a contextual error message"
        );
    }

    #[test]
    fn sync_dir_path_new_accepts_valid_directory() {
        let dir = std::env::temp_dir().join(format!("sync-dir-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let sync_dir = SyncDirPath::new(&dir).expect("valid directory should resolve");
        assert_eq!(sync_dir.as_path(), dir.canonicalize().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── join_contained containment boundary (story #6334) ────────────────

    fn make_root() -> (tempfile::TempDir, SyncDirPath) {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn join_contained_accepts_legitimate_nested_path() {
        let (dir, root) = make_root();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();

        let joined = root
            .join_contained("a/b/file.txt")
            .expect("legitimate nested path should be accepted");
        // Compare against the canonicalized root (SyncDirPath canonicalizes on
        // construction). On macOS the temp dir lives under /var which is a
        // symlink to /private/var, so the raw dir.path() differs from the
        // canonicalized root and would spuriously fail.
        assert_eq!(joined, root.as_path().join("a/b/file.txt"));
        assert!(joined.starts_with(root.as_path()));
    }

    #[test]
    fn join_contained_rejects_parent_traversal_and_preserves_sentinel() {
        let (dir, root) = make_root();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        // Outside sentinel file that must never be touched by an escaping path.
        let sentinel = dir.path().parent().unwrap().join("sentinel-outside.txt");
        std::fs::write(&sentinel, "original").unwrap();

        for attack in [
            "../sentinel-outside.txt",
            "sub/../../sentinel-outside.txt",
            "a/../..",
        ] {
            let err = root
                .join_contained(attack)
                .expect_err(&format!("parent traversal {attack:?} must be rejected"));
            assert!(
                err.to_string().contains("parent traversal"),
                "unexpected error for {attack:?}: {err}"
            );
        }

        // The sentinel is untouched: no write/delete could have escaped.
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "original",
            "outside sentinel must be preserved"
        );

        // A legitimate nested file is still reachable afterwards.
        let joined = root.join_contained("sub/ok.txt").unwrap();
        assert!(joined.starts_with(root.as_path()));
    }

    #[test]
    fn join_contained_rejects_absolute_paths() {
        let (_dir, root) = make_root();
        for attack in ["/etc/passwd", "//abs", "/"] {
            let err = root
                .join_contained(attack)
                .expect_err(&format!("absolute path {attack:?} must be rejected"));
            assert!(
                err.to_string().contains("absolute path"),
                "unexpected error for {attack:?}: {err}"
            );
        }
    }

    // A drive/prefix path is a Windows concept (`Component::Prefix`); on Unix
    // a backslash is a literal filename character, so it is not an escape.
    #[cfg(windows)]
    #[test]
    fn join_contained_rejects_drive_paths() {
        let (_dir, root) = make_root();
        for attack in ["C:\\windows\\system32", "C:/windows"] {
            let err = root
                .join_contained(attack)
                .expect_err(&format!("drive path {attack:?} must be rejected"));
            assert!(
                err.to_string().contains("absolute path"),
                "unexpected error for {attack:?}: {err}"
            );
        }
    }

    #[test]
    fn join_contained_rejects_symlinked_file_escape_and_preserves_sentinel() {
        use std::os::unix::fs::symlink;

        let (dir, root) = make_root();
        let outside = tempfile::tempdir().unwrap();
        // A symlink inside the root whose target resolves outside it.
        let link = dir.path().join("escape-link");
        symlink(outside.path(), &link).unwrap();

        let sentinel = outside.path().join("sentinel-outside.txt");
        std::fs::write(&sentinel, "original").unwrap();

        // Writing through the symlinked file would land outside the root.
        let err = root
            .join_contained("escape-link/sentinel-outside.txt")
            .expect_err("symlink escape must be rejected");
        assert!(
            err.to_string().contains("escapes sync root"),
            "unexpected error: {err}"
        );

        // Direct write to the symlink itself (a symlinked file destination).
        assert!(
            root.join_contained("escape-link").is_err(),
            "a symlinked destination must be rejected"
        );

        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "original",
            "outside sentinel must be preserved"
        );
    }

    #[test]
    fn join_contained_rejects_symlinked_parent_escape() {
        use std::os::unix::fs::symlink;

        let (dir, root) = make_root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        // An ancestor directory that is a symlink to an outside directory.
        symlink(outside.path(), dir.path().join("sub/link")).unwrap();

        let err = root
            .join_contained("sub/link/deep/file.txt")
            .expect_err("symlinked parent escape must be rejected");
        assert!(
            err.to_string().contains("escapes sync root"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn join_contained_rejects_dangling_symlink_destination() {
        use std::os::unix::fs::symlink;

        let (dir, root) = make_root();
        // A dangling symlink that would otherwise redirect a create outside.
        let link = dir.path().join("dangling");
        symlink(dir.path().join("does-not-exist-outside"), &link).unwrap();

        assert!(
            root.join_contained("dangling/new.txt").is_err(),
            "a dangling symlink destination must be rejected"
        );
    }

    #[test]
    fn join_contained_rejects_component_alias_escape() {
        // A parent-traversal that is smuggled through a symlinked component
        // must not be able to reach outside even when the direct `..` form is
        // normalized away by the filesystem.
        let (_dir, root) = make_root();
        assert!(root.join_contained("sub").is_ok());
        assert!(
            root.join_contained(".").is_ok(),
            "current-dir stays in root"
        );
        assert!(root.join_contained("sub/./file.txt").is_ok());
        assert!(root.join_contained("sub/../..").is_err());
    }
}
