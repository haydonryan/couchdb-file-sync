use crate::models::{Change, ChangeFingerprint, FileState, IgnoreMatcher, SyncDirPath};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, trace, warn};
use walkdir::WalkDir;

/// Conservative timestamp granularity (seconds) used for the git-style
/// "racily clean" check. A file whose mtime is within this window of the scan
/// time may have been written inside the same filesystem timestamp tick, so a
/// write can leave mtime (and size) unchanged while the content differs. Such
/// files are re-hashed instead of reusing the stored hash. Chosen to cover
/// coarse-granularity filesystems (e.g. FAT-style 2s ticks) while having no
/// practical cost on unchanged trees, whose mtimes are far in the past.
const TIMESTAMP_GRANULARITY_SECS: i64 = 2;

/// True when `modified_at` falls inside the timestamp-granularity window of
/// "now" (or is in the future, e.g. from clock skew), meaning the mtime alone
/// cannot prove the content is unchanged. Mirrors git's racily-clean handling:
/// a file touched inside the current timestamp tick must be re-checked even
/// when its stored mtime and size match.
fn is_racily_clean(modified_at: DateTime<Utc>) -> bool {
    // A negative delta (mtime in the future) is also within the window because
    // `now - modified_at <= granularity` holds for negative values.
    Utc::now().signed_duration_since(modified_at)
        <= chrono::Duration::seconds(TIMESTAMP_GRANULARITY_SECS)
}

/// Result of a full filesystem scan: the discovered file states plus a flag
/// recording whether every subtree and file was actually read.
///
/// `complete` is `false` whenever the directory walk or any per-file
/// metadata/hash read failed (permission or I/O error) or the sync root is
/// missing/unmounted. When it is `false`, absence of a stored path from
/// `states` does NOT prove the file was deleted — it may simply be hidden in
/// an unreadable subtree — so callers must not infer deletions from the scan.
#[derive(Debug, Clone)]
pub struct ScanOutcome {
    /// The file states discovered by the scan, in walk order.
    pub states: Vec<FileState>,
    /// `true` only when the walk and every per-file read succeeded.
    pub complete: bool,
}

/// Scans the filesystem for changes
#[derive(Clone)]
pub struct Scanner {
    root_dir: SyncDirPath,
    ignore_matcher: IgnoreMatcher,
    /// Number of full file re-reads / SHA-256 computations issued by
    /// [`Self::scan_file_with_stored`] (test-only). Shared across clones via
    /// `Arc` so the count is visible on the scanner that drives a
    /// [`Self::full_scan_with_stored`] call even though the blocking scan runs
    /// on a cloned scanner. Lets tests assert unchanged scans reuse stored
    /// hashes instead of re-hashing every file.
    #[cfg(test)]
    hash_computations: Arc<std::sync::atomic::AtomicU64>,
}

impl Scanner {
    /// Create a new scanner for the given root directory
    #[must_use]
    #[cfg_attr(not(test), allow(clippy::missing_const_for_fn))]
    pub fn new(root_dir: SyncDirPath, ignore_matcher: IgnoreMatcher) -> Self {
        Self {
            root_dir,
            ignore_matcher,
            #[cfg(test)]
            hash_computations: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Perform a full scan of the directory.
    ///
    /// The blocking walkdir/hash work runs on tokio's blocking thread pool via
    /// [`tokio::task::spawn_blocking`] so the async executor is never stalled by
    /// the scan.
    ///
    /// # Errors
    ///
    /// Returns an error if the blocking scan task fails.
    pub async fn full_scan(&self) -> Result<ScanOutcome> {
        self.full_scan_with_stored(Arc::new(Vec::new())).await
    }

    /// Perform a full scan, reusing the stored hash for files whose stored
    /// state has the same mtime AND size as the on-disk file, is not racily
    /// clean, and whose reliable change fingerprint matches (Unix `ctime`; see
    /// [`Self::scan_file_with_stored`]). On supported platforms an unchanged
    /// tree — including one whose files were just synced and therefore have an
    /// mtime older than their `last_sync_at` — skips the full disk read +
    /// SHA-256 for every file. Platforms without change evidence fall back to
    /// the conservative `last_sync_at` guard. New files, files with a differing
    /// mtime, size, or fingerprint, racily-clean files, and files an mtime
    /// older than the stored last sync (no fingerprint platforms) are hashed
    /// as usual, so created/modified/deleted detection semantics are unchanged.
    ///
    /// `stored_states` is shared with the blocking task through an [`Arc`] so
    /// the (potentially large) state table is never copied.
    ///
    /// # Errors
    ///
    /// Returns an error if the blocking scan task fails.
    pub async fn full_scan_with_stored(
        &self,
        stored_states: Arc<Vec<FileState>>,
    ) -> Result<ScanOutcome> {
        let scanner = self.clone();
        Ok(tokio::task::spawn_blocking(move || scanner.scan_blocking(&stored_states)).await?)
    }

    /// Walk the directory tree and hash every file using blocking filesystem
    /// calls. Called on a blocking thread pool by [`Self::full_scan`].
    ///
    /// Any walk error (including an unreadable subtree or a missing/unmounted
    /// sync root) or any per-file metadata/hash read failure marks the result
    /// incomplete, so callers know absence from the returned states cannot be
    /// used to infer deletions.
    fn scan_blocking(&self, stored_states: &[FileState]) -> ScanOutcome {
        let mut states = Vec::new();
        let mut complete = true;
        // Index stored states by relative path for O(1) reuse lookups. Keys are
        // borrowed, so no path/hash strings are copied.
        let stored_map: HashMap<&str, &FileState> =
            stored_states.iter().map(|s| (s.path.as_str(), s)).collect();

        for entry in WalkDir::new(&self.root_dir).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    warn!("Error walking directory: {}; marking scan incomplete", e);
                    complete = false;
                    continue;
                }
            };

            let path = entry.path();

            // Skip directories and non-files
            if !entry.file_type().is_file() {
                continue;
            }

            // Check ignore patterns
            let Ok(relative_path) = path.strip_prefix(self.root_dir.as_path()) else {
                continue;
            };

            // Reject relative paths that cannot be represented losslessly as
            // UTF-8 instead of converting them with `to_string_lossy`. A lossy
            // conversion maps an invalid byte to U+FFFD, aliasing this file
            // with a *distinct* file whose real name is the replacement
            // character: the wrong stored hash could be reused (uploading the
            // wrong bytes) or the file could look absent and become a deletion
            // candidate. Skip such files and mark the scan incomplete so
            // callers never infer a deletion from their absence.
            let Some(path_str) = relative_path.to_str() else {
                warn!(
                    "Skipping file with non-UTF-8 relative path {:?}: cannot represent it losslessly; marking scan incomplete",
                    relative_path
                );
                complete = false;
                continue;
            };

            if self.ignore_matcher.should_ignore(relative_path) {
                trace!("Ignoring file: {}", relative_path.display());
                continue;
            }

            // Get file metadata and hash, reusing the stored hash when the
            // mtime+size match is safe.
            let stored = stored_map.get(path_str).copied();
            match self.scan_file_with_stored(path, stored) {
                Ok(state) => {
                    debug!("Scanned file: {} (hash: {})", state.path, &state.hash[..8]);
                    states.push(state);
                }
                Err(e) => {
                    warn!(
                        "Failed to scan file {}: {}; marking scan incomplete",
                        path.display(),
                        e
                    );
                    complete = false;
                }
            }
        }

