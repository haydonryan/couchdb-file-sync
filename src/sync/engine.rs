use crate::couchdb::CouchDb;
use crate::local::{compute_bytes_hash, compute_file_hash, LocalDb, Scanner};
use crate::models::{
    Change, ChangeType, Checkpoint, Conflict, CouchRev, DownloadCount, FileState, IgnoreMatcher,
    RemoteState, ResolutionStrategy, SyncDirPath, UploadCount,
};
use crate::sync::triage;
use anyhow::Result;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

/// The main sync engine
pub struct SyncEngine {
    couchdb: CouchDb,
    local_db: AsyncLocalDb,
    scanner: Scanner,
    root_dir: SyncDirPath,
    /// Kept for backward compatibility; delegates to scanner.
    ignore_matcher: IgnoreMatcher,
}

/// Report from a sync operation
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub uploaded: UploadCount,
    pub downloaded: DownloadCount,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub conflicts: usize,
    pub errors: Vec<String>,
}

/// Shared handle to the blocking SQLite-backed [`LocalDb`].
///
/// `rusqlite::Connection` performs synchronous disk I/O, so calling it
/// directly from an async `SyncEngine` method would stall a tokio worker
/// thread for the duration of every query/statement. This wrapper keeps the
/// database behind an `Arc<Mutex<..>>` and runs each operation on tokio's
/// blocking thread pool via `spawn_blocking`. The mutex serializes access so
/// return values, operation ordering, and sync/conflict semantics are
/// identical to the original straight-line synchronous calls.
#[derive(Clone)]
struct AsyncLocalDb {
    inner: Arc<Mutex<LocalDb>>,
}

impl AsyncLocalDb {
    fn new(local_db: LocalDb) -> Self {
        Self {
            inner: Arc::new(Mutex::new(local_db)),
        }
    }

    /// Run one blocking LocalDb operation on the blocking thread pool,
    /// holding the mutex for the full duration of the call so operations are
    /// serialized exactly as they were on the single sync executor thread.
    async fn run<T, F>(&self, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&LocalDb) -> Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let db = inner
                .lock()
                .map_err(|_| anyhow::anyhow!("LocalDb mutex poisoned"))?;
            op(&db)
        })
        .await
        .map_err(|e| anyhow::anyhow!("LocalDb blocking task panicked: {e}"))?
    }

    async fn get_all_file_states(&self) -> Result<Vec<FileState>> {
        self.run(|db| db.get_all_file_states()).await
    }

    async fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        let path = path.to_string();
        self.run(move |db| db.get_file_state(&path)).await
    }

    async fn save_file_state(&self, state: &FileState) -> Result<()> {
        let state = state.clone();
        self.run(move |db| db.save_file_state(&state)).await
    }

    /// Number of `save_file_state` writes issued so far (test-only).
    #[cfg(test)]
    async fn save_file_state_calls(&self) -> u64 {
        self.run(|db| Ok(db.save_file_state_calls()))
            .await
            .expect("LocalDb save_file_state_calls must not fail")
    }

    async fn delete_file_state(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        self.run(move |db| db.delete_file_state(&path)).await
    }

    async fn get_checkpoint(&self) -> Result<Option<Checkpoint>> {
        self.run(|db| db.get_checkpoint()).await
    }

    async fn save_checkpoint(&self, seq: &str) -> Result<()> {
        let seq = seq.to_string();
        self.run(move |db| db.save_checkpoint(&seq)).await
    }

    async fn store_conflict(&self, conflict: &Conflict) -> Result<()> {
        let conflict = conflict.clone();
        self.run(move |db| db.store_conflict(&conflict)).await
    }

    async fn delete_conflict(&self, path: &str) -> Result<()> {
        let path = path.to_string();
        self.run(move |db| db.delete_conflict(&path)).await
    }

    async fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        self.run(|db| db.get_conflicts()).await
    }

    async fn get_conflict(&self, path: &str) -> Result<Option<Conflict>> {
        let path = path.to_string();
        self.run(move |db| db.get_conflict(&path)).await
    }

    async fn reset_sync_state(&self) -> Result<()> {
        self.run(|db| db.reset_sync_state()).await
    }
}

impl SyncEngine {
    /// Create a new sync engine
    pub fn new(couchdb: CouchDb, local_db: LocalDb, root_dir: SyncDirPath) -> Self {
        let ignore_matcher = IgnoreMatcher::empty();
        let scanner = Scanner::new(root_dir.clone(), ignore_matcher.clone());
        Self {
            couchdb,
            local_db: AsyncLocalDb::new(local_db),
            scanner,
            root_dir,
            ignore_matcher,
        }
    }

    /// Create a new sync engine with ignore patterns applied to full scans.
    pub fn with_ignore(
        couchdb: CouchDb,
        local_db: LocalDb,
        root_dir: SyncDirPath,
        ignore_matcher: IgnoreMatcher,
    ) -> Self {
        let scanner = Scanner::new(root_dir.clone(), ignore_matcher.clone());
        Self {
            couchdb,
            local_db: AsyncLocalDb::new(local_db),
            scanner,
            root_dir,
            ignore_matcher,
        }
    }

    /// Perform a full sync cycle.
    pub async fn sync(&mut self) -> Result<SyncReport> {
        self.run_cycle(false).await
    }

    /// Perform a dry-run sync cycle.
    ///
    /// Walks the full sync pipeline (local scan, remote fetch, triage, and
    /// conflict detection) but skips every write: nothing is written to
    /// CouchDB, the local filesystem, or the state database. The returned
    /// `SyncReport` reflects what *would* have been uploaded, downloaded,
    /// deleted, and conflicted.
    pub async fn sync_dry_run(&mut self) -> Result<SyncReport> {
        self.run_cycle(true).await
    }

