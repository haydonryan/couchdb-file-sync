use crate::models::{Change, FileState, IgnoreMatcher};
use anyhow::Result;
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

/// Scans the filesystem for changes
pub struct Scanner {
    root_dir: PathBuf,
    ignore_matcher: IgnoreMatcher,
}

impl Scanner {
    /// Create a new scanner for the given root directory
    pub fn new(root_dir: PathBuf, ignore_matcher: IgnoreMatcher) -> Self {
        Self {
            root_dir,
            ignore_matcher,
        }
    }

    /// Perform a full scan of the directory
    pub fn full_scan(&self) -> Result<Vec<FileState>> {
        self.full_scan_with_previous(&[])
    }

    /// Perform a full scan, reusing hashes for files whose metadata is unchanged.
    pub fn full_scan_with_previous(&self, previous_states: &[FileState]) -> Result<Vec<FileState>> {
        let mut states = Vec::new();
        let previous_map: HashMap<&str, &FileState> = previous_states
            .iter()
            .map(|state| (state.path.as_str(), state))
            .collect();

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
            let relative_path = match path.strip_prefix(&self.root_dir) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if self.ignore_matcher.should_ignore(relative_path) {
                trace!("Ignoring file: {}", relative_path.display());
                continue;
            }

            let relative_path_str = relative_path.to_string_lossy();

            // Get file metadata and hash
            match self.scan_file_with_previous(
                path,
                previous_map.get(relative_path_str.as_ref()).copied(),
            ) {
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
        self.scan_file_with_previous(path, None)
    }

    fn scan_file_with_previous(
        &self,
        path: &Path,
        previous: Option<&FileState>,
    ) -> Result<FileState> {
        let metadata = std::fs::metadata(path)?;
        let relative_path = path.strip_prefix(&self.root_dir)?.to_path_buf();
        let path_str = relative_path.to_string_lossy().to_string();
        let modified_at = metadata.modified()?.into();
        let size = metadata.len();

        let hash = match previous {
            Some(previous) if previous.size == size && previous.modified_at == modified_at => {
                previous.hash.clone()
            }
            _ => compute_file_hash(path)?,
        };

        Ok(FileState {
            path: path_str,
            hash,
            size,
            modified_at,
            couch_rev: previous.and_then(|state| state.couch_rev.clone()),
            last_sync_at: previous
                .map(|state| state.last_sync_at)
                .unwrap_or_else(Utc::now),
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
        let full_path = self.root_dir.join(relative_path);

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
    let mut buffer = [0_u8; 8192];

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
    use chrono::Duration;
    use std::io::Write;
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

    #[test]
    fn test_full_scan_with_previous_reuses_hash_for_unchanged_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "content").unwrap();

        let scanner = Scanner::new(temp_dir.path().to_path_buf(), IgnoreMatcher::empty());
        let initial = scanner.full_scan().unwrap();
        let reused = scanner.full_scan_with_previous(&initial).unwrap();

        assert_eq!(reused.len(), 1);
        assert_eq!(reused[0].hash, initial[0].hash);
        assert_eq!(reused[0].couch_rev, initial[0].couch_rev);
        assert_eq!(reused[0].last_sync_at, initial[0].last_sync_at);
    }

    #[test]
    fn test_full_scan_with_previous_rehashes_modified_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "content").unwrap();

        let scanner = Scanner::new(temp_dir.path().to_path_buf(), IgnoreMatcher::empty());
        let mut initial = scanner.full_scan().unwrap();
        initial[0].couch_rev = Some("1-test".to_string());
        initial[0].last_sync_at += Duration::seconds(5);

        std::thread::sleep(std::time::Duration::from_secs(1));
        std::fs::write(&file_path, "updated content").unwrap();

        let rescanned = scanner.full_scan_with_previous(&initial).unwrap();

        assert_eq!(rescanned.len(), 1);
        assert_ne!(rescanned[0].hash, initial[0].hash);
        assert_eq!(rescanned[0].couch_rev, initial[0].couch_rev);
        assert_eq!(rescanned[0].last_sync_at, initial[0].last_sync_at);
    }
}