        ScanOutcome { states, complete }
    }

    /// Scan a single file, always computing the SHA-256 hash.
    ///
    /// Used by the watcher quick-scan path ([`Self::scan_single`]) where no
    /// stored-state shortcut applies; behavior is identical to
    /// [`Self::scan_file_with_stored`] with no stored state.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or hashed.
    pub fn scan_file(&self, path: &Path) -> Result<FileState> {
        self.scan_file_with_stored(path, None)
    }

    /// Scan a single file, reusing the stored hash when the stored state's
    /// mtime AND size match the on-disk file, the file is not racily clean,
    /// and reliable change evidence confirms the content is unchanged.
    ///
    /// On platforms with a change fingerprint (Unix `ctime`), the stored
    /// fingerprint is compared against the on-disk fingerprint: a match proves
    /// no content write happened since the last scan (the kernel bumps `ctime`
    /// on any write, and userspace cannot set it), so the stored hash is
    /// reused. This closes the mtime-preserving edit hole (`cp -p`, `rsync -t`,
    /// `git checkout`, `touch -t` can rewrite content while leaving an old
    /// mtime and identical size) while still letting genuinely-unchanged
    /// synced files reuse their hash even though their mtime predates
    /// `last_sync_at`.
    ///
    /// On platforms without such evidence (no `ctime` equivalent), the stored
    /// hash is reused only when the on-disk mtime is at least as new as the
    /// stored `last_sync_at`; an mtime older than the last sync cannot prove
    /// the content is unchanged, so the file is re-hashed (conservative
    /// fallback that always preserves correctness).
    ///
    /// `stored` is the previously recorded state for this file, if any. When it
    /// is `None` (or the shortcut does not apply) the SHA-256 hash is computed
    /// from the file content, preserving existing detection behavior.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be resolved within the scan root or
    /// the file cannot be read and hashed.
    pub fn scan_file_with_stored(
        &self,
        path: &Path,
        stored: Option<&FileState>,
    ) -> Result<FileState> {
        let metadata = std::fs::metadata(path)?;

        // Try the lexical root strip_prefix first. In production, scan_blocking
        // walks the already-canonical SyncDirPath root, so this textual prefix
        // comparison succeeds and we avoid the redundant path.canonicalize()
        // realpath syscall on every scanned file. Only when the lexical
        // strip_prefix fails (e.g. a caller hands scan_file a non-canonical
        // path through a symlinked root, as in #2941) do we canonicalize and
        // re-derive the residual path with the canonical-with-lexical fallback
        // below.
        let (resolved_path, relative_path): (Cow<'_, Path>, Cow<'_, Path>) =
            if let Ok(rel) = path.strip_prefix(self.root_dir.as_path()) {
                (Cow::Borrowed(path), Cow::Borrowed(rel))
            } else {
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
                let relative = resolved.strip_prefix(self.root_dir.as_path())?;
                let relative_buf = relative.to_path_buf();
                (Cow::Owned(resolved), Cow::Owned(relative_buf))
            };
        // Reject non-UTF-8 relative paths (rather than lossily converting
        // them) so a direct single-file scan cannot alias this file with a
        // distinct replacement-character name or upload the wrong bytes. The
        // full-scan path in [`Self::scan_blocking`] already skips such files
        // and marks the scan incomplete; this guard covers direct callers such
        // as the watcher quick-scan.
        let path_str = match relative_path.to_str() {
            Some(s) => s.to_string(),
            None => {
                return Err(anyhow::anyhow!(
                    "cannot represent relative path {} losslessly: not valid UTF-8",
                    relative_path.display()
                ));
            }
        };

        // Get modification time, size, and change fingerprint from the same
        // metadata the hash shortcut decision uses.
        let modified_at = metadata.modified()?.into();
        let size = metadata.len();
        let fingerprint = ChangeFingerprint::from_metadata(&metadata);

        // Conservative mtime+size shortcut (git-style): reuse the stored hash
        // when the stored state has the same mtime AND size as the on-disk file
        // and the file is not racily clean. Unchanged-content proof then comes
        // from reliable change evidence: when a change fingerprint is available
        // (Unix ctime), a stored-vs-on-disk fingerprint match proves no content
        // write happened since the last scan — even for a synced file whose
        // mtime predates `last_sync_at` (the previous guard that defeated the
        // cache on real synced files). When no fingerprint is available, fall
        // back to the conservative `last_sync_at` guard, which re-hashes any
        // file whose mtime is older than its stored last sync (the
        // mtime-preserving edit hole) at the cost of re-hashing every unchanged
        // synced file. A one-sided fingerprint (stored but unavailable now, or
        // vice versa) cannot prove the content is unchanged, so it re-hashes.
        // Otherwise (new file, size or mtime drift, a recently-touched
        // racily-clean file, an mtime older than the stored last sync, or no
        // matching change evidence) compute the SHA-256 hash as before, so
        // change detection stays correct.
        let reuse_hash = match stored {
            Some(s)
                if s.size == size
                    && s.modified_at == modified_at
                    && !is_racily_clean(modified_at)
                    && match (s.change_fingerprint, fingerprint) {
                        (Some(stored_fp), Some(disk_fp)) => stored_fp == disk_fp,
                        // No reliable change evidence on this platform: use the
                        // conservative last-sync guard as the fallback.
                        (None, None) => modified_at >= s.last_sync_at,
                        // One-sided evidence cannot prove the content unchanged.
                        _ => false,
                    } =>
            {
                Some(s.hash.clone())
            }
            _ => None,
        };

        let hash = if let Some(hash) = reuse_hash {
            hash
        } else {
            #[cfg(test)]
            self.hash_computations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Compute the hash from the same resolved path as the
            // relative-path derivation so hashing and residual derivation
            // stay consistent.
            compute_file_hash(resolved_path.as_ref())?
        };

        Ok(FileState {
            path: path_str,
            hash,
            size,
            modified_at,
            couch_rev: None,
            last_sync_at: Utc::now(),
            change_fingerprint: fingerprint,
        })
    }

    /// Number of file hashes computed by this scanner since it was created
    /// (test-only).
    #[cfg(test)]
    #[must_use]
    pub fn hash_computations(&self) -> u64 {
        self.hash_computations
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Detect changes by comparing current state with stored state.
    ///
    /// `scan_complete` records whether `current_states` came from a complete
    /// scan ([`ScanOutcome::complete`]). When it is `false`, a stored path
    /// absent from `current_states` may simply be hidden in an unreadable
    /// subtree, so no deletion is inferred from its absence.
    pub fn detect_changes(
        &self,
        current_states: &[FileState],
        stored_states: &[FileState],
        scan_complete: bool,
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

        // Detect deleted files.
        for stored in stored_states {
            if current_map.contains_key(&stored.path) {
                continue;
            }
            // A stored path absent from the scan could mean it was truly
            // deleted, OR that the scan could not read it (permission or I/O
            // error, an unreadable/missing ancestor subtree, an absent or
            // unmounted sync root, or a race with a concurrent delete/rename).
            // Reporting a delete for a file that is still present would destroy
            // its remote copy, so absence is only treated as a deletion when
            // the scan was complete AND the file is genuinely gone from the
            // filesystem.
            if !scan_complete {
                debug!("Scan incomplete; not inferring deletion of {}", stored.path);
                continue;
            }
            let full_path = self.root_dir.as_path().join(&stored.path);
            match std::fs::metadata(&full_path) {
                Ok(_) => {
                    // Still present on disk (e.g. it exists but could not be
                    // hashed, or a race recreated it); not a deletion.
                    debug!(
                        "File {} exists on disk but could not be scanned; not reporting a delete",
                        stored.path
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Genuinely gone: the metadata probe confirms absence
                    // rather than an inaccessible path.
                    changes.push(Change::local_deleted(stored.path.clone()));
                }
                Err(e) => {
                    // Permission denied or another I/O error: absence cannot be
                    // distinguished from inaccessibility, so do not tombstone.
                    debug!(
                        "Cannot verify absence of {} ({}); not reporting a delete",
                        stored.path, e
                    );
                }
            }
        }

        changes
    }

    /// Quick scan for a single file by relative path, returning `None` if it
    /// does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or hashed.
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
/// Compute the SHA-256 hash of a file, returning it as a lowercase hex string.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read.
#[allow(clippy::large_stack_arrays)]
pub fn compute_file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
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
#[must_use]
pub fn compute_bytes_hash(bytes: &[u8]) -> String {
    let result = Sha256::digest(bytes);
    hex::encode(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::IgnoreMatcher;
    use crate::models::change::{ChangeSource, ChangeType};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    /// Set a file's mtime deterministically so tests can place files inside or
    /// outside the racily-clean timestamp-granularity window.
    fn set_mtime(path: &Path, time: SystemTime) {
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time))
            .expect("set file mtime");
    }

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

    // ---- stored-hash mtime+size shortcut tests (#3005) ----

    /// An unchanged file (same mtime AND size, not racily clean) must reuse the
    /// stored hash and produce no local change. The stored hash is a marker
    /// value that would only appear if the file was NOT re-read.
    #[test]
    fn test_scan_file_reuses_stored_hash_when_mtime_and_size_match() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("keep.txt");
        std::fs::write(&file_path, b"stable content").unwrap();

        // Place the mtime far enough in the past that the file is not racily
        // clean, so the mtime+size shortcut is eligible.
        let old_mtime = SystemTime::now() - Duration::from_hours(1);
        set_mtime(&file_path, old_mtime);

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        // Build the stored state from the file's actual reported metadata so
        // the mtime equality comparison is exact on any filesystem precision.
        let meta = std::fs::metadata(&file_path).unwrap();
        let mut stored = FileState::new(
            "keep.txt".to_string(),
            "a".repeat(64), // marker hash: only present if reused
            meta.len(),
            meta.modified().unwrap().into(),
        );
        // Capture the change fingerprint (Unix ctime) from the current on-disk
        // metadata, so the unchanged file's fingerprint matches on the next
        // scan. The file mtime is an hour in the past; give the stored state a
        // last_sync_at that predates it so the conservative non-fingerprint
        // fallback (used on platforms without a ctime) stays eligible too.
        stored.change_fingerprint = ChangeFingerprint::from_metadata(&meta);
        stored.last_sync_at = Utc::now() - Duration::from_hours(2);

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        // The marker hash proves the stored hash was reused, not recomputed.
        assert_eq!(state.hash, stored.hash, "stored hash must be reused");
        assert_eq!(state.size, stored.size);
        assert_eq!(state.modified_at, stored.modified_at);

        // Change detection against the stored state reports no change.
        let changes = scanner.detect_changes(&[state], &[stored], true);
        assert!(changes.is_empty(), "unchanged file must produce no changes");
    }

    /// A content change that also changes the mtime (same size) must be
    /// detected: the mtime differs, so the hash is recomputed.
    #[test]
    fn test_scan_file_recomputes_hash_when_mtime_differs() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("edit.txt");
        std::fs::write(&file_path, b"AAAA").unwrap();
        let old_mtime = SystemTime::now() - Duration::from_hours(1);
        set_mtime(&file_path, old_mtime);

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let meta = std::fs::metadata(&file_path).unwrap();
        let stored = FileState::new(
            "edit.txt".to_string(),
            "a".repeat(64),
            meta.len(),
            meta.modified().unwrap().into(),
        );

        // Change content but keep the size; bump the mtime.
        std::fs::write(&file_path, b"BBBB").unwrap();
        set_mtime(&file_path, old_mtime + Duration::from_secs(10));

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        assert_eq!(state.size, stored.size);
        assert_ne!(state.modified_at, stored.modified_at);
        assert_ne!(state.hash, stored.hash, "hash must be recomputed");
        assert_eq!(state.hash, compute_file_hash(&file_path).unwrap());

        let changes = scanner.detect_changes(&[state], &[stored], true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "edit.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Modified);
    }

    /// A content change that also changes the size (same mtime) must be
    /// detected: the size differs, so the hash is recomputed.
    #[test]
    fn test_scan_file_recomputes_hash_when_size_differs() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("edit.txt");
        std::fs::write(&file_path, b"AAAA").unwrap();
        let old_mtime = SystemTime::now() - Duration::from_hours(1);
        set_mtime(&file_path, old_mtime);

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let meta = std::fs::metadata(&file_path).unwrap();
        let stored = FileState::new(
            "edit.txt".to_string(),
            "a".repeat(64),
            meta.len(),
            meta.modified().unwrap().into(),
        );

        // Change content and size; restore the exact same mtime.
        std::fs::write(&file_path, b"AAAAA").unwrap();
        set_mtime(&file_path, old_mtime);

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        assert_eq!(state.modified_at, stored.modified_at);
        assert_ne!(state.size, stored.size);
        assert_ne!(state.hash, stored.hash, "hash must be recomputed");
        assert_eq!(state.hash, compute_file_hash(&file_path).unwrap());

        let changes = scanner.detect_changes(&[state], &[stored], true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type(), ChangeType::Modified);
    }

    /// Regression test for the racily-clean edge: a content change that keeps
    /// both the mtime and size identical (possible inside the filesystem
    /// timestamp-granularity window) must still be detected. Because the file
    /// is racily clean, the stored hash is NOT reused and the new content is
    /// hashed.
    #[test]
    fn test_scan_file_rehashes_racily_clean_file_with_same_mtime_and_size() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("racy.txt");

        // Seed the file and give it an mtime inside the racily-clean window
        // (within TIMESTAMP_GRANULARITY_SECS of "now").
        std::fs::write(&file_path, b"AAAA").unwrap();
        let racy_mtime = SystemTime::now() - Duration::from_millis(50);
        set_mtime(&file_path, racy_mtime);

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let meta = std::fs::metadata(&file_path).unwrap();
        let stored = FileState::new(
            "racy.txt".to_string(),
            "a".repeat(64),
            meta.len(),
            meta.modified().unwrap().into(),
        );

        // Overwrite with different content but the SAME size, then restore the
        // exact same mtime so both mtime and size match the stored state.
        std::fs::write(&file_path, b"BBBB").unwrap();
        set_mtime(&file_path, racy_mtime);

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        // mtime and size match the stored state...
        assert_eq!(state.modified_at, stored.modified_at);
        assert_eq!(state.size, stored.size);
        // ...but the file is racily clean, so the hash must be recomputed and
        // the content change detected.
        assert_ne!(
            state.hash, stored.hash,
            "racily-clean file with matching mtime+size must still be re-hashed"
        );
        assert_eq!(state.hash, compute_file_hash(&file_path).unwrap());

        let changes = scanner.detect_changes(&[state], &[stored], true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "racy.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Modified);
    }

    /// Regression test (#3628): a content change that preserves both the past
    /// mtime and the same size must still be detected. Mtime-preserving tools
    /// (`cp -p`, `rsync -t`, `git checkout`, `touch -t`) can rewrite a file
    /// while leaving an old mtime and identical size, so when the on-disk mtime
    /// is older than the stored `last_sync_at` the stored hash is NOT reused and
    /// the new content is hashed.
    #[test]
    fn test_scan_file_rehashes_when_preserved_past_mtime_predates_last_sync() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("preserved.txt");

        // Seed the file with an mtime in the past (outside the racily-clean
        // window).
        std::fs::write(&file_path, b"AAAA").unwrap();
        let past_mtime = SystemTime::now() - Duration::from_hours(1);
        set_mtime(&file_path, past_mtime);

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let meta = std::fs::metadata(&file_path).unwrap();
        // Record the file's change fingerprint (Unix ctime) as of the original
        // content, so the mtime-preserving rewrite below is detected by the
        // ctime bump. `FileState::new` also records a RECENT last_sync_at (now)
        // while the file mtime is in the past, so on platforms without a
        // fingerprint the conservative fallback (mtime older than last_sync)
        // detects the rewrite the same way.
        let mut stored = FileState::new(
            "preserved.txt".to_string(),
            "a".repeat(64), // marker hash: only present if reused
            meta.len(),
            meta.modified().unwrap().into(),
        );
        stored.change_fingerprint = ChangeFingerprint::from_metadata(&meta);

        // Overwrite with different content but the SAME size, then restore the
        // exact same past mtime, as a mtime-preserving tool would.
        std::fs::write(&file_path, b"BBBB").unwrap();
        set_mtime(&file_path, past_mtime);

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        // mtime and size still match the stored state...
        assert_eq!(state.modified_at, stored.modified_at);
        assert_eq!(state.size, stored.size);
        // ...but the rewrite bumped the change fingerprint (Unix ctime), so the
        // hash must be recomputed and the content change detected. (On
        // platforms without a fingerprint, the mtime being older than the
        // stored last_sync_at triggers the conservative fallback instead.)
        assert_ne!(
            state.hash, stored.hash,
            "preserved past mtime + same size must still be re-hashed"
        );
        assert_eq!(state.hash, compute_file_hash(&file_path).unwrap());

        let changes = scanner.detect_changes(&[state], &[stored], true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "preserved.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Modified);
    }

    /// `full_scan_with_stored` over an unchanged tree must reuse every stored
    /// hash (zero file re-reads) while producing the same states as `full_scan`.
    #[tokio::test]
    async fn test_full_scan_with_stored_reuses_all_hashes_on_unchanged_tree() {
        let temp_dir = TempDir::new().unwrap();
        let root = SyncDirPath::new(temp_dir.path()).unwrap();
        let old_mtime = SystemTime::now() - Duration::from_hours(1);
        for (i, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
            let file_path = root.as_path().join(name);
            std::fs::write(&file_path, format!("content {i}")).unwrap();
            set_mtime(&file_path, old_mtime);
        }

        let scanner = Scanner::new(root.clone(), IgnoreMatcher::empty());

        // Establish the stored states (this first scan hashes everything and
        // records each file's change fingerprint + last_sync_at = scan time).
        let stored_vec = scanner.full_scan().await.unwrap().states;
        let hashes_after_first = scanner.hash_computations();
        assert_eq!(hashes_after_first, 3, "first scan must hash all 3 files");

        // A real post-sync scan faces exactly this state: each file's mtime is
        // in the past, older than its last_sync_at (which the scan stamped at
        // the sync time). No artificial timestamp shift is applied. On Unix the
        // unchanged ctime fingerprint is reliable change evidence, so the
        // second scan reuses every stored hash. On platforms without a
        // fingerprint the conservative fallback re-hashes every file (mtime
        // older than last_sync_at), preserving correctness without a cache hit.
        let stored_states = Arc::new(stored_vec);

        // A second scan against the stored states must reuse all hashes.
        let current = scanner
            .full_scan_with_stored(stored_states.clone())
            .await
            .unwrap();
        let hashes_after_second = scanner.hash_computations();
        #[cfg(unix)]
        assert_eq!(
            hashes_after_second, hashes_after_first,
            "unchanged scan must not re-hash any file on Unix (ctime fingerprint)"
        );
        #[cfg(not(unix))]
        assert_eq!(
            hashes_after_second,
            hashes_after_first + 3,
            "no change evidence: conservative fallback re-hashes files whose mtime predates last_sync_at"
        );
        assert_eq!(current.states.len(), 3);
        assert!(current.complete, "unchanged scan must be complete");
        for state in &current.states {
            let stored = stored_states
                .iter()
                .find(|s| s.path == state.path)
                .expect("stored state exists");
            assert_eq!(state.hash, stored.hash);
            assert_eq!(state.size, stored.size);
            assert_eq!(state.modified_at, stored.modified_at);
        }
        // And change detection sees no changes.
        let changes = scanner.detect_changes(&current.states, &stored_states, current.complete);
        assert!(changes.is_empty());
    }

    /// A real synced file has an mtime older than its `last_sync_at` (the sync
    /// records `last_sync_at = now`, later than the file's preserved mtime).
    /// On Unix the unchanged ctime fingerprint proves the content is unchanged,
    /// so the stored hash is reused without re-reading the file — the cache is
    /// effective on real synced files, not just on artificially backdated
    /// timestamps.
    #[cfg(unix)]
    #[test]
    fn test_scan_file_reuses_stored_hash_on_real_synced_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("synced.txt");
        std::fs::write(&file_path, b"stable content").unwrap();
        let old_mtime = SystemTime::now() - Duration::from_hours(1);
        set_mtime(&file_path, old_mtime);

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let meta = std::fs::metadata(&file_path).unwrap();
        // Stored state exactly as a real sync records it: mtime in the past,
        // but `last_sync_at` left at its default (now — NEWER than the mtime),
        // plus the change fingerprint captured at sync time.
        let mut stored = FileState::new(
            "synced.txt".to_string(),
            "a".repeat(64), // marker hash: only present if reused
            meta.len(),
            meta.modified().unwrap().into(),
        );
        stored.change_fingerprint = ChangeFingerprint::from_metadata(&meta);

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        // The marker hash proves the stored hash was reused even though the
        // mtime is older than last_sync_at (the ctime fingerprint matched).
        assert_eq!(state.hash, stored.hash, "synced file hash must be reused");
        let changes = scanner.detect_changes(&[state], &[stored], true);
        assert!(
            changes.is_empty(),
            "unchanged synced file must produce no changes"
        );
    }

    /// A file whose mtime is in the future (clock skew) or inside the current
    /// timestamp tick cannot be proven unchanged by mtime alone: it is treated
    /// as racily clean and re-hashed, even when its size and stored change
    /// fingerprint would otherwise match. This mirrors git's racily-clean
    /// handling for coarse-granularity filesystems and future-mtime skew.
    #[test]
    fn test_scan_file_rehashes_future_or_coarse_mtime() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("future.txt");
        std::fs::write(&file_path, b"AAAA").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let meta = std::fs::metadata(&file_path).unwrap();
        let mut stored = FileState::new(
            "future.txt".to_string(),
            "a".repeat(64),
            meta.len(),
            meta.modified().unwrap().into(),
        );
        stored.change_fingerprint = ChangeFingerprint::from_metadata(&meta);
        // Place last_sync_at before the (future) mtime so the non-fingerprint
        // fallback would not itself force a re-hash; the future mtime must
        // still trigger one via the racily-clean check.
        stored.last_sync_at = Utc::now() - Duration::from_hours(1);

        // Move the file mtime into the future (clock skew / coarse tick).
        set_mtime(&file_path, SystemTime::now() + Duration::from_secs(10));

        let state = scanner
            .scan_file_with_stored(&file_path, Some(&stored))
            .unwrap();

        // mtime and size match the stored state, but the future mtime is
        // racily clean, so the hash must be recomputed.
        assert_eq!(state.size, stored.size);
        assert_ne!(
            state.hash, stored.hash,
            "future/coarse mtime must still be re-hashed"
        );
        assert_eq!(state.hash, compute_file_hash(&file_path).unwrap());
    }

    // ---- scan_file tests ----

    #[test]
    fn test_scan_file_returns_correct_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("hello.txt");
        std::fs::write(&file_path, b"Hello, world!").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
            SyncDirPath::new(base.path()).unwrap(),
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap().states;

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
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap().states;

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
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap().states;

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
        let scanner = Scanner::new(SyncDirPath::new(temp_dir.path()).unwrap(), matcher);
        let states = scanner.full_scan().await.unwrap().states;

        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"keep.txt"));
        assert!(paths.contains(&"also_keep.rs"));
        assert!(!paths.contains(&"ignore.log"));
    }

    /// On Unix, a filename with an invalid UTF-8 byte must be rejected (skipped
    /// and the scan marked incomplete) rather than lossily aliased with a real
    /// replacement-character file. The real U+FFFD name must survive
    /// unmodified, the invalid file must never be uploaded with the wrong
    /// content, the skipped file must never become a deletion candidate, and
    /// ordinary Unicode must still round-trip exactly.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_full_scan_rejects_non_utf8_without_aliasing_replacement_char() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let temp_dir = TempDir::new().unwrap();
        // A real file whose name is the U+FFFD replacement character (valid UTF-8).
        let ff_name = "\u{FFFD}.txt";
        std::fs::write(temp_dir.path().join(ff_name), b"REAL-REPLACEMENT").unwrap();
        // A distinct file whose name contains an invalid UTF-8 byte (0xFF).
        let invalid_name = OsString::from_vec(b"a\xFF.txt".to_vec());
        let invalid_path = temp_dir.path().join(&invalid_name);
        std::fs::write(&invalid_path, b"INVALID-BYTES").unwrap();
        // Ordinary Unicode must still round-trip exactly.
        std::fs::write(temp_dir.path().join("héllo.txt"), b"unicode").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let outcome = scanner.full_scan().await.unwrap();

        // The invalid-UTF-8 file must not be present, so it must not be aliased
        // to the replacement-character path (which would upload wrong bytes).
        assert!(
            !outcome.states.iter().any(|s| s.path == "a\u{FFFD}.txt"),
            "invalid-UTF-8 file must not be aliased to the replacement-character path"
        );

        // Exactly one state carries the real replacement-character name, and it
        // points at the real file's content, never the invalid file's bytes.
        let ff_states: Vec<&FileState> = outcome
            .states
            .iter()
            .filter(|s| s.path == ff_name)
            .collect();
        assert_eq!(
            ff_states.len(),
            1,
            "replacement-character name must not be merged with another file"
        );
        assert_eq!(ff_states[0].hash, compute_bytes_hash(b"REAL-REPLACEMENT"));

        // Ordinary Unicode still round-trips exactly.
        let unicode_states: Vec<&FileState> = outcome
            .states
            .iter()
            .filter(|s| s.path == "héllo.txt")
            .collect();
        assert_eq!(unicode_states.len(), 1);
        assert_eq!(unicode_states[0].hash, compute_bytes_hash(b"unicode"));

        // The scan is incomplete, so callers never infer a deletion for the
        // skipped invalid-UTF-8 file.
        assert!(
            !outcome.complete,
            "a non-UTF-8 file must mark the scan incomplete"
        );

        // Direct single-file scan also rejects the invalid-UTF-8 path.
        assert!(
            scanner.scan_file(&invalid_path).is_err(),
            "direct scan of a non-UTF-8 path must error"
        );
    }

    #[tokio::test]
    async fn test_full_scan_skips_dotfiles() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("visible.txt"), b"seen").unwrap();
        std::fs::write(temp_dir.path().join(".hidden"), b"hidden").unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap().states;

        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"visible.txt"));
        assert!(!paths.contains(&".hidden"));
    }

    #[tokio::test]
    async fn test_full_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let states = scanner.full_scan().await.unwrap().states;

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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
        let scanner = Scanner::new(SyncDirPath::new(temp_dir.path()).unwrap(), matcher);
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let state = FileState::new("file.txt".to_string(), "abc".to_string(), 10, Utc::now());

        let current = vec![state.clone()];
        let stored = vec![state];

        let changes = scanner.detect_changes(&current, &stored, true);
        assert!(changes.is_empty(), "No changes expected when states match");
    }

    #[test]
    fn test_detect_changes_new_file_created() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let current = vec![FileState::new(
            "new.txt".to_string(),
            "hash1".to_string(),
            42,
            Utc::now(),
        )];
        let stored: Vec<FileState> = vec![];

        let changes = scanner.detect_changes(&current, &stored, true);
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
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

        let changes = scanner.detect_changes(&current, &stored, true);
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
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let stored = vec![FileState::new(
            "gone.txt".to_string(),
            "hash".to_string(),
            10,
            Utc::now(),
        )];
        let current: Vec<FileState> = vec![];

        let changes = scanner.detect_changes(&current, &stored, true);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path(), "gone.txt");
        assert_eq!(changes[0].change_type(), ChangeType::Deleted);
        assert_eq!(changes[0].source(), ChangeSource::Local);
    }

    #[test]
    fn test_detect_changes_present_but_unscanned_file_is_not_deleted() {
        // A stored file absent from the current scan could mean it exists on
        // disk but failed to scan (permission/I-O error). It must not be
        // reported as a delete, or its remote copy would be destroyed.
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("unreadable.txt"), b"data").unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let stored = vec![FileState::new(
            "unreadable.txt".to_string(),
            "hash".to_string(),
            4,
            Utc::now(),
        )];
        // The file is missing from `current` (as when its scan errored).
        let current: Vec<FileState> = vec![];

        let changes = scanner.detect_changes(&current, &stored, true);
        assert!(
            changes.is_empty(),
            "present-but-unscanned file must not be reported deleted"
        );
    }

    #[test]
    fn test_detect_changes_mixed_changes() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
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

        let changes = scanner.detect_changes(&current, &stored, true);
        assert_eq!(changes.len(), 3);
        let change_paths: Vec<&str> = changes
            .iter()
            .map(crate::models::change::Change::path)
            .collect();
        assert!(change_paths.contains(&"create.txt"));
        assert!(change_paths.contains(&"modify.txt"));
        assert!(change_paths.contains(&"delete.txt"));
    }

    /// Regression test for #6341: an incomplete scan (any unreadable subtree,
    /// walk error, or missing/unmounted sync root) must NEVER infer a deletion
    /// from a stored path's absence, even when the file is genuinely gone.
    /// Otherwise an unreadable ancestor would tombstone tracked remote files.
    #[test]
    fn test_detect_changes_incomplete_scan_never_infers_deletion() {
        let temp_dir = TempDir::new().unwrap();
        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );

        let stored = vec![FileState::new(
            "gone.txt".to_string(),
            "hash".to_string(),
            10,
            Utc::now(),
        )];
        // The file does not exist on disk AND the scan was incomplete. Absence
        // may still be an artifact of the incomplete scan, so no deletion.
        let current: Vec<FileState> = vec![];
        let changes = scanner.detect_changes(&current, &stored, false);
        assert!(
            changes.is_empty(),
            "incomplete scan must not infer a deletion for an absent stored path"
        );
    }

    /// Even within a complete scan, absence must be confirmed by a `NotFound`
    /// metadata probe. An inaccessible ancestor (permission denied) is not
    /// evidence of deletion, so the stored path must not be tombstoned.
    #[test]
    fn test_detect_changes_inaccessible_ancestor_not_deleted() {
        if is_effective_root() {
            // Root bypasses permission checks; chmod-based tests are void.
            return;
        }
        let temp_dir = TempDir::new().unwrap();
        let secret_dir = temp_dir.path().join("secret");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("data.txt"), b"data").unwrap();
        let mut perms = std::fs::metadata(&secret_dir).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&secret_dir, perms).unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let stored = vec![FileState::new(
            "secret/data.txt".to_string(),
            "hash".to_string(),
            4,
            Utc::now(),
        )];
        let current: Vec<FileState> = vec![];

        let changes = scanner.detect_changes(&current, &stored, true);
        assert!(
            changes.is_empty(),
            "inaccessible metadata must not be treated as a deletion"
        );
    }

    /// A missing/unmounted sync root must be reported as an incomplete scan so
    /// callers cannot tombstone tracked files from it.
    #[tokio::test]
    async fn test_full_scan_missing_root_is_incomplete() {
        let missing = std::env::temp_dir().join(format!(
            "couchfs-missing-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let scanner = Scanner::new(SyncDirPath::new(&missing).unwrap(), IgnoreMatcher::empty());
        let outcome = scanner.full_scan().await.unwrap();
        assert!(
            !outcome.complete,
            "a missing sync root must produce an incomplete scan"
        );
        assert_eq!(outcome.states, [] as [FileState; 0]);
    }

    /// A non-root permission failure inside a subtree must mark the whole scan
    /// incomplete, and change detection over that outcome must not infer
    /// deletions for the hidden files.
    #[tokio::test]
    async fn test_full_scan_incomplete_when_subtree_unreadable() {
        if is_effective_root() {
            // Root bypasses permission checks; chmod-based tests are void.
            return;
        }
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("visible.txt"), b"keep").unwrap();
        let locked_dir = temp_dir.path().join("locked");
        std::fs::create_dir_all(&locked_dir).unwrap();
        std::fs::write(locked_dir.join("hidden.txt"), b"secret").unwrap();
        let mut perms = std::fs::metadata(&locked_dir).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&locked_dir, perms).unwrap();

        let scanner = Scanner::new(
            SyncDirPath::new(temp_dir.path()).unwrap(),
            IgnoreMatcher::empty(),
        );
        let outcome = scanner.full_scan().await.unwrap();
        assert!(
            !outcome.complete,
            "unreadable subtree must mark the scan incomplete"
        );
        // The visible file is still discovered; only completeness is lost.
        assert!(outcome.states.iter().any(|s| s.path == "visible.txt"));

        // Change detection over the incomplete outcome must not tombstone the
        // hidden file even though it is absent from the scan.
        let stored = vec![FileState::new(
            "locked/hidden.txt".to_string(),
            "hash".to_string(),
            6,
            Utc::now(),
        )];
        let changes = scanner.detect_changes(&outcome.states, &stored, outcome.complete);
        assert!(
            !changes
                .iter()
                .any(|c| c.path() == "locked/hidden.txt" && c.change_type() == ChangeType::Deleted),
            "hidden file must not be reported deleted on an incomplete scan"
        );
    }

    fn is_effective_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
    }
}