    /// Shared sync-cycle implementation.
    ///
    /// When `dry_run` is true every write operation (CouchDB writes, local
    /// filesystem writes, and state-DB saves) is skipped while the read-only
    /// triage, conflict detection, and report generation still run.
    async fn run_cycle(&mut self, dry_run: bool) -> Result<SyncReport> {
        if dry_run {
            info!("========== DRY-RUN SYNC CYCLE STARTING ==========");
        } else {
            info!("========== SYNC CYCLE STARTING ==========");
        }
        let mut report = SyncReport::default();

        // 1. Scan local changes
        let local_changes = self.scan_local_changes(dry_run).await?;
        info!("Local changes detected: {}", local_changes.len());
        for change in &local_changes {
            debug!("  [LOCAL] {} ({:?})", change.path(), change.change_type());
        }

        // 2. Get remote changes
        let (remote_changes, last_seq) = self.fetch_remote_changes().await?;
        info!("Remote files fetched: {}", remote_changes.len());
        for change in &remote_changes {
            debug!(
                "  [REMOTE] {} - rev: {:?}, mtime: {:?}",
                change.path(),
                change.rev(),
                change.mtime()
            );
        }

        // 3. Detect conflicts
        let (local_to_upload, remote_to_apply, conflicts) = self
            .detect_conflicts(&local_changes, &remote_changes, dry_run)
            .await?;

        report.conflicts = conflicts.len();

        info!("After analysis:");
        info!("  - Files to upload: {}", local_to_upload.len());
        info!("  - Files to download: {}", remote_to_apply.len());
        info!("  - Conflicts: {}", conflicts.len());

        // 4. Store conflicts (skipped in dry-run)
        for conflict in &conflicts {
            info!("CONFLICT: {}", conflict.path);
            if !dry_run {
                self.local_db.store_conflict(conflict).await?;
            }
        }

        // 5. Apply clean local changes to remote (skipped in dry-run)
        info!(
            "========== UPLOADING {} FILES ==========",
            local_to_upload.len()
        );
        for change in local_to_upload {
            debug!(
                "  Preparing to upload: {} -> {}",
                change.path(),
                self.couchdb.get_remote_path(change.path())
            );

            if matches!(change.change_type(), ChangeType::Deleted) {
                report.deleted_remote += 1;
                if !dry_run {
                    match self.apply_to_couchdb(&change).await {
                        Ok(_) => self.local_db.delete_file_state(change.path()).await?,
                        Err(e) => {
                            error!("Failed to upload {}: {}", change.path(), e);
                            report
                                .errors
                                .push(format!("Upload {}: {}", change.path(), e));
                        }
                    }
                }
            } else {
                report.uploaded.0 += 1;
                if !dry_run {
                    match self.apply_to_couchdb(&change).await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("Failed to upload {}: {}", change.path(), e);
                            report
                                .errors
                                .push(format!("Upload {}: {}", change.path(), e));
                        }
                    }
                }
            }
        }

        // 6. Apply clean remote changes to local (skipped in dry-run)
        info!(
            "========== DOWNLOADING {} FILES ==========",
            remote_to_apply.len()
        );
        for change in remote_to_apply {
            debug!(
                "  Preparing to download: {} -> {}",
                change.path(),
                self.couchdb.get_local_path(change.path())
            );

            if matches!(change.change_type(), ChangeType::Deleted) {
                report.deleted_local += 1;
                if !dry_run {
                    match self.apply_to_filesystem(&change).await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("Failed to download {}: {}", change.path(), e);
                            report
                                .errors
                                .push(format!("Download {}: {}", change.path(), e));
                        }
                    }
                }
            } else {
                report.downloaded.0 += 1;
                if !dry_run {
                    match self.apply_to_filesystem(&change).await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("Failed to download {}: {}", change.path(), e);
                            report
                                .errors
                                .push(format!("Download {}: {}", change.path(), e));
                        }
                    }
                }
            }
        }

        // 7. Update checkpoint (skipped in dry-run)
        if !dry_run {
            self.local_db.save_checkpoint(&last_seq).await?;
        }

        if dry_run {
            info!(
                "========== DRY-RUN COMPLETE: {} would upload, {} would download, {} conflicts ==========",
                report.uploaded, report.downloaded, report.conflicts
            );
        } else {
            info!(
                "========== SYNC COMPLETE: {} uploaded, {} downloaded, {} conflicts ==========",
                report.uploaded, report.downloaded, report.conflicts
            );
        }

        Ok(report)
    }

    /// Rebuild the remote scope so it exactly matches the local filesystem.
    pub async fn rebuild_remote_from_local(&mut self) -> Result<SyncReport> {
        info!("========== REMOTE REBUILD STARTING ==========");

        let local_states = self.scanner.full_scan().await?;
        let remote_docs = self.couchdb.get_all_files().await?;
        let (uploads, remote_deletes) =
            triage::plan_remote_rebuild(&local_states, &remote_docs, self.couchdb.remote_prefix());

        self.local_db.reset_sync_state().await?;

        let mut report = SyncReport::default();

        for local_path in uploads {
            let remote_path = self.couchdb.get_remote_path(&local_path);
            self.upload_local_file(&local_path, &remote_path).await?;
            report.uploaded.0 += 1;
        }

        for remote_path in remote_deletes {
            self.couchdb.delete_file(&remote_path).await?;
            report.deleted_remote += 1;
        }

        info!(
            "========== REMOTE REBUILD COMPLETE: {} uploaded, {} remote deletes ==========",
            report.uploaded, report.deleted_remote
        );

        Ok(report)
    }

    /// Rebuild the local filesystem so it exactly matches the remote scope.
    pub async fn rebuild_local_from_remote(&mut self) -> Result<SyncReport> {
        info!("========== LOCAL REBUILD STARTING ==========");

        let local_states = self.scanner.full_scan().await?;
        let remote_docs = self.couchdb.get_all_files().await?;
        let (local_deletes, remote_downloads) =
            triage::plan_local_rebuild(&local_states, &remote_docs);

        self.local_db.reset_sync_state().await?;

        let mut report = SyncReport::default();

        for local_path in local_deletes {
            let file_path = self.root_dir.as_path().join(&local_path);
            if file_path.exists() {
                tokio::fs::remove_file(&file_path).await?;
                report.deleted_local += 1;
            }
        }

        for remote_path in remote_downloads {
            let local_path = self.couchdb.get_local_path(&remote_path);
            if self
                .download_remote_file(&remote_path, &local_path, true)
                .await?
                .is_some()
            {
                report.downloaded.0 += 1;
            }
        }

        info!(
            "========== LOCAL REBUILD COMPLETE: {} deleted locally, {} downloaded ==========",
            report.deleted_local, report.downloaded
        );

        Ok(report)
    }

    /// Scan for local changes.
    ///
    /// In dry-run mode the scan still detects changes, but the state-DB
    /// cleanups (removing polluted/ignored entries and re-saving unchanged
    /// states) are skipped so that nothing is written to the state DB.
    async fn scan_local_changes(&self, dry_run: bool) -> Result<Vec<Change>> {
        let current_states = self.scanner.full_scan().await?;
        let stored_states = self.local_db.get_all_file_states().await?;
        let remote_prefix = self.couchdb.remote_prefix();
        let mut valid_stored_states = Vec::with_capacity(stored_states.len());

        for state in stored_states {
            if triage::is_polluted_state_path(&state.path, remote_prefix) {
                warn!(
                    "Removing invalid state entry for {}: local state includes remote prefix {}",
                    state.path, remote_prefix
                );
                if !dry_run {
                    self.local_db.delete_file_state(&state.path).await?;
                }
            } else if self
                .ignore_matcher
                .should_ignore(std::path::Path::new(&state.path))
            {
                info!(
                    "Removing ignored state entry from local database: {}",
                    state.path
                );
                if !dry_run {
                    self.local_db.delete_file_state(&state.path).await?;
                }
            } else {
                valid_stored_states.push(state);
            }
        }

        debug!("Scanned {} files on disk", current_states.len());
        debug!(
            "Found {} files in local database",
            valid_stored_states.len()
        );

        let changes = self
            .scanner
            .detect_changes(&current_states, &valid_stored_states);

        debug!("Detected {} changes from local scan", changes.len());
        for change in &changes {
            debug!(
                "  Local change: {} ({:?})",
                change.path(),
                change.change_type()
            );
        }

        // Only update stored states for files that haven't changed
        // (new and modified files will be updated after successful sync).
        // Skipped entirely in dry-run mode so the state DB is left untouched.
        if !dry_run {
            // Build a map of stored states to preserve couch_rev
            let stored_map: HashMap<_, _> =
                valid_stored_states.iter().map(|s| (&s.path, s)).collect();

            // Prebuild a set of changed paths so per-file membership checks
            // are O(1) instead of an O(M) scan over the changes list.
            let changed_paths: HashSet<&str> = changes.iter().map(|c| c.path()).collect();

            for state in &current_states {
                // Check if this file is in the changes list
                let is_changed = changed_paths.contains(state.path.as_str());
                if !is_changed {
                    // File unchanged - preserve the couch_rev and last_sync_at
                    // from the stored state. The freshly-scanned state's
                    // `last_sync_at` is set to the scan time by the scanner and
                    // must not clobber the real last-sync timestamp: doing so
                    // makes any remote change that arrived since the previous
                    // sync look stale (remote mtime < last_sync_at) and it would
                    // never be applied locally.
                    let stored = stored_map.get(&state.path);
                    let couch_rev = stored.and_then(|s| s.couch_rev.clone());
                    let last_sync_at = stored.map(|s| s.last_sync_at).unwrap_or(state.last_sync_at);
                    let preserved_state = FileState {
                        path: state.path.clone(),
                        hash: state.hash.clone(),
                        size: state.size,
                        modified_at: state.modified_at,
                        couch_rev,
                        last_sync_at,
                    };
                    // Skip the redundant SQLite write entirely when the
                    // preserved state (path, hash, size, modified_at,
                    // couch_rev, last_sync_at) is byte-identical to the stored
                    // row, so a no-op sync issues zero file-state writes
                    // instead of rewriting every unchanged file via
                    // INSERT OR REPLACE each cycle.
                    let identical_to_stored = stored.is_some_and(|s| (**s) == preserved_state);
                    if !identical_to_stored {
                        self.local_db.save_file_state(&preserved_state).await?;
                    }
                }
            }
        }

        Ok(changes)
    }

    /// Fetch remote changes from CouchDB
    async fn fetch_remote_changes(&self) -> Result<(Vec<Change>, String)> {
        let checkpoint = self.local_db.get_checkpoint().await?;
        let since = checkpoint.map(|cp| cp.last_seq);

        self.couchdb.get_changes(since.as_deref()).await
    }

    /// Detect conflicts between local and remote changes
    ///
    /// In dry-run mode the identical-content "silent sync" branch skips the
    /// state-DB save; conflict detection still runs and conflicts are returned
    /// as if they would be recorded.
    async fn detect_conflicts(
        &self,
        local_changes: &[Change],
        remote_changes: &[Change],
        dry_run: bool,
    ) -> Result<(Vec<Change>, Vec<Change>, Vec<Conflict>)> {
        // Build a complete map of stored states (one I/O batch)
        let mut stored_states: HashMap<String, FileState> = HashMap::new();
        // Actually load all stored states individually (keeps existing pattern)
        for lc in local_changes {
            if !stored_states.contains_key(lc.path()) {
                if let Some(state) = self.local_db.get_file_state(lc.path()).await? {
                    stored_states.insert(lc.path().to_string(), state);
                }
            }
        }
        for rc in remote_changes {
            let local_path = self.couchdb.get_local_path(rc.path());
            // Use entry API: clone the key for the lookup to avoid borrow-after-move
            if let std::collections::hash_map::Entry::Vacant(e) =
                stored_states.entry(local_path.clone())
            {
                if let Some(state) = self.local_db.get_file_state(&local_path).await? {
                    e.insert(state);
                }
            }
        }

        let remote_prefix = self.couchdb.remote_prefix();

        debug!(
            "========== ANALYZING {} LOCAL CHANGES ==========",
            local_changes.len()
        );
        for lc in local_changes {
            let remote_path = self.couchdb.get_remote_path(lc.path());
            debug!("--- LOCAL CHANGE: {} ---", lc.path());
            debug!("  Local path: {}", lc.path());
            debug!("  Remote path: {}", remote_path);
            debug!("  Change type: {:?}", lc.change_type());

            if let Some(state) = stored_states.get(lc.path()) {
                debug!("  STORED STATE:");
                debug!("    hash: {}...", &state.hash[..8.min(state.hash.len())]);
                debug!("    size: {} bytes", state.size);
                debug!("    modified_at: {:?}", state.modified_at);
                debug!("    couch_rev: {:?}", state.couch_rev);
                debug!("    last_sync_at: {:?}", state.last_sync_at);
            } else {
                debug!("  NO STORED STATE (first time sync)");
            }

            // Extra debug for remote change detection
            if let Some(rc) = remote_changes.iter().find(|rc| rc.path() == remote_path) {
                if let Some(remote_mtime) = rc.mtime() {
                    if let Some(state) = stored_states.get(lc.path()) {
                        if *remote_mtime > state.last_sync_at {
                            info!("  [REMOTE CHANGE DETECTED] {}", lc.path());
                            info!(
                                "    Remote mtime: {} | Last sync: {} | Diff: +{}s",
                                remote_mtime.format("%Y-%m-%d %H:%M:%S"),
                                state.last_sync_at.format("%Y-%m-%d %H:%M:%S"),
                                (*remote_mtime - state.last_sync_at).num_seconds()
                            );
                            if let Some(remote_rev) = &rc.rev() {
                                let stored_rev = state.couch_rev.as_deref().unwrap_or("none");
                                info!(
                                    "    Remote rev: {} | Stored rev: {}",
                                    &remote_rev[..12.min(remote_rev.len())],
                                    &stored_rev[..12.min(stored_rev.len())]
                                );
                            }
                            if let Some(remote_size) = rc.size() {
                                info!(
                                    "    Remote size: {} bytes | Local size: {} bytes",
                                    remote_size, state.size
                                );
                            }
                        } else {
                            debug!(
                                "    {} - remote_mtime ({}) <= last_sync_at ({}), no remote change",
                                lc.path(),
                                remote_mtime.format("%Y-%m-%d %H:%M:%S"),
                                state.last_sync_at.format("%Y-%m-%d %H:%M:%S")
                            );
                        }
                    } else {
                        info!(
                            "  [REMOTE CHANGE DETECTED] {} - no stored state (first sync)",
                            lc.path()
                        );
                        info!(
                            "    Remote mtime: {:?} | Remote rev: {:?} | Remote size: {:?}",
                            rc.mtime(),
                            rc.rev(),
                            rc.size()
                        );
                    }
                } else {
                    info!("  [REMOTE CHANGE DETECTED] {} - no remote mtime available, assuming changed", lc.path());
                    if let Some(state) = stored_states.get(lc.path()) {
                        info!(
                            "    Stored rev: {:?} | Remote rev: {:?}",
                            state.couch_rev,
                            rc.rev()
                        );
                    }
                }
            } else {
                debug!("  {} - file not on remote yet", lc.path());
            }
        }

        // ── Run the pure triage function ──────────────────────────────────
        let triage_result =
            triage::triage_changes(local_changes, remote_changes, &stored_states, remote_prefix);

        // Collect results
        let local_to_upload = triage_result.uploads;
        let mut remote_to_apply = triage_result.downloads;
        let mut conflicts: Vec<Conflict> = Vec::new();

        // ── Handle needs_comparison pairs (requires I/O) ──────────────────
        for decision in &triage_result.needs_comparison {
            let lc = match &decision.local_change {
                Some(c) => c,
                None => continue,
            };
            let _rc = match &decision.remote_change {
                Some(c) => c,
                None => continue,
            };

            let remote_path = self.couchdb.get_remote_path(lc.path());
            debug!("  => Remote is newer, fetching content to compare...");

            // Fetch remote metadata
            let remote_doc = match self.couchdb.fetch_metadata(&remote_path).await? {
                Some(doc) => doc,
                None => {
                    debug!("  Remote document not found!");
                    continue;
                }
            };

            // Get local state for comparison
            let local_state = self.get_local_state(lc.path()).await?;
            debug!("  LOCAL STATE (from disk):");
            debug!(
                "    hash: {}...",
                &local_state.hash[..8.min(local_state.hash.len())]
            );
            debug!("    size: {} bytes", local_state.size);
            debug!("    modified_at: {:?}", local_state.modified_at);

            // Download remote content and compute hash for comparison
            let remote_content = self.couchdb.get_file_content(&remote_path).await?;
            let remote_hash = compute_bytes_hash(&remote_content);

            debug!("  COMPARING CONTENT:");
            debug!(
                "    local hash:  {}",
                &local_state.hash[..8.min(local_state.hash.len())]
            );
            debug!(
                "    remote hash: {}",
                &remote_hash[..8.min(remote_hash.len())]
            );
            debug!("    local size:  {} bytes", local_state.size);
            debug!("    remote size: {} bytes", remote_content.len());

            // Compare hashes to determine if content actually differs
            if local_state.hash == remote_hash {
                info!(
                    "  [OK] {} - content identical (hash: {}), updating sync state",
                    lc.path(),
                    &local_state.hash[..8.min(local_state.hash.len())]
                );
                // Update local state to reflect remote rev (skipped in dry-run)
                if !dry_run {
                    let updated_state = FileState {
                        path: lc.path().to_string(),
                        hash: local_state.hash,
                        size: local_state.size,
                        modified_at: local_state.modified_at,
                        couch_rev: remote_doc.rev.as_deref().and_then(CouchRev::new),
                        last_sync_at: Utc::now(),
                    };
                    self.local_db.save_file_state(&updated_state).await?;
                }
            } else {
                info!(
                    "  [CONFLICT] {} - content differs (local: {}, remote: {})",
                    lc.path(),
                    &local_state.hash[..8.min(local_state.hash.len())],
                    &remote_hash[..8.min(remote_hash.len())]
                );

                // Convert mtime (milliseconds since epoch) to DateTime
                use chrono::TimeZone;
                let remote_modified_at = Utc
                    .timestamp_millis_opt(remote_doc.mtime.as_u64() as i64)
                    .single()
                    .unwrap_or_else(Utc::now);

                let remote_state = RemoteState {
                    hash: remote_hash,
                    size: remote_content.len() as u64,
                    modified_at: remote_modified_at,
                    couch_rev: CouchRev::new(
                        remote_doc.rev.as_deref().unwrap_or(CouchRev::DEFAULT_REV),
                    )
                    .unwrap_or_default(),
                    deleted: remote_doc.deleted,
                };
                debug!("  REMOTE STATE:");
                debug!("    size: {} bytes", remote_state.size);
                debug!("    modified_at: {:?}", remote_state.modified_at);
                debug!("    couch_rev: {:?}", remote_state.couch_rev);

                conflicts.push(Conflict::new(
                    lc.path().to_string(),
                    local_state,
                    remote_state,
                ));
            }
        }

        // ── Handle remote deletes ─────────────────────────────────────────
        for rc in &triage_result.remote_deletes {
            let local_path = self.couchdb.get_local_path(rc.path());
            let relative_path = local_path.trim_start_matches('/');
            let file_path = self.root_dir.as_path().join(relative_path);
            let stored_state = stored_states.get(&local_path);
            if triage::should_apply_remote_delete(
                stored_state,
                rc.mtime().copied(),
                file_path.exists(),
            ) {
                debug!("  Remote delete is newer than last sync, scheduling local delete");
                remote_to_apply.push(rc.clone());
            } else if file_path.exists() || stored_state.is_some() {
                debug!("  Remote delete is stale or file is untracked locally, skipping");
            } else {
                debug!("  Remote deleted, no local file/state, skipping");
            }
        }

        // ── Debug skipped items ───────────────────────────────────────────
        for decision in &triage_result.skipped {
            debug!("  [SKIP] {} - already in sync", decision.path);
        }

        debug!("========== ANALYSIS COMPLETE ==========");
        debug!("  Uploads queued: {}", local_to_upload.len());
        debug!("  Downloads queued: {}", remote_to_apply.len());
        debug!("  Conflicts found: {}", conflicts.len());

        Ok((local_to_upload, remote_to_apply, conflicts))
    }

    /// Get local file state    /// Get local file state
    async fn get_local_state(&self, path: &str) -> Result<FileState> {
        // Strip leading / to prevent absolute path issues
        let relative_path = path.trim_start_matches('/');
        let file_path = self.root_dir.as_path().join(relative_path);

        // Hashing is a blocking open+read+SHA-256 of the full file, so run it on
        // tokio's blocking thread pool instead of stalling the async executor.
        let hash_path = file_path.clone();
        let hash = tokio::task::spawn_blocking(move || compute_file_hash(&hash_path))
            .await
            .map_err(|e| anyhow::anyhow!("Hash task panicked for {}: {}", file_path.display(), e))?
            .map_err(|e| {
                anyhow::anyhow!("Failed to compute hash for {}: {}", file_path.display(), e)
            })?;
        let metadata = tokio::fs::metadata(&file_path).await.map_err(|e| {
            anyhow::anyhow!("Failed to read metadata for {}: {}", file_path.display(), e)
        })?;

        Ok(FileState::new(
            path.to_string(),
            hash,
            metadata.len(),
            metadata.modified()?.into(),
        ))
    }

    async fn upload_local_file(
        &mut self,
        local_path: &str,
        remote_path: &str,
    ) -> Result<(usize, Option<String>)> {
        let relative_path = local_path.trim_start_matches('/');
        let file_path = self.root_dir.as_path().join(relative_path);
        let metadata = tokio::fs::metadata(&file_path).await?;
        let mtime = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        debug!("[UPLOAD] File path: {:?}", file_path);
        debug!("[UPLOAD] Size: {} bytes", metadata.len());
        debug!("[UPLOAD] mtime: {} ms", mtime);

        let content = tokio::fs::read(&file_path).await?;
        debug!("[UPLOAD] Read {} bytes from disk", content.len());

        let new_chunk_ids = self.couchdb.upload_file_content(&content).await?;
        debug!(
            "[UPLOAD] Uploaded content, got {} chunks",
            new_chunk_ids.len()
        );

        let (existing_rev, existing_ctime, old_chunk_ids) =
            match self.couchdb.get_file(remote_path).await? {
                Some(existing) => {
                    debug!("[UPLOAD] Existing doc found, rev: {:?}", existing.rev);
                    debug!("[UPLOAD] Existing ctime: {} ms", existing.ctime.as_u64());
                    debug!("[UPLOAD] Existing chunks: {}", existing.children.len());
                    (existing.rev, existing.ctime.as_u64(), existing.children)
                }
                None => {
                    debug!("[UPLOAD] No existing doc, creating new");
                    (None, mtime, Vec::new())
                }
            };

        let mut doc = crate::models::FileDoc {
            id: remote_path.to_string(),
            rev: existing_rev,
            children: new_chunk_ids,
            path: remote_path.to_string(),
            ctime: crate::models::TimestampMillis::new(existing_ctime),
            mtime: crate::models::TimestampMillis::new(mtime),
            size: metadata.len(),
            doc_type: crate::models::DocType::Plain,
            deleted: false,
        };

        self.couchdb.save_file(&mut doc).await?;
        debug!("[UPLOAD] Saved doc, new rev: {:?}", doc.rev);

        if !old_chunk_ids.is_empty() {
            debug!("[UPLOAD] Deleting {} old chunks", old_chunk_ids.len());
            self.couchdb.delete_chunks(&old_chunk_ids).await?;
        }

        // Hash the in-memory content buffer we just uploaded instead of
        // re-reading the file from disk. Since `content` is exactly the bytes
        // pushed to CouchDB, hashing it keeps the saved FileState in agreement
        // with the transferred bytes even under a mid-sync modification.
        let hash = compute_bytes_hash(&content);
        let state = FileState {
            path: local_path.to_string(),
            hash,
            size: metadata.len(),
            modified_at: metadata.modified()?.into(),
            couch_rev: doc.rev.as_deref().and_then(CouchRev::new),
            last_sync_at: Utc::now(),
        };
        self.local_db.save_file_state(&state).await?;
        debug!("[UPLOAD] Updated local state with rev: {:?}", doc.rev);

        Ok((content.len(), doc.rev))
    }

    async fn download_remote_file(
        &mut self,
        remote_path: &str,
        local_path: &str,
        require_doc: bool,
    ) -> Result<Option<usize>> {
        let doc = match self.couchdb.fetch_metadata(remote_path).await? {
            Some(d) => d,
            None => {
                if require_doc {
                    anyhow::bail!("Document not found in CouchDB: {}", remote_path);
                }
                warn!("Document not found in CouchDB: {}", remote_path);
                return Ok(None);
            }
        };

        let relative_path = local_path.trim_start_matches('/');
        let file_path = self.root_dir.as_path().join(relative_path);
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        debug!("[DOWNLOAD] Downloading {} chunks...", doc.children.len());
        let content = match self.couchdb.get_file_content(remote_path).await {
            Ok(data) => {
                debug!("[DOWNLOAD] Downloaded {} bytes from chunks", data.len());
                data
            }
            Err(e) => {
                // A content-fetch failure is a real sync failure, not "no
                // content": propagate it instead of swallowing it. Otherwise a
                // transient network/auth/server error would silently truncate
                // a valid local file to zero bytes and record it as synced.
                anyhow::bail!("failed to fetch content for {}: {}", remote_path, e);
            }
        };

        tokio::fs::write(&file_path, &content).await?;
        debug!("[DOWNLOAD] Wrote {} bytes to disk", content.len());

        // Hash the in-memory content buffer we just fetched and wrote instead
        // of re-reading the file back from disk. This avoids a redundant disk
        // read and keeps the saved hash equal to the bytes actually transferred.
        let hash = compute_bytes_hash(&content);
        let metadata = tokio::fs::metadata(&file_path).await?;
        let state = FileState {
            path: local_path.to_string(),
            hash,
            size: metadata.len(),
            modified_at: metadata.modified()?.into(),
            couch_rev: doc.rev.as_deref().and_then(CouchRev::new),
            last_sync_at: Utc::now(),
        };
        self.local_db.save_file_state(&state).await?;

        Ok(Some(content.len()))
    }

    /// Apply a change to CouchDB
    async fn apply_to_couchdb(&mut self, change: &Change) -> Result<()> {
        let remote_path = self.couchdb.get_remote_path(change.path());
        debug!("[UPLOAD] Starting: {} -> {}", change.path(), remote_path);
        debug!("[UPLOAD] Change type: {:?}", change.change_type());

        match change.change_type() {
            ChangeType::Created | ChangeType::Modified => {
                let (bytes_uploaded, new_rev) =
                    self.upload_local_file(change.path(), &remote_path).await?;
                info!(
                    "[UPLOAD] SUCCESS: {} -> {} ({} bytes, rev: {:?})",
                    change.path(),
                    remote_path,
                    bytes_uploaded,
                    new_rev
                );
            }
            ChangeType::Deleted => {
                debug!("[DELETE] Remote: {}", remote_path);
                self.couchdb.delete_file(&remote_path).await?;
                self.local_db.delete_file_state(change.path()).await?;
                info!("[DELETE] SUCCESS: {} -> {}", change.path(), remote_path);
            }
        }
        Ok(())
    }

    /// Apply a change to the local filesystem
    async fn apply_to_filesystem(&mut self, change: &Change) -> Result<()> {
        let remote_path = change.path();
        let local_path = self.couchdb.get_local_path(remote_path);
        let relative_path = local_path.trim_start_matches('/');
        let file_path = self.root_dir.as_path().join(relative_path);

        debug!("[DOWNLOAD] Remote is newer, downloading chunked file");
        debug!("[DOWNLOAD] {} -> {}", remote_path, local_path);

        match change.change_type() {
            ChangeType::Created | ChangeType::Modified => {
                if let Some(bytes) = self
                    .download_remote_file(remote_path, &local_path, false)
                    .await?
                {
                    info!(
                        "[DOWNLOAD] Chunked file downloaded: {} ({} bytes)",
                        local_path, bytes
                    );
                }
            }
            ChangeType::Deleted => {
                debug!(
                    "[LOCAL DELETE] Remote: {}, Local: {:?}",
                    remote_path, file_path
                );
                if file_path.exists() {
                    tokio::fs::remove_file(&file_path).await?;
                }
                self.local_db.delete_file_state(&local_path).await?;
                info!("[LOCAL DELETE] SUCCESS: {} -> {}", remote_path, local_path);
            }
        }
        Ok(())
    }

    /// Get list of conflicts
    pub async fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        self.local_db.get_conflicts().await
    }

    /// Apply a local change immediately (live sync)
    pub async fn apply_local_change(&mut self, change: &Change) -> Result<()> {
        self.apply_to_couchdb(change).await
    }

    /// Apply a remote change immediately (live sync)
    pub async fn apply_remote_change(&mut self, change: &Change) -> Result<()> {
        self.apply_to_filesystem(change).await
    }

    /// Get local tracked file state
    pub async fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        self.local_db.get_file_state(path).await
    }

    /// Save sync checkpoint
    pub async fn save_checkpoint(&self, seq: &str) -> Result<()> {
        self.local_db.save_checkpoint(seq).await
    }

    /// Get sync checkpoint
    pub async fn get_checkpoint(&self) -> Result<Option<Checkpoint>> {
        self.local_db.get_checkpoint().await
    }

    /// Convert local path to remote path using the configured prefix
    pub fn local_to_remote_path(&self, local_path: &str) -> String {
        self.couchdb.get_remote_path(local_path)
    }

    /// Convert remote path to local path by stripping the configured prefix
    pub fn remote_to_local_path(&self, remote_path: &str) -> String {
        self.couchdb.get_local_path(remote_path)
    }

    /// Get remote file content (converts local path to remote path)
    pub async fn get_remote_content(&self, local_path: &str) -> Result<Vec<u8>> {
        let remote_path = self.couchdb.get_remote_path(local_path);
        self.couchdb.get_file_content(&remote_path).await
    }

    /// Get the root directory
    pub fn root_dir(&self) -> &SyncDirPath {
        &self.root_dir
    }

    /// Get the ignore matcher (for testing)
    pub fn ignore_matcher(&self) -> &IgnoreMatcher {
        &self.ignore_matcher
    }

    /// Resolve a conflict
    /// Note: `local_path` is the local file path (stored in conflict), which gets
    /// converted to remote path when interacting with CouchDB
    pub async fn resolve_conflict(
        &mut self,
        local_path: &str,
        strategy: ResolutionStrategy,
    ) -> Result<()> {
        let _conflict = match self.local_db.get_conflict(local_path).await? {
            Some(c) => c,
            None => {
                anyhow::bail!("No conflict found for path: {}", local_path);
            }
        };

        // Convert local path to remote path for CouchDB operations
        let remote_path = self.couchdb.get_remote_path(local_path);

        match strategy {
            ResolutionStrategy::KeepLocal => {
                // Force upload local version to remote
                self.upload_local_file(local_path, &remote_path).await?;
                info!(
                    "Resolved conflict (keep-local): {} - uploaded to remote",
                    local_path
                );
            }
            ResolutionStrategy::KeepRemote => {
                // Force download remote version
                self.download_remote_file(&remote_path, local_path, true)
                    .await?;
                info!("Resolved conflict (keep-remote): {}", local_path);
            }
            ResolutionStrategy::KeepBoth => {
                // Save remote as .remote file
                let local_remote_path = format!("{}.remote", local_path);
                self.download_remote_file(&remote_path, &local_remote_path, true)
                    .await?;
                info!("Saved remote version as: {}", local_remote_path);

                // Local file stays as-is
                // User can manually merge/compare
            }
            ResolutionStrategy::Skip => {
                // Do nothing, leave conflict for later
                return Ok(());
            }
        }

        // Remove conflict record
        self.local_db.delete_conflict(local_path).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::couchdb::db::CannedCouch;
    use crate::couchdb::CouchDb;
    use crate::local::{compute_bytes_hash, compute_file_hash, LocalDb};
    use crate::models::{Change, CouchRev, FileDoc, IgnoreMatcher, TimestampMillis};
    use chrono::{Duration, Utc};
    use std::path::PathBuf;

    /// Create a minimal CouchDb instance for testing construction.
    /// Not all methods work without a real server.
    fn test_couchdb() -> CouchDb {
        CouchDb::for_test("test-prefix/")
    }

    fn test_local_db() -> LocalDb {
        LocalDb::open_in_memory().expect("in-memory LocalDb should construct")
    }

    fn test_root(path: &str) -> SyncDirPath {
        std::fs::create_dir_all(path).expect("create test dir");
        SyncDirPath::new(PathBuf::from(path)).expect("create SyncDirPath")
    }

    #[tokio::test]
    async fn engine_new_constructs_with_empty_ignore_matcher() {
        let couch = test_couchdb();
        let local = test_local_db();
        let root = test_root("/tmp/test-sync-engine-new");

        let engine = SyncEngine::new(couch, local, root.clone());

        assert_eq!(engine.root_dir(), &root);
        assert!(engine.get_conflicts().await.unwrap().is_empty());
        // with_ignore uses IgnoreMatcher::empty() internally
        // The default empty matcher should not ignore anything
        assert!(!engine
            .ignore_matcher()
            .should_ignore(std::path::Path::new("test.txt")));
    }

    #[test]
    fn engine_with_ignore_constructs_with_custom_ignore_matcher() {
        let couch = test_couchdb();
        let local = test_local_db();
        let root = test_root("/tmp/test-sync-engine-with-ignore");

        let matcher = IgnoreMatcher::from_content("*.log\nnode_modules/");
        let engine = SyncEngine::with_ignore(couch, local, root, matcher);

        assert!(engine
            .ignore_matcher()
            .should_ignore(std::path::Path::new("debug.log")));
        assert!(engine
            .ignore_matcher()
            .should_ignore(std::path::Path::new("node_modules/pkg/index.js")));
        assert!(!engine
            .ignore_matcher()
            .should_ignore(std::path::Path::new("src/main.rs")));
    }

    #[test]
    fn engine_new_and_with_ignore_produce_same_root_dir() {
        let root = test_root("/tmp/test-sync-engine-root");
        let engine1 = SyncEngine::new(test_couchdb(), test_local_db(), root.clone());
        let engine2 = SyncEngine::with_ignore(
            test_couchdb(),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::from_content("secret/"),
        );

        assert_eq!(engine1.root_dir(), &root);
        assert_eq!(engine2.root_dir(), &root);
    }

    #[tokio::test]
    async fn engine_initial_state_has_no_conflicts_and_no_file_states() {
        let engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            test_root("/tmp/test-sync-engine-state"),
        );

        // Fresh engine should have no conflicts
        assert!(engine.get_conflicts().await.unwrap().is_empty());

        // Fresh engine should have no file states
        assert!(engine
            .get_file_state("nonexistent.txt")
            .await
            .unwrap()
            .is_none());
        assert!(engine
            .get_file_state("other/path.md")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn engine_with_ignore_can_checkpoint() {
        let engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            test_root("/tmp/test-sync-engine-checkpoint"),
        );

        // Save a checkpoint
        engine.save_checkpoint("123-abc").await.unwrap();

        // Read it back
        let cp = engine.get_checkpoint().await.unwrap();
        assert!(cp.is_some());
        assert_eq!(cp.unwrap().last_seq, "123-abc");
    }

    #[test]
    fn engine_path_conversion_methods_use_couchdb_prefix() {
        let engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            test_root("/tmp/test-sync-engine-paths"),
        );

        // local_to_remote_path should prepend the prefix
        let remote = engine.local_to_remote_path("doc.txt");
        assert_eq!(remote, "test-prefix/doc.txt");

        // remote_to_local_path should strip the prefix
        let local = engine.remote_to_local_path("test-prefix/doc.txt");
        assert_eq!(local, "doc.txt");
    }

    #[tokio::test]
    async fn get_local_state_returns_byte_identical_hash_after_blocking_compute() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.as_path().join("data.bin"), b"regression content").unwrap();

        let engine = SyncEngine::new(test_couchdb(), test_local_db(), root.clone());

        let state = engine
            .get_local_state("data.bin")
            .await
            .expect("get_local_state should compute local state");

        // The hash must be byte-identical to a direct blocking hash of the same
        // file: moving the hash computation off the async executor via
        // spawn_blocking must not change the returned FileState.
        let expected_hash =
            compute_file_hash(&root.as_path().join("data.bin")).expect("direct compute_file_hash");
        assert_eq!(
            state.hash, expected_hash,
            "local state hash must be byte-identical to a direct file hash"
        );
        assert_eq!(
            state.hash,
            compute_bytes_hash(b"regression content"),
            "hash must match the file bytes"
        );
        assert_eq!(state.size, b"regression content".len() as u64);
        assert_eq!(state.path, "data.bin");
    }

    // ── Dry-run sync cycle ─────────────────────────────────────────────

    fn test_canned_couch(remote_path: &str, canned: CannedCouch) -> CouchDb {
        CouchDb::for_test_with_canned(remote_path, canned)
    }

    fn seed_file_state(local: &LocalDb, path: &str, hash: &str, size: u64) {
        let state = FileState::new(
            path.to_string(),
            hash.to_string(),
            size,
            Utc::now() - Duration::days(1),
        );
        local.save_file_state(&state).expect("seed file state");
    }

    #[tokio::test]
    async fn dry_run_counts_local_uploads_and_remotes_deletes_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        // New local file (would upload)
        std::fs::write(root.as_path().join("new.txt"), "hello new\n").unwrap();
        // A previously tracked file that no longer exists (would delete remote)
        let local = test_local_db();
        seed_file_state(&local, "gone.txt", "deadbeef", 4);

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "500-seed".to_string(),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync_dry_run().await.expect("dry run uploads");

        assert_eq!(report.uploaded.0, 1, "new.txt should be counted as upload");
        assert_eq!(
            report.deleted_remote, 1,
            "gone.txt should be counted as remote delete"
        );
        assert_eq!(report.downloaded.0, 0);
        assert_eq!(report.deleted_local, 0);
        assert!(report.conflicts == 0);
        assert!(report.errors.is_empty());

        // Trieage ran: the planned upload is the new file.
        // Dry run wrote nothing to the state DB:
        assert!(
            engine.get_file_state("new.txt").await.unwrap().is_none(),
            "dry run must not save file states"
        );
        assert!(
            engine.get_file_state("gone.txt").await.unwrap().is_some(),
            "dry run must not delete existing file states"
        );
        assert!(engine.get_conflicts().await.unwrap().is_empty());
        assert!(
            engine.get_checkpoint().await.unwrap().is_none(),
            "dry run must not save a checkpoint"
        );
        // Dry run issued no CouchDB writes:
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "dry run must not write to CouchDB"
        );
        // Local file untouched
        assert!(root.as_path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn dry_run_counts_downloads_without_writing_files_or_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        // Tracked local file, unchanged on disk.
        std::fs::write(root.as_path().join("foo.txt"), "foo-content").unwrap();
        let local = test_local_db();
        let foo_hash = compute_file_hash(&root.as_path().join("foo.txt")).unwrap();
        local.save_checkpoint("100-before").unwrap();

        let mut stored = FileState::new(
            "foo.txt".to_string(),
            foo_hash.clone(),
            11,
            Utc::now() - Duration::days(1),
        );
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_prefix = "prefix/";
        let canned = CannedCouch {
            changes: vec![
                // Unchanged local file now has a new rev -> download
                Change::remote_modified(
                    format!("{}foo.txt", remote_prefix),
                    "someremotehash".to_string(),
                    11,
                    Utc::now(),
                    "2-def".to_string(),
                ),
                // Brand new remote file -> download
                Change::remote_created(
                    format!("{}bar.txt", remote_prefix),
                    "barhash".to_string(),
                    3,
                    Utc::now(),
                    "1-x".to_string(),
                ),
            ],
            last_seq: "200-after".to_string(),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch(remote_prefix, canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync_dry_run().await.expect("dry run downloads");

        assert_eq!(report.downloaded.0, 2, "foo.txt and bar.txt downloads");
        assert_eq!(report.uploaded.0, 0);
        assert_eq!(report.deleted_local, 0);
        assert_eq!(report.deleted_remote, 0);
        assert!(report.conflicts == 0);

        // Nothing was written locally:
        assert!(
            !root.as_path().join("bar.txt").exists(),
            "dry run must not create downloaded files"
        );
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("foo.txt")).unwrap(),
            "foo-content",
            "dry run must not overwrite local files"
        );
        // State DB untouched (checkpoint still the seeded one, rev unchanged):
        assert_eq!(
            engine.get_checkpoint().await.unwrap().unwrap().last_seq,
            "100-before",
            "dry run must not advance the checkpoint"
        );
        let foo_state = engine.get_file_state("foo.txt").await.unwrap().unwrap();
        assert_eq!(
            foo_state.couch_rev.map(|r| r.to_string()),
            Some("1-abc".to_string())
        );
        assert!(engine.get_conflicts().await.unwrap().is_empty());
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "dry run must not write to CouchDB"
        );
    }

    #[tokio::test]
    async fn dry_run_detects_conflicts_without_persisting_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        // Local file changed since last sync.
        std::fs::write(root.as_path().join("both.txt"), "local-content").unwrap();
        let local = test_local_db();
        // Stored state is stale (old hash) so the local change is detected.
        seed_file_state(&local, "both.txt", "stalehash", 13);
        // Give the stored state an old enough last_sync_at.
        let mut stored = local.get_file_state("both.txt").unwrap().unwrap();
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_path = "prefix/both.txt";
        let remote_modified_at = TimestampMillis::now();
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 13);
        remote_doc.rev = Some("2-def".to_string());
        remote_doc.mtime = remote_modified_at;
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: vec![Change::remote_modified(
                remote_path.to_string(),
                "remotehash".to_string(),
                14,
                Utc::now(),
                "2-def".to_string(),
            )],
            last_seq: "900".to_string(),
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc.clone(),
            )]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                b"remote-content".to_vec(),
            )]),
            content_errors: std::collections::HashSet::new(),
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync_dry_run().await.expect("dry run conflict");

        assert_eq!(report.conflicts, 1, "content differs -> conflict");
        assert_eq!(report.uploaded.0, 0);
        assert_eq!(report.downloaded.0, 0);

        // Conflict identified but NOT persisted to the state DB:
        assert!(
            engine.get_conflicts().await.unwrap().is_empty(),
            "dry run must not store conflicts"
        );
        let stored = engine.get_file_state("both.txt").await.unwrap().unwrap();
        assert_eq!(
            stored.couch_rev.map(|r| r.to_string()),
            Some("1-abc".to_string()),
            "dry run must not update local state"
        );
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "dry run must not write to CouchDB"
        );
    }

    #[tokio::test]
    async fn dry_run_does_not_save_state_for_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        // Local file has the same content as remote; only local tracking is stale.
        std::fs::write(root.as_path().join("same.txt"), "same-content").unwrap();
        let local = test_local_db();
        seed_file_state(&local, "same.txt", "stalehash", 12);
        let mut stored = local.get_file_state("same.txt").unwrap().unwrap();
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_path = "prefix/same.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 12);
        remote_doc.rev = Some("2-def".to_string());
        remote_doc.mtime = TimestampMillis::now();
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: vec![Change::remote_modified(
                remote_path.to_string(),
                "remotehash".to_string(),
                12,
                Utc::now(),
                "2-def".to_string(),
            )],
            last_seq: "901".to_string(),
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc.clone(),
            )]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                b"same-content".to_vec(),
            )]),
            content_errors: std::collections::HashSet::new(),
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync_dry_run().await.expect("dry run identical");

        assert_eq!(report.conflicts, 0, "identical content is not a conflict");
        assert_eq!(report.uploaded.0, 0);
        assert_eq!(report.downloaded.0, 0);

        // The silent-sync state update must also be skipped in dry-run:
        let stored = engine.get_file_state("same.txt").await.unwrap().unwrap();
        assert_eq!(
            stored.couch_rev.map(|r| r.to_string()),
            Some("1-abc".to_string()),
            "identical-content dry run must not update the local state"
        );
        assert!(engine.get_conflicts().await.unwrap().is_empty());
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "dry run must not write to CouchDB"
        );
    }

    // ── No-op sync skips identical state rewrites (#2946) ─────────────────

    #[tokio::test]
    async fn unchanged_tree_second_sync_skips_identical_file_state_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        let file_path = root.as_path().join("keep.txt");
        std::fs::write(&file_path, "unchanged content").unwrap();

        // Seed a fully-tracked, unchanged file: hash/size/mtime match the
        // on-disk scan, with a couch_rev and an old last_sync_at to preserve.
        let local = test_local_db();
        let hash = compute_file_hash(&file_path).unwrap();
        let meta = std::fs::metadata(&file_path).unwrap();
        let mut stored = FileState::new(
            "keep.txt".to_string(),
            hash.clone(),
            meta.len(),
            meta.modified().unwrap().into(),
        );
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();
        local.save_checkpoint("100-before").unwrap();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "200-after".to_string(),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        // First full sync: tracked-and-unchanged, so the preservation loop
        // writes the file state once.
        let report = engine.sync().await.expect("first sync");
        assert_eq!(report.uploaded.0, 0);
        assert_eq!(report.downloaded.0, 0);
        assert_eq!(report.conflicts, 0);
        let writes_after_first = engine.local_db.save_file_state_calls().await;
        assert!(
            writes_after_first >= 1,
            "first sync must preserve the unchanged file state"
        );

        // Second full sync over the exact same unchanged tree must not
        // rewrite the identical file_states row.
        let report2 = engine.sync().await.expect("second sync");
        assert_eq!(report2.uploaded.0, 0);
        assert_eq!(report2.downloaded.0, 0);
        assert_eq!(report2.conflicts, 0);
        let writes_after_second = engine.local_db.save_file_state_calls().await;
        assert_eq!(
            writes_after_second, writes_after_first,
            "a second sync of an unchanged tree must not rewrite identical file_states rows"
        );

        // Observables unchanged: couch_rev, last_sync_at, and hash are all
        // preserved across the repeated syncs.
        let final_state = engine.get_file_state("keep.txt").await.unwrap().unwrap();
        assert_eq!(
            final_state.couch_rev.map(|r| r.to_string()),
            Some("1-abc".to_string())
        );
        assert_eq!(final_state.last_sync_at, stored.last_sync_at);
        assert_eq!(final_state.hash, hash);
    }

    // ── Hash in-memory buffer vs. re-read from disk (#2904) ────────────────

    #[tokio::test]
    async fn bytes_hash_matches_file_hash_for_equivalent_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        let file_path = root.as_path().join("content.bin");

        // Representative content: empty, small text, binary, and content large
        // enough to span multiple 8 KiB reads inside compute_file_hash.
        let cases: Vec<&[u8]> = vec![
            b"",
            b"hello world",
            &[0u8, 1, 2, 3, 255, 128, 64][..],
            &[0x07u8; 20_000][..],
        ];
        for content in cases {
            std::fs::write(&file_path, content).unwrap();
            let bytes = std::fs::read(&file_path).unwrap();
            assert_eq!(
                compute_bytes_hash(&bytes),
                compute_file_hash(&file_path).unwrap(),
                "hash of a file and of its read bytes must match for {} bytes",
                content.len()
            );
        }
    }

    #[tokio::test]
    async fn upload_saved_hash_matches_transferred_content_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        let content: Vec<u8> = b"upload content bytes".to_vec();
        std::fs::write(root.as_path().join("up.txt"), &content).unwrap();

        let local = test_local_db();
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let (bytes_sent, _rev) = engine
            .upload_local_file("up.txt", "prefix/up.txt")
            .await
            .expect("upload should succeed");

        assert_eq!(bytes_sent, content.len(), "all content bytes uploaded");

        let state = engine
            .get_file_state("up.txt")
            .await
            .unwrap()
            .expect("upload should save a FileState");
        assert_eq!(
            state.hash,
            compute_bytes_hash(&content),
            "saved upload hash must equal the hash of the transferred content buffer"
        );
        assert_eq!(state.size, content.len() as u64);
    }

    #[tokio::test]
    async fn download_saved_hash_matches_transferred_content_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        let content: Vec<u8> = b"downloaded content bytes".to_vec();
        let remote_path = "prefix/dl.txt";

        let mut remote_doc =
            FileDoc::new(remote_path.to_string(), String::new(), content.len() as u64);
        remote_doc.rev = Some("1-abc".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::from([(remote_path.to_string(), content.clone())]),
            content_errors: std::collections::HashSet::new(),
        };
        let local = test_local_db();
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes_written = engine
            .download_remote_file(remote_path, "dl.txt", true)
            .await
            .expect("download should succeed")
            .expect("download should write content");

        assert_eq!(bytes_written, content.len(), "all content bytes written");

        let state = engine
            .get_file_state("dl.txt")
            .await
            .unwrap()
            .expect("download should save a FileState");
        assert_eq!(
            state.hash,
            compute_bytes_hash(&content),
            "saved download hash must equal the hash of the transferred content buffer"
        );
        assert_eq!(state.size, content.len() as u64);
    }

    #[tokio::test]
    async fn download_remote_file_content_fetch_error_propagates_without_writing() {
        // Regression for #2898: a failed content fetch must propagate as an
        // error instead of being swallowed into an empty-file write plus
        // synced state (which would silently truncate a valid local file).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();

        // Scenario A: existing valid local file with tracked state. The failed
        // content fetch must surface as an error and leave file + state intact.
        let remote_path = "prefix/err.txt";
        let local_path = "err.txt";
        std::fs::write(root.as_path().join(local_path), "original-content").unwrap();

        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 16);
        remote_doc.rev = Some("1-abc".to_string());
        remote_doc.path = remote_path.to_string();

        let local_a = test_local_db();
        let mut stored = FileState::new(
            local_path.to_string(),
            "originalhash".to_string(),
            16,
            Utc::now() - Duration::days(1),
        );
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local_a.save_file_state(&stored).unwrap();

        let canned_a = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::new(),
            content_errors: std::collections::HashSet::from([remote_path.to_string()]),
        };
        let mut engine_a = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned_a),
            local_a,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let err = engine_a
            .download_remote_file(remote_path, local_path, true)
            .await
            .expect_err("content fetch failure should propagate as Err");
        assert!(
            format!("{err:#}").contains("failed to fetch content"),
            "error should identify the failed content fetch: {err:#}"
        );
        assert_eq!(
            std::fs::read(root.as_path().join(local_path)).unwrap(),
            b"original-content",
            "failed content fetch must not truncate the existing local file"
        );
        let stored = engine_a
            .get_file_state(local_path)
            .await
            .unwrap()
            .expect("seeded state must still be present");
        assert_eq!(
            stored.couch_rev.map(|r| r.to_string()),
            Some("1-abc".to_string()),
            "failed content fetch must not update saved sync state"
        );

        // Scenario B: brand-new remote file with no local file or state. The
        // failed content fetch must not create an empty file nor save any
        // FileState.
        let remote_path2 = "prefix/new.txt";
        let local_path2 = "new.txt";
        let mut remote_doc2 = FileDoc::new(remote_path2.to_string(), String::new(), 3);
        remote_doc2.rev = Some("1-new".to_string());
        remote_doc2.path = remote_path2.to_string();

        let canned_b = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path2.to_string(), remote_doc2)]),
            contents: std::collections::HashMap::new(),
            content_errors: std::collections::HashSet::from([remote_path2.to_string()]),
        };
        let mut engine_b = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned_b),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        engine_b
            .download_remote_file(remote_path2, local_path2, true)
            .await
            .expect_err("content fetch failure should propagate as Err");
        assert!(
            !root.as_path().join(local_path2).exists(),
            "failed content fetch must not create an empty local file"
        );
        assert!(
            engine_b
                .get_file_state(local_path2)
                .await
                .unwrap()
                .is_none(),
            "failed content fetch must not save any FileState"
        );
    }

    #[tokio::test]
    async fn download_remote_file_writes_empty_file_for_empty_content_doc() {
        // Guard for #2898: a document with no children is legitimate empty
        // content, not a fetch failure. It must keep writing an empty local
        // file and saving synced state (behavior preserved from before the
        // error-propagation fix).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();

        let remote_path = "prefix/empty.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 0);
        remote_doc.rev = Some("1-empty".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            // No content entry: get_file_content returns Ok(empty), matching
            // the real path for a childless document.
            contents: std::collections::HashMap::new(),
            content_errors: std::collections::HashSet::new(),
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes = engine
            .download_remote_file(remote_path, "empty.txt", true)
            .await
            .expect("empty-content download should succeed")
            .expect("empty-content download should write a file");

        assert_eq!(bytes, 0, "empty-content download should write 0 bytes");
        assert_eq!(
            std::fs::read(root.as_path().join("empty.txt")).unwrap(),
            b"",
            "empty-content document must still produce an empty local file"
        );
        let stored = engine
            .get_file_state("empty.txt")
            .await
            .unwrap()
            .expect("empty-content download should save FileState");
        assert_eq!(
            stored.size, 0,
            "empty-content FileState should record size 0"
        );
    }

    // ── Regressions for moving blocking LocalDb (rusqlite) I/O off the
    // async executor (#2937) ─────────────────────────────────────────────

    /// get_conflicts() synthesizes `local_state.last_sync_at` with `Utc::now()`
    /// on every read (that field is not persisted in the conflicts table), so
    /// strip it from both sides before comparing retrieved conflicts.
    fn conflicts_without_volatile_last_sync(conflicts: &[Conflict]) -> serde_json::Value {
        let mut value = serde_json::to_value(conflicts).unwrap();
        if let serde_json::Value::Array(items) = &mut value {
            for item in items {
                if let Some(local_state) =
                    item.get_mut("local_state").and_then(|v| v.as_object_mut())
                {
                    local_state.remove("last_sync_at");
                }
            }
        }
        value
    }

    #[tokio::test]
    async fn async_local_db_returns_identical_results_to_direct_sqlite() {
        // Seed a real LocalDb with file states, a checkpoint, and a conflict,
        // then confirm every value read back through the AsyncLocalDb wrapper
        // (which runs each rusqlite call on tokio's blocking thread pool) is
        // identical to the direct synchronous SQLite API.
        let db = test_local_db();
        let mut state_a = FileState::new("a.txt".to_string(), "hash-a".to_string(), 3, Utc::now());
        state_a.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        db.save_file_state(&state_a).unwrap();
        let mut state_b = FileState::new("b.bin".to_string(), "hash-b".to_string(), 99, Utc::now());
        state_b.last_sync_at = Utc::now() - Duration::days(2);
        db.save_file_state(&state_b).unwrap();
        db.save_checkpoint("1024-seq").unwrap();

        let conflict = Conflict::new(
            "c.txt".to_string(),
            FileState::new("c.txt".to_string(), "local-hash".to_string(), 5, Utc::now()),
            RemoteState {
                hash: "remote-hash".to_string(),
                size: 6,
                modified_at: Utc::now() - Duration::hours(1),
                couch_rev: CouchRev::new("2-def").unwrap(),
                deleted: false,
            },
        );
        db.store_conflict(&conflict).unwrap();

        // Snapshot every converted path with the direct (blocking) API.
        let direct_states = db.get_all_file_states().unwrap();
        let direct_checkpoint = db.get_checkpoint().unwrap();
        let direct_conflicts = db.get_conflicts().unwrap();

        // Wrap the *same* database and re-read through the async wrapper.
        let async_db = AsyncLocalDb::new(db);
        let async_states = async_db.get_all_file_states().await.unwrap();
        let async_checkpoint = async_db.get_checkpoint().await.unwrap();
        let async_conflicts = async_db.get_conflicts().await.unwrap();

        // FileState/Conflict/Checkpoint are not PartialEq, so compare their
        // serialized forms for exact equality.
        assert_eq!(
            serde_json::to_value(&async_states).unwrap(),
            serde_json::to_value(&direct_states).unwrap(),
            "file-state retrieval must be identical after moving SQLite off the executor"
        );
        match (&async_checkpoint, &direct_checkpoint) {
            (Some(async_cp), Some(direct_cp)) => {
                assert_eq!(
                    async_cp.last_seq, direct_cp.last_seq,
                    "checkpoint last_seq must be identical after moving SQLite off the executor"
                );
                assert_eq!(
                    async_cp.last_sync_at, direct_cp.last_sync_at,
                    "checkpoint last_sync_at must be identical after moving SQLite off the executor"
                );
            }
            (None, None) => {}
            (a, d) => panic!(
                "checkpoint presence differs after moving SQLite off the executor: {a:?} vs {d:?}"
            ),
        }
        assert_eq!(
            conflicts_without_volatile_last_sync(&async_conflicts),
            conflicts_without_volatile_last_sync(&direct_conflicts),
            "conflict retrieval must be identical after moving SQLite off the executor"
        );

        // Individual lookups and writes through the async wrapper behave
        // exactly like the direct API.
        assert_eq!(
            serde_json::to_value(async_db.get_file_state("a.txt").await.unwrap()).unwrap(),
            serde_json::to_value(Some(state_a)).unwrap(),
        );
        let mut new_state =
            FileState::new("d.txt".to_string(), "hash-d".to_string(), 1, Utc::now());
        new_state.couch_rev = Some(CouchRev::new("3-ghi").unwrap());
        async_db.save_file_state(&new_state).await.unwrap();
        assert_eq!(
            serde_json::to_value(async_db.get_file_state("d.txt").await.unwrap()).unwrap(),
            serde_json::to_value(Some(new_state)).unwrap(),
        );
        async_db.delete_file_state("a.txt").await.unwrap();
        assert!(async_db.get_file_state("a.txt").await.unwrap().is_none());

        async_db.save_checkpoint("2048-seq").await.unwrap();
        assert_eq!(
            async_db.get_checkpoint().await.unwrap().unwrap().last_seq,
            "2048-seq"
        );
        async_db.delete_conflict("c.txt").await.unwrap();
        assert!(async_db.get_conflicts().await.unwrap().is_empty());
        async_db.reset_sync_state().await.unwrap();
        assert!(async_db.get_checkpoint().await.unwrap().is_none());
        assert!(async_db.get_all_file_states().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_engine_async_accessors_return_identical_state_after_blocking_move() {
        // Exercise the converted public accessors (get_file_state,
        // save_checkpoint, get_checkpoint, get_conflicts) end-to-end through a
        // SyncEngine: reads and writes go through spawn_blocking and must
        // return/update identical data.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        let local = test_local_db();
        local.save_checkpoint("42-seq").unwrap();
        let conflict = Conflict::new(
            "conflict.txt".to_string(),
            FileState::new(
                "conflict.txt".to_string(),
                "lhash".to_string(),
                4,
                Utc::now(),
            ),
            RemoteState {
                hash: "rhash".to_string(),
                size: 5,
                modified_at: Utc::now(),
                couch_rev: CouchRev::new("9-zzz").unwrap(),
                deleted: false,
            },
        );
        local.store_conflict(&conflict).unwrap();

        let engine = SyncEngine::new(test_couchdb(), local, root.clone());

        let cp = engine
            .get_checkpoint()
            .await
            .unwrap()
            .expect("seeded checkpoint");
        assert_eq!(
            cp.last_seq, "42-seq",
            "checkpoint read via engine must match the seeded value"
        );

        let conflicts = engine.get_conflicts().await.unwrap();
        assert_eq!(
            conflicts.len(),
            1,
            "seeded conflict must be readable via engine"
        );
        assert_eq!(conflicts[0].path, conflict.path);
        assert_eq!(conflicts[0].local_state.hash, conflict.local_state.hash);
        assert_eq!(conflicts[0].remote_state.hash, conflict.remote_state.hash);

        engine.save_checkpoint("43-seq").await.unwrap();
        assert_eq!(
            engine.get_checkpoint().await.unwrap().unwrap().last_seq,
            "43-seq",
            "checkpoint write via engine must be read back unchanged"
        );
        assert!(engine
            .get_file_state("nonexistent.txt")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn remote_delete_between_syncs_is_applied_locally() {
        // Regression for the ignored live-CouchDB integration test
        // remote_move_deletes_old_local_path: a remote delete that arrives
        // after the previous sync must remove the local file. This used to be
        // skipped because scan_local_changes overwrote the stored last_sync_at
        // with the scan time, making any remote change between two syncs look
        // stale (remote mtime < advanced last_sync_at) and never get applied.
        // Use a unique per-run temp dir so concurrent test processes never
        // race on a shared state DB; TempDir removes the dir (state DB
        // included) when the test ends.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path().to_path_buf()).unwrap();
        std::fs::write(root.as_path().join("a.txt"), "hello\n").unwrap();
        let state_db = root.as_path().join("state.db");

        // Sync 1 uploads a.txt to an empty remote and records last_sync_at.
        let mut engine1 = SyncEngine::new(
            test_canned_couch("prefix/", CannedCouch::default()),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        );
        engine1.sync().await.unwrap();
        let last_sync = engine1
            .get_file_state("a.txt")
            .await
            .unwrap()
            .expect("a.txt state after first sync")
            .last_sync_at;
        assert!(root.as_path().join("a.txt").exists());

        // The remote delete lands just after the first sync (newer than our
        // last sync) but before this second sync runs.
        let remote_delete = Change::remote_deleted(
            "prefix/a.txt".to_string(),
            Some(last_sync + Duration::microseconds(1)),
        );
        let canned = CannedCouch {
            changes: vec![remote_delete],
            last_seq: "2".to_string(),
            ..CannedCouch::default()
        };
        let mut engine2 = SyncEngine::new(
            test_canned_couch("prefix/", canned),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        );
        let report = engine2.sync().await.unwrap();

        assert_eq!(
            report.deleted_local, 1,
            "a remote delete newer than the last sync must remove the local file"
        );
        assert!(
            !root.as_path().join("a.txt").exists(),
            "local a.txt must be removed after the remote delete"
        );
    }
}
