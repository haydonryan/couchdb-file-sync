use crate::couchdb::CouchDb;
use crate::local::{compute_bytes_hash, compute_file_hash, LocalDb, Scanner};
use crate::models::{
    Change, ChangeType, Conflict, FileDoc, FileState, IgnoreMatcher, RemoteState,
    ResolutionStrategy,
};
use anyhow::Result;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
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
            debug!("  [LOCAL] {} ({:?})", change.path, change.change_type);
        }

        // 2. Get remote changes
        let (remote_changes, last_seq) = self.fetch_remote_changes().await?;
        info!("Remote files fetched: {}", remote_changes.len());
        for change in &remote_changes {
            debug!(
                "  [REMOTE] {} - rev: {:?}, mtime: {:?}",
                change.path, change.rev, change.mtime
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
                change.path,
                self.couchdb.get_remote_path(&change.path)
            );
            match self.apply_to_couchdb(&change).await {
                Ok(_) => {
                    if matches!(change.change_type, ChangeType::Deleted) {
                        report.deleted_remote += 1;
                        self.local_db.delete_file_state(&change.path)?;
                    } else {
                        report.uploaded += 1;
                    }
                }
                Err(e) => {
                    error!("Failed to upload {}: {}", change.path, e);
                    report.errors.push(format!("Upload {}: {}", change.path, e));
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
                change.path,
                self.couchdb.get_local_path(&change.path)
            );
            match self.apply_to_filesystem(&change).await {
                Ok(_) => {
                    if matches!(change.change_type, ChangeType::Deleted) {
                        report.deleted_local += 1;
                    } else {
                        report.downloaded += 1;
                    }
                }
                Err(e) => {
                    error!("Failed to download {}: {}", change.path, e);
                    report
                        .errors
                        .push(format!("Download {}: {}", change.path, e));
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
            plan_remote_rebuild(&local_states, &remote_docs, self.couchdb.remote_prefix());

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
        let (local_deletes, remote_downloads) = plan_local_rebuild(&local_states, &remote_docs);

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
            if is_polluted_state_path(&state.path, remote_prefix) {
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
            debug!("  Local change: {} ({:?})", change.path, change.change_type);
        }

        // Build a map of stored states to preserve couch_rev
        let stored_map: HashMap<_, _> = valid_stored_states.iter().map(|s| (&s.path, s)).collect();

        // Only update stored states for files that haven't changed
        // (new and modified files will be updated after successful sync)
        for state in &current_states {
            // Check if this file is in the changes list
            let is_changed = changes.iter().any(|c| c.path == state.path);
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
        let local_map: HashMap<_, _> = local_changes.iter().map(|c| (&c.path, c)).collect();
        let remote_map: HashMap<_, _> = remote_changes.iter().map(|c| (&c.path, c)).collect();

        let mut local_to_upload = Vec::new();
        let mut remote_to_apply = Vec::new();
        let mut conflicts = Vec::new();

        debug!(
            "========== ANALYZING {} LOCAL CHANGES ==========",
            local_changes.len()
        );
        for lc in local_changes {
            let remote_path = self.couchdb.get_remote_path(&lc.path);
            debug!("--- LOCAL CHANGE: {} ---", lc.path);
            debug!("  Local path: {}", lc.path);
            debug!("  Remote path: {}", remote_path);
            debug!("  Change type: {:?}", lc.change_type);

            if lc.change_type == ChangeType::Deleted {
                debug!("  Local delete, skipping remote content comparison");
                local_to_upload.push(lc.clone());
                debug!("");
                continue;
            }

            let stored_state = self.local_db.get_file_state(&lc.path)?;
            if let Some(ref state) = stored_state {
                debug!("  STORED STATE:");
                debug!("    hash: {}...", &state.hash[..8.min(state.hash.len())]);
                debug!("    size: {} bytes", state.size);
                debug!("    modified_at: {:?}", state.modified_at);
                debug!("    couch_rev: {:?}", state.couch_rev);
                debug!("    last_sync_at: {:?}", state.last_sync_at);
            } else {
                debug!("  NO STORED STATE (first time sync)");
            }

            let remote_changed = if let Some(rc) = remote_map.get(&remote_path) {
                match (&rc.mtime, &stored_state) {
                    (Some(remote_mtime), Some(state)) => {
                        let changed = *remote_mtime > state.last_sync_at;
                        if changed {
                            info!("  [REMOTE CHANGE DETECTED] {}", lc.path);
                            info!(
                                "    Remote mtime: {} | Last sync: {} | Diff: +{}s",
                                remote_mtime.format("%Y-%m-%d %H:%M:%S"),
                                state.last_sync_at.format("%Y-%m-%d %H:%M:%S"),
                                (*remote_mtime - state.last_sync_at).num_seconds()
                            );
                            if let Some(remote_rev) = &rc.rev {
                                let stored_rev = state.couch_rev.as_deref().unwrap_or("none");
                                info!(
                                    "    Remote rev: {} | Stored rev: {}",
                                    &remote_rev[..12.min(remote_rev.len())],
                                    &stored_rev[..12.min(stored_rev.len())]
                                );
                            }
                            if let Some(remote_size) = rc.size {
                                info!(
                                    "    Remote size: {} bytes | Local size: {} bytes",
                                    remote_size, state.size
                                );
                            }
                        } else {
                            debug!(
                                "    {} - remote_mtime ({}) <= last_sync_at ({}), no remote change",
                                lc.path,
                                remote_mtime.format("%Y-%m-%d %H:%M:%S"),
                                state.last_sync_at.format("%Y-%m-%d %H:%M:%S")
                            );
                        }
                        changed
                    }
                    (None, _) => {
                        info!("  [REMOTE CHANGE DETECTED] {} - no remote mtime available, assuming changed", lc.path);
                        if let Some(ref state) = stored_state {
                            info!(
                                "    Stored rev: {:?} | Remote rev: {:?}",
                                state.couch_rev, rc.rev
                            );
                        }
                        true
                    }
                    (_, None) => {
                        info!(
                            "  [REMOTE CHANGE DETECTED] {} - no stored state (first sync)",
                            lc.path
                        );
                        info!(
                            "    Remote mtime: {:?} | Remote rev: {:?} | Remote size: {:?}",
                            rc.mtime, rc.rev, rc.size
                        );
                        true
                    }
                }
            } else {
                debug!("  {} - file not on remote yet", lc.path);
                false
            };

            if remote_changed {
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
                let local_state = self.get_local_state(&lc.path).await?;
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
                        lc.path,
                        &local_state.hash[..8.min(local_state.hash.len())]
                    );
                    // Update local state to reflect remote rev
                    let updated_state = FileState {
                        path: lc.path.clone(),
                        hash: local_state.hash,
                        size: local_state.size,
                        modified_at: local_state.modified_at,
                        couch_rev: remote_doc.rev,
                        last_sync_at: Utc::now(),
                    };
                    self.local_db.save_file_state(&updated_state)?;
                } else {
                    info!(
                        "  [CONFLICT] {} - content differs (local: {}, remote: {})",
                        lc.path,
                        &local_state.hash[..8.min(local_state.hash.len())],
                        &remote_hash[..8.min(remote_hash.len())]
                    );

                    // Convert mtime (milliseconds since epoch) to DateTime
                    use chrono::TimeZone;
                    let remote_modified_at = Utc
                        .timestamp_millis_opt(remote_doc.mtime as i64)
                        .single()
                        .unwrap_or_else(Utc::now);

                    let remote_state = RemoteState {
                        path: remote_path.clone(),
                        hash: remote_hash,
                        size: remote_content.len() as u64,
                        modified_at: remote_modified_at,
                        couch_rev: remote_doc.rev.unwrap_or_default(),
                        deleted: remote_doc.deleted,
                    };
                    debug!("  REMOTE STATE:");
                    debug!("    size: {} bytes", remote_state.size);
                    debug!("    modified_at: {:?}", remote_state.modified_at);
                    debug!("    couch_rev: {:?}", remote_state.couch_rev);

                    conflicts.push(Conflict::new(lc.path.clone(), local_state, remote_state));
                }
            } else {
                info!(
                    "  [UPLOAD] {} - remote unchanged, queuing for upload",
                    lc.path
                );
                local_to_upload.push(lc.clone());
            }
            debug!("");
        }

        debug!(
            "========== CHECKING {} REMOTE FILES ==========",
            remote_changes.len()
        );
        for rc in remote_changes {
            let local_path = self.couchdb.get_local_path(&rc.path);
            debug!("--- REMOTE FILE: {} -> {} ---", rc.path, local_path);
            debug!(
                "  mtime: {:?}, rev: {:?}, size: {:?}",
                rc.mtime, rc.rev, rc.size
            );

            if local_map.contains_key(&local_path) {
                debug!("  File also changed locally, skipping (handled above)");
                continue;
            }

            if rc.change_type == ChangeType::Deleted {
                let relative_path = local_path.trim_start_matches('/');
                let file_path = self.root_dir.join(relative_path);
                let stored_state = self.local_db.get_file_state(&local_path)?;
                if should_apply_remote_delete(stored_state.as_ref(), rc.mtime, file_path.exists()) {
                    debug!("  Remote delete is newer than last sync, scheduling local delete");
                    remote_to_apply.push(rc.clone());
                } else if file_path.exists() || stored_state.is_some() {
                    debug!("  Remote delete is stale or file is untracked locally, skipping");
                } else {
                    debug!("  Remote deleted, no local file/state, skipping");
                }
                debug!("");
                continue;
            }

            let should_download = match self.local_db.get_file_state(&local_path)? {
                Some(state) => {
                    debug!("  LOCAL STATE EXISTS:");
                    debug!("    hash: {}...", &state.hash[..8.min(state.hash.len())]);
                    debug!("    size: {} bytes", state.size);
                    debug!("    modified_at: {:?}", state.modified_at);
                    debug!("    couch_rev: {:?}", state.couch_rev);
                    debug!("    last_sync_at: {:?}", state.last_sync_at);

                    match (rc.rev.as_ref(), state.couch_rev.as_ref()) {
                        (Some(remote_rev), Some(stored_rev)) => {
                            let changed = remote_rev != stored_rev;
                            debug!("  REVISION COMPARE:");
                            debug!("    stored_rev: {:?}", stored_rev);
                            debug!("    remote_rev: {:?}", remote_rev);
                            debug!("    changed: {}", changed);
                            changed
                        }
                        (Some(_), None) => {
                            debug!("  No stored rev, treating as new file");
                            true
                        }
                        (None, Some(_)) => {
                            debug!("  No remote rev, skipping");
                            false
                        }
                        (None, None) => {
                            debug!("  No revs at all, skipping");
                            false
                        }
                    }
                }
                None => {
                    debug!("  NO LOCAL STATE");
                    let relative_path = local_path.trim_start_matches('/');
                    let file_path = self.root_dir.join(relative_path);
                    if file_path.exists() {
                        debug!("  File exists on disk but not tracked, skipping");
                        false
                    } else {
                        debug!("  New file on remote, will download");
                        true
                    }
                }
            };

            if should_download {
                debug!("  => Remote is newer, will download chunked file");
                remote_to_apply.push(rc.clone());
            } else {
                debug!("  => Skipping (already in sync)");
            }
            debug!("");
        }

        debug!("========== ANALYSIS COMPLETE ==========");
        debug!("  Uploads queued: {}", local_to_upload.len());
        debug!("  Downloads queued: {}", remote_to_apply.len());
        debug!("  Conflicts found: {}", conflicts.len());

        Ok((local_to_upload, remote_to_apply, conflicts))
    }

    /// Get local file state
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
                    debug!("[UPLOAD] Existing ctime: {} ms", existing.ctime);
                    debug!("[UPLOAD] Existing chunks: {}", existing.children.len());
                    (existing.rev, existing.ctime, existing.children)
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
            ctime: existing_ctime,
            mtime,
            size: metadata.len(),
            doc_type: "plain".to_string(),
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
            couch_rev: doc.rev.clone(),
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
            couch_rev: doc.rev.clone(),
            last_sync_at: Utc::now(),
        };
        self.local_db.save_file_state(&state)?;

        Ok(Some(content.len()))
    }

    /// Apply a change to CouchDB
    async fn apply_to_couchdb(&mut self, change: &Change) -> Result<()> {
        let remote_path = self.couchdb.get_remote_path(&change.path);
        debug!("[UPLOAD] Starting: {} -> {}", change.path, remote_path);
        debug!("[UPLOAD] Change type: {:?}", change.change_type);

        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                let (bytes_uploaded, new_rev) =
                    self.upload_local_file(&change.path, &remote_path).await?;
                info!(
                    "[UPLOAD] SUCCESS: {} -> {} ({} bytes, rev: {:?})",
                    change.path, remote_path, bytes_uploaded, new_rev
                );
            }
            ChangeType::Deleted => {
                debug!("[DELETE] Remote: {}", remote_path);
                self.couchdb.delete_file(&remote_path).await?;
                self.local_db.delete_file_state(&change.path)?;
                info!("[DELETE] SUCCESS: {} -> {}", change.path, remote_path);
            }
        }
        Ok(())
    }

    /// Apply a change to the local filesystem
    async fn apply_to_filesystem(&mut self, change: &Change) -> Result<()> {
        let remote_path = change.path.as_str();
        let local_path = self.couchdb.get_local_path(remote_path);
        let relative_path = local_path.trim_start_matches('/');
        let file_path = self.root_dir.join(relative_path);

        debug!("[DOWNLOAD] Remote is newer, downloading chunked file");
        debug!("[DOWNLOAD] {} -> {}", remote_path, local_path);

        match change.change_type {
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

fn should_apply_remote_delete(
    stored_state: Option<&FileState>,
    remote_mtime: Option<chrono::DateTime<Utc>>,
    _file_exists: bool,
) -> bool {
    match stored_state {
        Some(state) => match remote_mtime {
            Some(remote_mtime) => remote_mtime > state.last_sync_at,
            None => true,
        },
        None => false,
    }
}

fn is_polluted_state_path(path: &str, remote_prefix: &str) -> bool {
    let remote_prefix = remote_prefix.trim_end_matches('/');
    !remote_prefix.is_empty()
        && (path == remote_prefix || path.starts_with(&format!("{remote_prefix}/")))
}

fn plan_remote_rebuild(
    local_states: &[FileState],
    remote_docs: &[FileDoc],
    remote_prefix: &str,
) -> (Vec<String>, Vec<String>) {
    let local_paths: HashSet<_> = local_states
        .iter()
        .map(|state| state.path.as_str())
        .collect();
    let uploads = local_states
        .iter()
        .map(|state| state.path.clone())
        .collect::<Vec<_>>();
    let remote_deletes = remote_docs
        .iter()
        .filter(|doc| !doc.deleted)
        .filter_map(|doc| {
            let local_path = remote_path_to_local_path(&doc.id, remote_prefix);
            (!local_paths.contains(local_path.as_str())).then(|| doc.id.clone())
        })
        .collect::<Vec<_>>();

    (uploads, remote_deletes)
}

fn plan_local_rebuild(
    local_states: &[FileState],
    remote_docs: &[FileDoc],
) -> (Vec<String>, Vec<String>) {
    let local_deletes = local_states
        .iter()
        .map(|state| state.path.clone())
        .collect::<Vec<_>>();
    let remote_downloads = remote_docs
        .iter()
        .filter(|doc| !doc.deleted)
        .map(|doc| doc.id.clone())
        .collect::<Vec<_>>();

    (local_deletes, remote_downloads)
}

fn remote_path_to_local_path(remote_path: &str, remote_prefix: &str) -> String {
    if remote_prefix.is_empty() {
        remote_path.to_string()
    } else {
        remote_path
            .strip_prefix(remote_prefix)
            .unwrap_or(remote_path)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_polluted_state_path, plan_local_rebuild, plan_remote_rebuild, remote_path_to_local_path,
        should_apply_remote_delete,
    };
    use crate::models::{FileDoc, FileState, IgnoreMatcher};
    use chrono::{Duration, TimeZone, Utc};

    fn file_state(last_sync_at: chrono::DateTime<Utc>) -> FileState {
        FileState {
            path: "example.txt".to_string(),
            hash: "abc".to_string(),
            size: 1,
            modified_at: last_sync_at,
            couch_rev: Some("1-abc".to_string()),
            last_sync_at,
        }
    }

    #[test]
    fn stale_remote_delete_does_not_remove_tracked_local_file() {
        let last_sync = Utc.with_ymd_and_hms(2026, 3, 23, 17, 12, 33).unwrap();
        let remote_delete = last_sync - Duration::days(6);

        assert!(!should_apply_remote_delete(
            Some(&file_state(last_sync)),
            Some(remote_delete),
            true,
        ));
    }

    #[test]
    fn newer_remote_delete_removes_tracked_local_file() {
        let last_sync = Utc.with_ymd_and_hms(2026, 3, 23, 17, 12, 33).unwrap();
        let remote_delete = last_sync + Duration::seconds(1);

        assert!(should_apply_remote_delete(
            Some(&file_state(last_sync)),
            Some(remote_delete),
            true,
        ));
    }

    #[test]
    fn untracked_existing_local_file_is_not_deleted() {
        let remote_delete = Utc.with_ymd_and_hms(2026, 3, 23, 17, 12, 33).unwrap();

        assert!(!should_apply_remote_delete(None, Some(remote_delete), true));
    }

    #[test]
    fn detects_state_entries_polluted_with_remote_prefix() {
        assert!(is_polluted_state_path(
            "Agents/ross-coulthart/AGENT.md",
            "Agents/"
        ));
        assert!(is_polluted_state_path("Agents", "Agents/"));
        assert!(!is_polluted_state_path(
            "ross-coulthart/AGENT.md",
            "Agents/"
        ));
        assert!(!is_polluted_state_path("Agentsmith/profile.md", "Agents/"));
    }

    #[test]
    fn ignore_matcher_marks_previously_tracked_ignored_files() {
        let matcher = IgnoreMatcher::from_content("auth-profiles.json");

        assert!(matcher.should_ignore(std::path::Path::new("ross-coulthart/auth-profiles.json")));
        assert!(!matcher.should_ignore(std::path::Path::new("ross-coulthart/AGENT.md")));
    }

    #[test]
    fn remote_rebuild_uploads_local_files_and_deletes_remote_orphans() {
        let local_states = vec![
            FileState::new(
                "notes/a.md".to_string(),
                "hash-a".to_string(),
                10,
                Utc::now(),
            ),
            FileState::new(
                "notes/b.md".to_string(),
                "hash-b".to_string(),
                20,
                Utc::now(),
            ),
        ];
        let remote_docs = vec![
            FileDoc::new("mirror/notes/a.md".to_string(), String::new(), 10),
            FileDoc::new("mirror/notes/orphan.md".to_string(), String::new(), 30),
            FileDoc {
                deleted: true,
                ..FileDoc::new("mirror/notes/deleted.md".to_string(), String::new(), 0)
            },
        ];

        let (uploads, remote_deletes) = plan_remote_rebuild(&local_states, &remote_docs, "mirror/");

        assert_eq!(
            uploads,
            vec!["notes/a.md".to_string(), "notes/b.md".to_string()]
        );
        assert_eq!(remote_deletes, vec!["mirror/notes/orphan.md".to_string()]);
    }

    #[test]
    fn local_rebuild_deletes_local_files_and_downloads_live_remote_docs() {
        let local_states = vec![
            FileState::new(
                "notes/old.md".to_string(),
                "hash-old".to_string(),
                10,
                Utc::now(),
            ),
            FileState::new(
                "notes/extra.md".to_string(),
                "hash-extra".to_string(),
                20,
                Utc::now(),
            ),
        ];
        let remote_docs = vec![
            FileDoc::new("mirror/notes/new.md".to_string(), String::new(), 15),
            FileDoc {
                deleted: true,
                ..FileDoc::new("mirror/notes/deleted.md".to_string(), String::new(), 0)
            },
        ];

        let (local_deletes, remote_downloads) = plan_local_rebuild(&local_states, &remote_docs);

        assert_eq!(
            local_deletes,
            vec!["notes/old.md".to_string(), "notes/extra.md".to_string()]
        );
        assert_eq!(remote_downloads, vec!["mirror/notes/new.md".to_string()]);
    }

    #[test]
    fn remote_prefix_is_stripped_when_mapping_remote_paths_back_to_local() {
        assert_eq!(
            remote_path_to_local_path("Agents/ross-coulthart/AGENT.md", "Agents/"),
            "ross-coulthart/AGENT.md"
        );
        assert_eq!(
            remote_path_to_local_path("notes/test.md", ""),
            "notes/test.md"
        );
    }
}
