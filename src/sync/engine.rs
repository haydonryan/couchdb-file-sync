use crate::couchdb::CouchDb;
use crate::local::{compute_bytes_hash, compute_file_hash, LocalDb, Scanner};
use crate::models::{Change, ChangeType, Conflict, FileState, RemoteState, ResolutionStrategy};
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
        Self {
            couchdb,
            local_db,
            root_dir,
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
        let (clean_local, clean_remote, conflicts) = self
            .detect_conflicts(&local_changes, &remote_changes)
            .await?;

        report.conflicts = conflicts.len();

        info!("After analysis:");
        info!("  - Files to upload: {}", clean_local.len());
        info!("  - Files to download: {}", clean_remote.len());
        info!("  - Conflicts: {}", conflicts.len());

        // 4. Store conflicts
        for conflict in conflicts {
            info!("CONFLICT: {}", conflict.path);
            self.local_db.store_conflict(&conflict)?;
        }

        // 5. Apply clean local changes to remote
        info!(
            "========== UPLOADING {} FILES ==========",
            clean_local.len()
        );
        for change in clean_local {
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
            clean_remote.len()
        );
        for change in clean_remote {
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

    /// Scan for local changes
    async fn scan_local_changes(&self) -> Result<Vec<Change>> {
        use crate::models::IgnoreMatcher;

        let scanner = Scanner::new(self.root_dir.clone(), IgnoreMatcher::empty());
        let current_states = scanner.full_scan()?;
        let stored_states = self.local_db.get_all_file_states()?;

        debug!("Scanned {} files on disk", current_states.len());
        debug!("Found {} files in local database", stored_states.len());

        let changes = scanner.detect_changes(&current_states, &stored_states);

        debug!("Detected {} changes from local scan", changes.len());
        for change in &changes {
            debug!("  Local change: {} ({:?})", change.path, change.change_type);
        }

        // Build a map of stored states to preserve couch_rev
        let stored_map: HashMap<_, _> = stored_states.iter().map(|s| (&s.path, s)).collect();

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

        let mut clean_local = Vec::new();
        let mut clean_remote = Vec::new();
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
                clean_local.push(lc.clone());
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
                            info!(
                                "  [REMOTE CHANGE DETECTED] {}",
                                lc.path
                            );
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
                                    remote_size,
                                    state.size
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
                            info!("    Stored rev: {:?} | Remote rev: {:?}", state.couch_rev, rc.rev);
                        }
                        true
                    }
                    (_, None) => {
                        info!("  [REMOTE CHANGE DETECTED] {} - no stored state (first sync)", lc.path);
                        info!("    Remote mtime: {:?} | Remote rev: {:?} | Remote size: {:?}", rc.mtime, rc.rev, rc.size);
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
                debug!("    local hash:  {}", &local_state.hash[..8.min(local_state.hash.len())]);
                debug!("    remote hash: {}", &remote_hash[..8.min(remote_hash.len())]);
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
                info!("  [UPLOAD] {} - remote unchanged, queuing for upload", lc.path);
                clean_local.push(lc.clone());
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
            debug!("  mtime: {:?}, rev: {:?}, size: {:?}", rc.mtime, rc.rev, rc.size);

            if local_map.contains_key(&local_path) {
                debug!("  File also changed locally, skipping (handled above)");
                continue;
            }

            if rc.change_type == ChangeType::Deleted {
                let relative_path = local_path.trim_start_matches('/');
                let file_path = self.root_dir.join(relative_path);
                let has_state = self.local_db.get_file_state(&local_path)?.is_some();
                if file_path.exists() || has_state {
                    debug!("  Remote deleted, scheduling local delete");
                    clean_remote.push(rc.clone());
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
                clean_remote.push(rc.clone());
            } else {
                debug!("  => Skipping (already in sync)");
            }
            debug!("");
        }

        debug!("========== ANALYSIS COMPLETE ==========");
        debug!("  Uploads queued: {}", clean_local.len());
        debug!("  Downloads queued: {}", clean_remote.len());
        debug!("  Conflicts found: {}", conflicts.len());

        Ok((clean_local, clean_remote, conflicts))
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

    /// Apply a change to CouchDB
    async fn apply_to_couchdb(&mut self, change: &Change) -> Result<()> {
        let remote_path = self.couchdb.get_remote_path(&change.path);
        debug!("[UPLOAD] Starting: {} -> {}", change.path, remote_path);
        debug!("[UPLOAD] Change type: {:?}", change.change_type);

        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                let relative_path = change.path.trim_start_matches('/');
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
                    match self.couchdb.get_file(&remote_path).await? {
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
                    id: remote_path.clone(),
                    rev: existing_rev,
                    children: new_chunk_ids,
                    path: remote_path.clone(),
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

                let new_rev = doc.rev.clone();
                let hash = compute_file_hash(&file_path)?;
                let state = FileState {
                    path: change.path.clone(),
                    hash,
                    size: metadata.len(),
                    modified_at: metadata.modified()?.into(),
                    couch_rev: new_rev.clone(),
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&state)?;
                debug!("[UPLOAD] Updated local state with rev: {:?}", new_rev);

                info!(
                    "[UPLOAD] SUCCESS: {} -> {} ({} bytes, rev: {:?})",
                    change.path,
                    remote_path,
                    content.len(),
                    new_rev
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
        let local_path = self.couchdb.get_local_path(&change.path);
        let relative_path = local_path.trim_start_matches('/');
        let file_path = self.root_dir.join(relative_path);

        debug!("[DOWNLOAD] Remote is newer, downloading chunked file");
        debug!("[DOWNLOAD] {} -> {}", change.path, local_path);

        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                // Fetch metadata first
                let doc = match self.couchdb.fetch_metadata(&change.path).await? {
                    Some(d) => d,
                    None => {
                        warn!("Document not found in CouchDB: {}", change.path);
                        return Ok(());
                    }
                };

                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                // Now download the chunked content
                debug!("[DOWNLOAD] Downloading {} chunks...", doc.children.len());
                let content = match self.couchdb.get_file_content(&change.path).await {
                    Ok(data) => {
                        debug!("[DOWNLOAD] Downloaded {} bytes from chunks", data.len());
                        data
                    }
                    Err(e) => {
                        debug!("No content for {}: {}, using empty file", change.path, e);
                        Vec::new()
                    }
                };

                tokio::fs::write(&file_path, &content).await?;
                debug!("[DOWNLOAD] Wrote {} bytes to disk", content.len());

                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;

                let state = FileState {
                    path: local_path.clone(),
                    hash,
                    size: metadata.len(),
                    modified_at: metadata.modified()?.into(),
                    couch_rev: doc.rev.clone(),
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&state)?;

                info!(
                    "[DOWNLOAD] Chunked file downloaded: {} ({} bytes)",
                    local_path,
                    content.len()
                );
            }
            ChangeType::Deleted => {
                debug!(
                    "[LOCAL DELETE] Remote: {}, Local: {:?}",
                    change.path, file_path
                );
                if file_path.exists() {
                    tokio::fs::remove_file(&file_path).await?;
                }
                self.local_db.delete_file_state(&local_path)?;
                info!("[LOCAL DELETE] SUCCESS: {} -> {}", change.path, local_path);
            }
        }
        Ok(())
    }

    /// Get list of conflicts
    pub fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        self.local_db.get_conflicts()
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
                let file_path = self.root_dir.join(local_path);
                let metadata = std::fs::metadata(&file_path)?;
                let mtime = metadata
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                // Read local file content and upload as chunks
                let content = tokio::fs::read(&file_path).await?;
                let new_chunk_ids = self.couchdb.upload_file_content(&content).await?;

                // Get existing revision and old chunks
                let (existing_rev, existing_ctime, old_chunk_ids) =
                    match self.couchdb.get_file(&remote_path).await? {
                        Some(existing) => (existing.rev, existing.ctime, existing.children),
                        None => (None, mtime, Vec::new()),
                    };

                let mut doc = crate::models::FileDoc {
                    id: remote_path.clone(),
                    rev: existing_rev,
                    children: new_chunk_ids,
                    path: remote_path.clone(),
                    ctime: existing_ctime,
                    mtime,
                    size: metadata.len(),
                    doc_type: "plain".to_string(),
                    deleted: false,
                };

                self.couchdb.save_file(&mut doc).await?;

                // Delete old chunks
                if !old_chunk_ids.is_empty() {
                    self.couchdb.delete_chunks(&old_chunk_ids).await?;
                }

                // Update local state with new revision
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

                info!("Resolved conflict (keep-local): {} - uploaded to remote", local_path);
            }
            ResolutionStrategy::KeepRemote => {
                // Force download remote version
                let doc = match self.couchdb.get_file(&remote_path).await? {
                    Some(d) => d,
                    None => anyhow::bail!("Document not found: {}", remote_path),
                };

                let file_path = self.root_dir.join(local_path);
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                // Download file content from CouchDB
                let content = self
                    .couchdb
                    .get_file_content(&remote_path)
                    .await
                    .unwrap_or_default();
                tokio::fs::write(&file_path, &content).await?;

                // Update local state
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let state = FileState {
                    path: local_path.to_string(),
                    hash,
                    size: metadata.len(),
                    modified_at: metadata.modified()?.into(),
                    couch_rev: doc.rev,
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&state)?;

                info!("Resolved conflict (keep-remote): {}", local_path);
            }
            ResolutionStrategy::KeepBoth => {
                // Save remote as .remote file
                let doc = match self.couchdb.get_file(&remote_path).await? {
                    Some(d) => d,
                    None => anyhow::bail!("Document not found: {}", remote_path),
                };

                let local_remote_path = format!("{}.remote", local_path);
                let file_path = self.root_dir.join(&local_remote_path);
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                // Download file content from CouchDB
                let content = self
                    .couchdb
                    .get_file_content(&remote_path)
                    .await
                    .unwrap_or_default();
                tokio::fs::write(&file_path, &content).await?;
                info!("Saved remote version as: {}", local_remote_path);

                // Local file stays as-is
                // User can manually merge/compare

                // Update local state for remote file
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let state = FileState {
                    path: local_remote_path,
                    hash,
                    size: metadata.len(),
                    modified_at: metadata.modified()?.into(),
                    couch_rev: doc.rev,
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&state)?;
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
