use crate::couchdb::CouchDb;
use crate::local::{compute_bytes_hash, compute_file_hash, LocalDb, Scanner};
use crate::models::{
    Change, ChangeType, Conflict, CouchRev, FileState, IgnoreMatcher, RemoteState,
    ResolutionStrategy,
};
use crate::sync::triage;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

/// The main sync engine
pub struct SyncEngine {
    couchdb: CouchDb,
    local_db: LocalDb,
    root_dir: PathBuf,
    ignore_matcher: IgnoreMatcher,
}

/// Report from a sync operation
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub conflicts: usize,
    pub errors: Vec<String>,
}

impl SyncEngine {
    /// Create a new sync engine
    pub fn new(couchdb: CouchDb, local_db: LocalDb, root_dir: PathBuf) -> Self {
        Self::with_ignore(couchdb, local_db, root_dir, IgnoreMatcher::empty())
    }

    /// Create a new sync engine with ignore patterns applied to full scans.
    pub fn with_ignore(
        couchdb: CouchDb,
        local_db: LocalDb,
        root_dir: PathBuf,
        ignore_matcher: IgnoreMatcher,
    ) -> Self {
        Self {
            couchdb,
            local_db,
            root_dir,
            ignore_matcher,
        }
    }

    /// Perform a full sync cycle
    pub async fn sync(&mut self) -> Result<SyncReport> {
        info!("========== SYNC CYCLE STARTING ==========");
        let mut report = SyncReport::default();

        // 1. Scan local changes
        let local_changes = self.scan_local_changes().await?;
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
            .detect_conflicts(&local_changes, &remote_changes)
            .await?;

        report.conflicts = conflicts.len();

        info!("After analysis:");
        info!("  - Files to upload: {}", local_to_upload.len());
        info!("  - Files to download: {}", remote_to_apply.len());
        info!("  - Conflicts: {}", conflicts.len());

        // 4. Store conflicts
        for conflict in conflicts {
            info!("CONFLICT: {}", conflict.path);
            self.local_db.store_conflict(&conflict)?;
        }

        // 5. Apply clean local changes to remote
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
            match self.apply_to_couchdb(&change).await {
                Ok(_) => {
                    if matches!(change.change_type(), ChangeType::Deleted) {
                        report.deleted_remote += 1;
                        self.local_db.delete_file_state(change.path())?;
                    } else {
                        report.uploaded += 1;
                    }
                }
                Err(e) => {
                    error!("Failed to upload {}: {}", change.path(), e);
                    report
                        .errors
                        .push(format!("Upload {}: {}", change.path(), e));
                }
            }
        }

