use crate::models::{Change, FileState, IgnoreMatcher, SyncDirPath};
use anyhow::Result;
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

/// Scans the filesystem for changes
#[derive(Clone)]
pub struct Scanner {
    root_dir: SyncDirPath,
    ignore_matcher: IgnoreMatcher,
}

impl Scanner {
    /// Create a new scanner for the given root directory
    pub fn new(root_dir: SyncDirPath, ignore_matcher: IgnoreMatcher) -> Self {
        Self {
            root_dir,
            ignore_matcher,
        }
    }

    /// Perform a full scan of the directory.
    ///
    /// The blocking walkdir/hash work runs on tokio's blocking thread pool via
    /// [`tokio::task::spawn_blocking`] so the async executor is never stalled by
    /// the scan.
    pub async fn full_scan(&self) -> Result<Vec<FileState>> {
        let scanner = self.clone();
        tokio::task::spawn_blocking(move || scanner.scan_blocking()).await?
    }

    /// Walk the directory tree and hash every file using blocking filesystem
    /// calls. Called on a blocking thread pool by [`Self::full_scan`].
    fn scan_blocking(&self) -> Result<Vec<FileState>> {
        let mut states = Vec::new();

        for entry in WalkDir::new(&self.root_dir).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("Error walking directory: {}", e);
                    continue;
                }
            };

            let path = entry.path();

            // Skip directories and non-files
            if !entry.file_type().is_file() {
                continue;
            }

            // Check ignore patterns
            let relative_path = match path.strip_prefix(self.root_dir.as_path()) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if self.ignore_matcher.should_ignore(relative_path) {
                trace!("Ignoring file: {}", relative_path.display());
                continue;
            }

            // Get file metadata and hash
            match self.scan_file(path) {
                Ok(state) => {
                    debug!("Scanned file: {} (hash: {})", state.path, &state.hash[..8]);
                    states.push(state);
                }
                Err(e) => {
                    warn!("Failed to scan file {}: {}", path.display(), e);
                }
            }
        }

        Ok(states)
    }

    /// Scan a single file
    pub fn scan_file(&self, path: &Path) -> Result<FileState> {
        let metadata = std::fs::metadata(path)?;

        // Try the lexical root strip_prefix first. In production, scan_blocking
        // walks the already-canonical SyncDirPath root, so this textual prefix
        // comparison succeeds and we avoid the redundant path.canonicalize()
        // realpath syscall on every scanned file. Only when the lexical
        // strip_prefix fails (e.g. a caller hands scan_file a non-canonical
        // path through a symlinked root, as in #2941) do we canonicalize and
        // re-derive the residual path with the canonical-with-lexical fallback
        // below.
        let (resolved_path, relative_path) = match path.strip_prefix(self.root_dir.as_path()) {
            Ok(rel) => (path.to_path_buf(), rel.to_path_buf()),
            Err(_) => {
                // SyncDirPath::new() canonicalizes the scan root at
                // construction. On macOS, temp/sync roots under /var/folders
                // are symlinks to their real location under /private/var/
                // folders, so a caller may hand scan_file a lexical path whose
                // textual prefix does not match the canonical root, and a
                // naive strip_prefix would fail with StripPrefixError ("prefix
                // not found"). Resolve the scanned path before the comparison
                // so the residual-path derivation is robust to symlinked
                // roots. If the path is itself a symlink pointing outside the
                // root (or cannot be resolved), fall back to the lexical path
                // so existing behavior is preserved.
                let resolved = match path.canonicalize() {
                    Ok(canonical) if canonical.starts_with(self.root_dir.as_path()) => canonical,
                    _ => path.to_path_buf(),
                };
                let relative = resolved
                    .strip_prefix(self.root_dir.as_path())?
                    .to_path_buf();
                (resolved, relative)
            }
        };
        let path_str = relative_path.to_string_lossy().to_string();

        // Compute hash / mtime from the same resolved path as the relative-path
        // derivation so hashing and residual derivation stay consistent.
        let hash = compute_file_hash(&resolved_path)?;

        // Get modification time
        let modified_at = metadata.modified()?.into();

        Ok(FileState {
            path: path_str,
            hash,
            size: metadata.len(),
            modified_at,
            couch_rev: None,
            last_sync_at: Utc::now(),
        })
    }

    /// Detect changes by comparing current state with stored state
    pub fn detect_changes(
        &self,
        current_states: &[FileState],
        stored_states: &[FileState],
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        let stored_map: std::collections::HashMap<_, _> =
            stored_states.iter().map(|s| (&s.path, s)).collect();
        let current_map: std::collections::HashMap<_, _> =
            current_states.iter().map(|s| (&s.path, s)).collect();

        // Detect created and modified files
        for state in current_states {
            match stored_map.get(&state.path) {
                None => {
                    // New file
                    info!(
                        "New local file detected: {} (size: {} bytes)",
                        state.path, state.size
                    );
                    changes.push(Change::local_created(
                        state.path.clone(),
                        state.hash.clone(),
                        state.size,
                    ));
                }
                Some(stored) => {
                    // Check if modified
                    if state.hash != stored.hash {
                        info!("Modified local file detected: {}", state.path);
                        info!(
                            "  hash: {} -> {}",
                            &stored.hash[..8.min(stored.hash.len())],
                            &state.hash[..8.min(state.hash.len())]
                        );
                        if state.size != stored.size {
                            info!("  size: {} -> {} bytes", stored.size, state.size);
                        }
                        info!(
                            "  mtime: {} -> {}",
                            stored.modified_at.format("%Y-%m-%d %H:%M:%S"),
                            state.modified_at.format("%Y-%m-%d %H:%M:%S")
                        );
                        changes.push(Change::local_modified(
                            state.path.clone(),
                            state.hash.clone(),
                            state.size,
                        ));
                    }
                }
            }
        }

        // Detect deleted files
        for stored in stored_states {
            if !current_map.contains_key(&stored.path) {
                changes.push(Change::local_deleted(stored.path.clone()));
            }
        }

        changes
    }

    /// Quick scan for a specific path
    pub fn scan_single(&self, relative_path: &Path) -> Result<Option<FileState>> {
        let full_path = self.root_dir.as_path().join(relative_path);

        if !full_path.exists() {
            return Ok(None);
        }

        if !full_path.is_file() {
            return Ok(None);
        }

        if self.ignore_matcher.should_ignore(relative_path) {
            return Ok(None);
        }

        self.scan_file(&full_path).map(Some)
    }
}