        // 6. Apply clean remote changes to local
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
            match self.apply_to_filesystem(&change).await {
                Ok(_) => {
                    if matches!(change.change_type(), ChangeType::Deleted) {
                        report.deleted_local += 1;
                    } else {
                        report.downloaded += 1;
                    }
                }
                Err(e) => {
                    error!("Failed to download {}: {}", change.path(), e);
                    report
                        .errors
                        .push(format!("Download {}: {}", change.path(), e));
                }
            }
        }

        // 7. Update checkpoint
        self.local_db.save_checkpoint(&last_seq)?;

        info!(
            "========== SYNC COMPLETE: {} uploaded, {} downloaded, {} conflicts ==========",
            report.uploaded, report.downloaded, report.conflicts
        );

        Ok(report)
    }

    /// Rebuild the remote scope so it exactly matches the local filesystem.
    pub async fn rebuild_remote_from_local(&mut self) -> Result<SyncReport> {
        info!("========== REMOTE REBUILD STARTING ==========");

        let scanner = Scanner::new(self.root_dir.clone(), self.ignore_matcher.clone());
        let local_states = scanner.full_scan()?;
        let remote_docs = self.couchdb.get_all_files().await?;
        let (uploads, remote_deletes) =
            triage::plan_remote_rebuild(&local_states, &remote_docs, self.couchdb.remote_prefix());

        self.local_db.reset_sync_state()?;

        let mut report = SyncReport::default();

        for local_path in uploads {
            let remote_path = self.couchdb.get_remote_path(&local_path);
            self.upload_local_file(&local_path, &remote_path).await?;
            report.uploaded += 1;
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

        let scanner = Scanner::new(self.root_dir.clone(), self.ignore_matcher.clone());
        let local_states = scanner.full_scan()?;
        let remote_docs = self.couchdb.get_all_files().await?;
        let (local_deletes, remote_downloads) =
            triage::plan_local_rebuild(&local_states, &remote_docs);

        self.local_db.reset_sync_state()?;

        let mut report = SyncReport::default();

        for local_path in local_deletes {
            let file_path = self.root_dir.join(&local_path);
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
                report.downloaded += 1;
            }
        }

        info!(
            "========== LOCAL REBUILD COMPLETE: {} deleted locally, {} downloaded ==========",
            report.deleted_local, report.downloaded
        );

        Ok(report)
    }

    /// Scan for local changes
    async fn scan_local_changes(&self) -> Result<Vec<Change>> {
        let scanner = Scanner::new(self.root_dir.clone(), self.ignore_matcher.clone());
        let current_states = scanner.full_scan()?;
        let stored_states = self.local_db.get_all_file_states()?;
        let remote_prefix = self.couchdb.remote_prefix();
        let mut valid_stored_states = Vec::with_capacity(stored_states.len());

        for state in stored_states {
            if triage::is_polluted_state_path(&state.path, remote_prefix) {
                warn!(
                    "Removing invalid state entry for {}: local state includes remote prefix {}",
                    state.path, remote_prefix
                );
                self.local_db.delete_file_state(&state.path)?;
            } else if self
                .ignore_matcher
                .should_ignore(std::path::Path::new(&state.path))
            {
                info!(
                    "Removing ignored state entry from local database: {}",
                    state.path
                );
                self.local_db.delete_file_state(&state.path)?;
            } else {
                valid_stored_states.push(state);
            }
        }

        debug!("Scanned {} files on disk", current_states.len());
        debug!(
            "Found {} files in local database",
            valid_stored_states.len()
        );

        let changes = scanner.detect_changes(&current_states, &valid_stored_states);

        debug!("Detected {} changes from local scan", changes.len());
        for change in &changes {
            debug!(
                "  Local change: {} ({:?})",
                change.path(),
                change.change_type()
            );
        }

        // Build a map of stored states to preserve couch_rev
        let stored_map: HashMap<_, _> = valid_stored_states.iter().map(|s| (&s.path, s)).collect();

        // Only update stored states for files that haven't changed
        // (new and modified files will be updated after successful sync)
        for state in &current_states {
            // Check if this file is in the changes list
            let is_changed = changes.iter().any(|c| c.path() == state.path);
            if !is_changed {
                // File unchanged - preserve the couch_rev from stored state
                let couch_rev = stored_map
                    .get(&state.path)
                    .and_then(|s| s.couch_rev.clone());
                let preserved_state = FileState {
                    path: state.path.clone(),
                    hash: state.hash.clone(),
                    size: state.size,
                    modified_at: state.modified_at,
                    couch_rev,
                    last_sync_at: state.last_sync_at,
                };
                self.local_db.save_file_state(&preserved_state)?;
            }
        }

        Ok(changes)
    }

    /// Fetch remote changes from CouchDB
    async fn fetch_remote_changes(&self) -> Result<(Vec<Change>, String)> {
        let checkpoint = self.local_db.get_checkpoint()?;
        let since = checkpoint.map(|(seq, _)| seq);

        self.couchdb.get_changes(since.as_deref()).await
    }

    /// Detect conflicts between local and remote changes
    async fn detect_conflicts(
        &self,
        local_changes: &[Change],
        remote_changes: &[Change],
    ) -> Result<(Vec<Change>, Vec<Change>, Vec<Conflict>)> {
        // Build a complete map of stored states (one I/O batch)
        let mut stored_states: HashMap<String, FileState> = HashMap::new();
        // Collect all paths we need state for
        let mut paths_to_lookup: Vec<&str> = Vec::new();
        for lc in local_changes {
            if !paths_to_lookup.contains(&lc.path()) {
                paths_to_lookup.push(lc.path());
            }
        }
        for rc in remote_changes {
            let local_path = self.couchdb.get_local_path(rc.path());
            if !paths_to_lookup.contains(&local_path.as_str()) {
                // Need to clone to satisfy borrow checker; collect then lookup
            }
        }
        // Actually load all stored states individually (keeps existing pattern)
        for lc in local_changes {
            if !stored_states.contains_key(lc.path()) {
                if let Some(state) = self.local_db.get_file_state(lc.path())? {
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
                if let Some(state) = self.local_db.get_file_state(&local_path)? {
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
                // Update local state to reflect remote rev
                let updated_state = FileState {
                    path: lc.path().to_string(),
                    hash: local_state.hash,
                    size: local_state.size,
                    modified_at: local_state.modified_at,
                    couch_rev: remote_doc.rev.as_deref().and_then(CouchRev::new),
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&updated_state)?;
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
                    couch_rev: CouchRev::new(remote_doc.rev.as_deref().unwrap_or("1-"))
                        .unwrap_or(CouchRev::new("1-").unwrap()),
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
            let file_path = self.root_dir.join(relative_path);
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
        let file_path = self.root_dir.join(relative_path);
        let hash = compute_file_hash(&file_path).map_err(|e| {
            anyhow::anyhow!("Failed to compute hash for {}: {}", file_path.display(), e)
        })?;
        let metadata = std::fs::metadata(&file_path).map_err(|e| {
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
        let file_path = self.root_dir.join(relative_path);
        let metadata = std::fs::metadata(&file_path)?;
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

        let hash = compute_file_hash(&file_path)?;
        let state = FileState {
            path: local_path.to_string(),
            hash,
            size: metadata.len(),
            modified_at: metadata.modified()?.into(),
            couch_rev: doc.rev.as_deref().and_then(CouchRev::new),
            last_sync_at: Utc::now(),
        };
        self.local_db.save_file_state(&state)?;
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
        let file_path = self.root_dir.join(relative_path);
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
                debug!("No content for {}: {}, using empty file", remote_path, e);
                Vec::new()
            }
        };

        tokio::fs::write(&file_path, &content).await?;
        debug!("[DOWNLOAD] Wrote {} bytes to disk", content.len());

        let hash = compute_file_hash(&file_path)?;
        let metadata = std::fs::metadata(&file_path)?;
        let state = FileState {
            path: local_path.to_string(),
            hash,
            size: metadata.len(),
            modified_at: metadata.modified()?.into(),
            couch_rev: doc.rev.as_deref().and_then(CouchRev::new),
            last_sync_at: Utc::now(),
        };
        self.local_db.save_file_state(&state)?;

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
                self.local_db.delete_file_state(change.path())?;
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
        let file_path = self.root_dir.join(relative_path);

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
                self.local_db.delete_file_state(&local_path)?;
                info!("[LOCAL DELETE] SUCCESS: {} -> {}", remote_path, local_path);
            }
        }
        Ok(())
    }

    /// Get list of conflicts
    pub fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        self.local_db.get_conflicts()
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
    pub fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        self.local_db.get_file_state(path)
    }

    /// Save sync checkpoint
    pub fn save_checkpoint(&self, seq: &str) -> Result<()> {
        self.local_db.save_checkpoint(seq)
    }

    /// Get sync checkpoint
    pub fn get_checkpoint(&self) -> Result<Option<(String, chrono::DateTime<chrono::Utc>)>> {
        self.local_db.get_checkpoint()
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
    pub fn root_dir(&self) -> &PathBuf {
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
        let _conflict = match self.local_db.get_conflict(local_path)? {
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
        self.local_db.delete_conflict(local_path)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::couchdb::CouchDb;
    use crate::local::LocalDb;
    use crate::models::IgnoreMatcher;
    use std::path::PathBuf;

    /// Create a minimal CouchDb instance for testing construction.
    /// Not all methods work without a real server.
    fn test_couchdb() -> CouchDb {
        CouchDb::for_test("test-prefix/")
    }

    fn test_local_db() -> LocalDb {
        LocalDb::open_in_memory().expect("in-memory LocalDb should construct")
    }

    #[test]
    fn engine_new_constructs_with_empty_ignore_matcher() {
        let couch = test_couchdb();
        let local = test_local_db();
        let root = PathBuf::from("/tmp/test-sync-engine-new");

        let engine = SyncEngine::new(couch, local, root.clone());

        assert_eq!(engine.root_dir(), &root);
        assert!(engine.get_conflicts().unwrap().is_empty());
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
        let root = PathBuf::from("/tmp/test-sync-engine-with-ignore");

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
        let root = PathBuf::from("/tmp/test-sync-engine-root");
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

    #[test]
    fn engine_initial_state_has_no_conflicts_and_no_file_states() {
        let engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            PathBuf::from("/tmp/test-sync-engine-state"),
        );

        // Fresh engine should have no conflicts
        assert!(engine.get_conflicts().unwrap().is_empty());

        // Fresh engine should have no file states
        assert!(engine.get_file_state("nonexistent.txt").unwrap().is_none());
        assert!(engine.get_file_state("other/path.md").unwrap().is_none());
    }

    #[test]
    fn engine_with_ignore_can_checkpoint() {
        let engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            PathBuf::from("/tmp/test-sync-engine-checkpoint"),
        );

        // Save a checkpoint
        engine.save_checkpoint("123-abc").unwrap();

        // Read it back
        let cp = engine.get_checkpoint().unwrap();
        assert!(cp.is_some());
        assert_eq!(cp.unwrap().0, "123-abc");
    }

    #[test]
    fn engine_path_conversion_methods_use_couchdb_prefix() {
        let engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            PathBuf::from("/tmp/test-sync-engine-paths"),
        );

        // local_to_remote_path should prepend the prefix
        let remote = engine.local_to_remote_path("doc.txt");
        assert_eq!(remote, "test-prefix/doc.txt");

        // remote_to_local_path should strip the prefix
        let local = engine.remote_to_local_path("test-prefix/doc.txt");
        assert_eq!(local, "doc.txt");
    }
}