/// Compute SHA-256 hash of a file
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    let result = hasher.finalize();
    Ok(hex::encode(result))
}

/// Compute hash from bytes
pub fn compute_bytes_hash(bytes: &[u8]) -> String {
    let result = Sha256::digest(bytes);
    hex::encode(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::change::{ChangeSource, ChangeType};
    use crate::models::IgnoreMatcher;
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn test_compute_file_hash() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(b"hello world").unwrap();
        drop(file);

        let hash1 = compute_file_hash(&file_path).unwrap();
        let hash2 = compute_file_hash(&file_path).unwrap();

        assert_eq!(hash1, hash2, "Hash should be consistent");
        assert_eq!(hash1.len(), 64, "SHA-256 hash should be 64 hex chars");
    }

    #[test]
    fn test_hash_changes_with_content() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        // First content
        std::fs::write(&file_path, "content1").unwrap();
        let hash1 = compute_file_hash(&file_path).unwrap();

        // Second content
        std::fs::write(&file_path, "content2").unwrap();
        let hash2 = compute_file_hash(&file_path).unwrap();

        assert_ne!(hash1, hash2, "Hash should change when content changes");
    }

    // ---- scan_file tests ----

    #[test]
    fn test_scan_file_returns_correct_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("hello.txt");
        std::fs::write(&file_path, b"Hello, world!").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let state = scanner.scan_file(&file_path).unwrap();

        // Path should be relative to root_dir
        assert_eq!(state.path, "hello.txt");
        // Size should match content length
        assert_eq!(state.size, 13);
        // Hash should be SHA-256 (64 hex chars)
        assert_eq!(state.hash.len(), 64);
        // Hash should be deterministic
        let expected_hash = compute_file_hash(&file_path).unwrap();
        assert_eq!(state.hash, expected_hash);
        // modified_at should be set
        let now = Utc::now();
        let age = now - state.modified_at;
        assert!(age.num_seconds() < 5, "modified_at should be recent");
    }

    #[test]
    fn test_scan_file_with_nested_file() {
        let temp_dir = TempDir::new().unwrap();
        let nested_dir = temp_dir.path().join("sub").join("dir");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let file_path = nested_dir.join("data.txt");
        std::fs::write(&file_path, b"nested content").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let state = scanner.scan_file(&file_path).unwrap();

        assert_eq!(state.path, "sub/dir/data.txt");
        assert_eq!(state.size, 14);
    }

    // Regression for #2941: on macOS, tempdir paths under /var/folders are
    // symlinked to their real location under /private/var/folders.
    // SyncDirPath::new() canonicalizes the root (so the scanner root becomes
    // /private/var/folders/...) while TempDir hands scan_file the lexical
    // /var/folders/... path, making a naive path.strip_prefix(root) panic with
    // StripPrefixError ("prefix not found"). Linux /tmp is not a symlink, which
    // is why these tests only failed on macos-latest. This test mirrors the
    // mismatch with an explicit Unix symlink: the scanner root is the canonical
    // target path and the scanned file path goes through the symlink.
    #[cfg(unix)]
    #[test]
    fn test_scan_file_with_symlinked_temp_root() {
        use std::os::unix::fs::symlink;

        let base = TempDir::new().unwrap();
        let link_parent = TempDir::new().unwrap();
        let link = link_parent.path().join("linked_root");
        symlink(base.path(), &link).unwrap();

        let file_path = link.join("hello.txt");
        std::fs::write(&file_path, b"Hello, world!").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(base.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let state = scanner.scan_file(&file_path).unwrap();

        // The residual path still derives from the file under the (symlinked)
        // root, and existing FileState.path contents are unchanged.
        assert_eq!(state.path, "hello.txt");
        assert_eq!(state.size, 13);
    }

    #[test]
    fn test_scan_file_returns_error_for_missing_file() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let missing = temp_dir.path().join("does-not-exist.txt");

        let result = scanner.scan_file(&missing);
        assert!(result.is_err(), "scan_file should fail for missing files");
    }

    // ---- full_scan tests ----

    #[tokio::test]
    async fn test_full_scan_returns_all_files() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("a.txt"), b"aaa").unwrap();
        std::fs::write(temp_dir.path().join("b.txt"), b"bbb").unwrap();
        std::fs::write(temp_dir.path().join("c.txt"), b"ccc").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap();

        assert_eq!(states.len(), 3);
        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"));
        assert!(paths.contains(&"b.txt"));
        assert!(paths.contains(&"c.txt"));
    }

    #[tokio::test]
    async fn test_full_scan_with_nested_directories() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("nested")).unwrap();
        std::fs::write(temp_dir.path().join("root.txt"), b"root").unwrap();
        std::fs::write(temp_dir.path().join("nested").join("child.txt"), b"child").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap();

        assert_eq!(states.len(), 2);
        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"root.txt"));
        assert!(paths.contains(&"nested/child.txt"));
    }

    #[tokio::test]
    async fn test_full_scan_skips_directories() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("empty_dir")).unwrap();
        std::fs::write(temp_dir.path().join("file.txt"), b"data").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap();

        assert_eq!(states.len(), 1);
        assert_eq!(states[0].path, "file.txt");
    }

    #[tokio::test]
    async fn test_full_scan_respects_ignore_patterns() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("keep.txt"), b"keep").unwrap();
        std::fs::write(temp_dir.path().join("ignore.log"), b"log data").unwrap();
        std::fs::write(temp_dir.path().join("also_keep.rs"), b"rust").unwrap();

        let matcher = IgnoreMatcher::from_content("*.log");
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            matcher,
        );
        let states = scanner.full_scan().await.unwrap();

        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"keep.txt"));
        assert!(paths.contains(&"also_keep.rs"));
        assert!(!paths.contains(&"ignore.log"));
    }

    #[tokio::test]
    async fn test_full_scan_skips_dotfiles() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("visible.txt"), b"seen").unwrap();
        std::fs::write(temp_dir.path().join(".hidden"), b"hidden").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap();

        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"visible.txt"));
        assert!(!paths.contains(&".hidden"));
    }

    #[tokio::test]
    async fn test_full_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap();

        assert!(
            states.is_empty(),
            "Empty directory should produce no file states"
        );
    }

    // ---- scan_single tests ----

    #[test]
    fn test_scan_single_returns_some_for_existing_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("target.txt");
        std::fs::write(&file_path, b"scan me").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let result = scanner.scan_single(Path::new("target.txt")).unwrap();

        assert!(result.is_some());
        let state = result.unwrap();
        assert_eq!(state.path, "target.txt");
        assert_eq!(state.size, 7);
    }

    #[test]
    fn test_scan_single_returns_none_for_missing_file() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let result = scanner.scan_single(Path::new("nonexistent.txt")).unwrap();

        assert!(
            result.is_none(),
            "scan_single should return None for missing files"
        );
    }

    #[test]
    fn test_scan_single_returns_none_for_ignored_file() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("secret.log"), b"logs").unwrap();

        let matcher = IgnoreMatcher::from_content("*.log");
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            matcher,
        );
        let result = scanner.scan_single(Path::new("secret.log")).unwrap();

        assert!(
            result.is_none(),
            "scan_single should return None for ignored files"
        );
    }

    #[test]
    fn test_scan_single_returns_none_for_dotfile() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join(".hidden"), b"secret").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let result = scanner.scan_single(Path::new(".hidden")).unwrap();

        assert!(
            result.is_none(),
            "scan_single should return None for dotfiles"
        );
    }

    #[test]
    fn test_scan_single_returns_none_for_directory() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(temp_dir.path().join("adir")).unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let result = scanner.scan_single(Path::new("adir")).unwrap();

        assert!(
            result.is_none(),
            "scan_single should return None for directories"
        );
    }

    // ---- detect_changes tests ----

    #[test]
    fn test_detect_changes_no_changes() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let state = FileState::new("file.txt".to_string(), "abc".to_string(), 10, Utc::now());

        let current = vec![state.clone()];
        let stored = vec![state];

        let changes = scanner.detect_changes(&current, &stored);
        assert!(changes.is_empty(), "No changes expected when states match");
    }

    #[test]
    fn test_detect_changes_new_file_created() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let current = vec![FileState::new(
            "new.txt".to_string(),
            "hash1".to_string(),
            42,
            Utc::now(),
        )];
        let stored: Vec<FileState> = vec![];

        let changes = scanner.detect_changes(&current, &stored);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "new.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Created);
        assert_eq!(changes[0].source(), ChangeSource::Local);
        assert_eq!(changes[0].hash(), Some("hash1"));
        assert_eq!(changes[0].size(), Some(42));
    }

    #[test]
    fn test_detect_changes_modified_file() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let stored = vec![FileState::new(
            "edit.txt".to_string(),
            "old_hash".to_string(),
            10,
            Utc::now(),
        )];
        let current = vec![FileState::new(
            "edit.txt".to_string(),
            "new_hash".to_string(),
            20,
            Utc::now(),
        )];

        let changes = scanner.detect_changes(&current, &stored);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "edit.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Modified);
        assert_eq!(changes[0].source(), ChangeSource::Local);
        assert_eq!(changes[0].hash(), Some("new_hash"));
        assert_eq!(changes[0].size(), Some(20));
    }

    #[test]
    fn test_detect_changes_deleted_file() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let stored = vec![FileState::new(
            "gone.txt".to_string(),
            "hash".to_string(),
            10,
            Utc::now(),
        )];
        let current: Vec<FileState> = vec![];

        let changes = scanner.detect_changes(&current, &stored);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "gone.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Deleted);
        assert_eq!(changes[0].source(), ChangeSource::Local);
    }

    #[test]
    fn test_detect_changes_mixed_changes() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path().to_path_buf()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let stored = vec![
            FileState::new("keep.txt".to_string(), "h1".to_string(), 1, Utc::now()),
            FileState::new("modify.txt".to_string(), "h2".to_string(), 2, Utc::now()),
            FileState::new("delete.txt".to_string(), "h3".to_string(), 3, Utc::now()),
        ];
        let current = vec![
            FileState::new("keep.txt".to_string(), "h1".to_string(), 1, Utc::now()),
            FileState::new(
                "modify.txt".to_string(),
                "h2_new".to_string(),
                22,
                Utc::now(),
            ),
            FileState::new("create.txt".to_string(), "h4".to_string(), 4, Utc::now()),
        ];

        let changes = scanner.detect_changes(&current, &stored);
        assert_eq!(changes.len(), 3);
        let change_paths: Vec<&str> = changes.iter().map(|c| c.path()).collect();
        assert!(change_paths.contains(&"create.txt"));
        assert!(change_paths.contains(&"modify.txt"));
        assert!(change_paths.contains(&"delete.txt"));
    }
}
