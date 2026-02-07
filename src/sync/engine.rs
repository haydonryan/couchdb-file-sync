use crate::couchdb::CouchDb;
use crate::local::{compute_file_hash, LocalDb, Scanner};
use crate::models::{Change, ChangeType, Conflict, FileState, ResolutionStrategy};
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
        info!("Starting sync cycle");
        let mut report = SyncReport::default();

        // 1. Scan local changes
        let local_changes = self.scan_local_changes().await?;
        info!("Detected {} local changes", local_changes.len());
        for change in &local_changes {
            debug!(
                "Local change: {} ({:?})",
                change.path, change.change_type
            );
        }

        // 2. Get remote changes
        let (remote_changes, last_seq) = self.fetch_remote_changes().await?;
        info!("Detected {} remote changes", remote_changes.len());

        // 3. Detect conflicts
        let (clean_local, clean_remote, conflicts) = 
            self.detect_conflicts(&local_changes, &remote_changes).await?;
        
        report.conflicts = conflicts.len();

        // 4. Store conflicts
        for conflict in conflicts {
            info!("Conflict detected: {}", conflict.path);
            self.local_db.store_conflict(&conflict)?;
        }

        // 5. Apply clean local changes to remote
        for change in clean_local {
            match self.apply_to_couchdb(&change).await {
                Ok(_) => {
                    if matches!(change.change_type, ChangeType::Deleted) {
                        report.deleted_remote += 1;
                        self.local_db.delete_file_state(&change.path)?;
                    } else {
                        report.uploaded += 1;
                        // Save file state after successful upload
                        let relative_path = change.path.trim_start_matches('/');
                        let file_path = self.root_dir.join(relative_path);
                        if let Ok(metadata) = std::fs::metadata(&file_path) {
                            let hash = compute_file_hash(&file_path).unwrap_or_default();
                            let state = FileState::new(
                                change.path.clone(),
                                hash,
                                metadata.len(),
                                metadata.modified().unwrap_or(std::time::SystemTime::now()).into(),
                            );
                            self.local_db.save_file_state(&state)?;
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to upload {}: {}", change.path, e);
                    report.errors.push(format!("Upload {}: {}", change.path, e));
                }
            }
        }

        // 6. Apply clean remote changes to local
        for change in clean_remote {
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
                    report.errors.push(format!("Download {}: {}", change.path, e));
                }
            }
        }

        // 7. Update checkpoint
        self.local_db.save_checkpoint(&last_seq)?;

        info!(
            "Sync complete: {} uploaded, {} downloaded, {} conflicts",
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

        // Only update stored states for files that haven't changed
        // (new and modified files will be updated after successful sync)
        for state in &current_states {
            // Check if this file is in the changes list
            let is_changed = changes.iter().any(|c| c.path == state.path);
            if !is_changed {
                // File unchanged, update the state
                self.local_db.save_file_state(state)?;
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

        // Check local changes against remote state
        for local_change in local_changes {
            // Get our stored state (last sync) for this file
            let stored_state = self.local_db.get_file_state(&local_change.path)?;

            // Check if remote has this file and if it changed
            let remote_changed = if let Some(remote_change) = remote_map.get(&local_change.path) {
                match (&remote_change.mtime, &stored_state) {
                    (Some(remote_mtime), Some(state)) => *remote_mtime > state.last_sync_at,
                    (None, _) => true, // No mtime info, assume changed to be safe
                    (_, None) => true, // No stored state, file is new
                }
            } else {
                false // File doesn't exist on remote
            };

            if remote_changed {
                // Both local and remote changed - conflict
                let local_state = self.get_local_state(&local_change.path).await?;
                let remote_state = match self.couchdb.get_remote_state(&local_change.path).await? {
                    Some(state) => state,
                    None => continue,
                };

                info!("Conflict detected: {} (both local and remote changed)", local_change.path);
                conflicts.push(Conflict::new(
                    local_change.path.clone(),
                    local_state,
                    remote_state,
                ));
            } else {
                // Only local changed, safe to upload
                info!("Local file changed, will upload: {}", local_change.path);
                clean_local.push(local_change.clone());
            }
        }

        // Add remote-only changes (only if remote is newer than local or file doesn't exist)
        for remote_change in remote_changes {
            if !local_map.contains_key(&remote_change.path) {
                // Strip leading / to check file existence
                let relative_path = remote_change.path.trim_start_matches('/');
                let file_path = self.root_dir.join(relative_path);
                let file_exists = file_path.exists();

                // Check if we have local state for this file
                let should_download = match self.local_db.get_file_state(&remote_change.path)? {
                    Some(local_state) => {
                        if !file_exists {
                            // File was deleted locally, re-download it
                            debug!("File {} was deleted locally, will re-download", remote_change.path);
                            true
                        } else {
                            // Only download if remote mtime is newer than our last sync
                            match remote_change.mtime {
                                Some(remote_mtime) => remote_mtime > local_state.last_sync_at,
                                None => true, // No mtime info, download to be safe
                            }
                        }
                    }
                    None => {
                        // No local state - check if file exists on disk
                        if file_exists {
                            debug!("File {} exists but not tracked, skipping (add to local state)", remote_change.path);
                            false // File exists but not tracked, don't overwrite
                        } else {
                            debug!("New remote file: {}", remote_change.path);
                            true // New file, download it
                        }
                    }
                };

                if should_download {
                    clean_remote.push(remote_change.clone());
                } else {
                    debug!("Skipping {} - local is up to date", remote_change.path);
                }
            }
        }

        Ok((clean_local, clean_remote, conflicts))
    }

    /// Get local file state
    async fn get_local_state(&self, path: &str) -> Result<FileState> {
        // Strip leading / to prevent absolute path issues
        let relative_path = path.trim_start_matches('/');
        let file_path = self.root_dir.join(relative_path);
        let hash = compute_file_hash(&file_path)
            .map_err(|e| anyhow::anyhow!("Failed to compute hash for {}: {}", file_path.display(), e))?;
        let metadata = std::fs::metadata(&file_path)
            .map_err(|e| anyhow::anyhow!("Failed to read metadata for {}: {}", file_path.display(), e))?;

        Ok(FileState::new(
            path.to_string(),
            hash,
            metadata.len(),
            metadata.modified()?.into(),
        ))
    }

    /// Apply a change to CouchDB
    async fn apply_to_couchdb(&mut self, change: &Change) -> Result<()> {
        debug!("Applying local change to CouchDB: {} ({:?})", change.path, change.change_type);
        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                // Strip leading / to prevent absolute path issues
                let relative_path = change.path.trim_start_matches('/');
                let file_path = self.root_dir.join(relative_path);
                let metadata = std::fs::metadata(&file_path)
                    .map_err(|e| anyhow::anyhow!("Failed to read metadata for {}: {}", file_path.display(), e))?;
                let mtime = metadata
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                // Create FileDoc with current file info (Obsidian LiveSync format)
                let mut doc = crate::models::FileDoc {
                    id: change.path.clone(),
                    rev: None, // Will be set by CouchDB
                    children: Vec::new(), // TODO: implement chunking for uploads
                    path: change.path.clone(),
                    ctime: mtime, // Use mtime as ctime for now
                    mtime,
                    size: metadata.len(),
                    doc_type: "plain".to_string(),
                    deleted: false,
                };

                // Check if document exists to get its revision and children
                if let Some(existing) = self.couchdb.get_file(&change.path).await? {
                    doc.rev = existing.rev;
                    doc.children = existing.children;
                    doc.ctime = existing.ctime;
                }

                self.couchdb.save_file(&mut doc).await?;
                info!("Uploaded to CouchDB: {}", change.path);
            }
            ChangeType::Deleted => {
                self.couchdb.delete_file(&change.path).await?;
                self.local_db.delete_file_state(&change.path)?;
                info!("Deleted from CouchDB: {}", change.path);
            }
        }
        Ok(())
    }

    /// Apply a change to the local filesystem
    async fn apply_to_filesystem(&mut self, change: &Change) -> Result<()> {
        // Strip leading / to prevent absolute path issues
        let relative_path = change.path.trim_start_matches('/');
        let file_path = self.root_dir.join(relative_path);
        
        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                // Get the document from CouchDB
                let doc = match self.couchdb.get_file(&change.path).await? {
                    Some(d) => d,
                    None => {
                        warn!("Document not found in CouchDB: {}", change.path);
                        return Ok(());
                    }
                };

                // Ensure parent directory exists
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                // Download file content from CouchDB attachment
                let content = match self.couchdb.get_file_content(&change.path).await {
                    Ok(data) => data,
                    Err(e) => {
                        // If attachment doesn't exist, create empty file
                        debug!("No content attachment for {}: {}", change.path, e);
                        Vec::new()
                    }
                };

                tokio::fs::write(&file_path, &content).await?;
                
                // Update local state
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let state = FileState {
                    path: change.path.clone(),
                    hash,
                    size: metadata.len(),
                    modified_at: metadata.modified()?.into(),
                    couch_rev: doc.rev,
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&state)?;
                
                info!("Downloaded from CouchDB: {}", change.path);
            }
            ChangeType::Deleted => {
                if file_path.exists() {
                    tokio::fs::remove_file(&file_path).await?;
                    self.local_db.delete_file_state(&change.path)?;
                    info!("Deleted locally: {}", change.path);
                }
            }
        }
        Ok(())
    }

    /// Get list of conflicts
    pub fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        self.local_db.get_conflicts()
    }

    /// Resolve a conflict
    pub async fn resolve_conflict(
        &mut self,
        path: &str,
        strategy: ResolutionStrategy,
    ) -> Result<()> {
        let _conflict = match self.local_db.get_conflict(path)? {
            Some(c) => c,
            None => {
                anyhow::bail!("No conflict found for path: {}", path);
            }
        };

        match strategy {
            ResolutionStrategy::KeepLocal => {
                // Force upload local version
                let file_path = self.root_dir.join(path);
                let _hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let mtime = metadata
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let mut doc = crate::models::FileDoc {
                    id: path.to_string(),
                    rev: None,
                    children: Vec::new(),
                    path: path.to_string(),
                    ctime: mtime,
                    mtime,
                    size: metadata.len(),
                    doc_type: "plain".to_string(),
                    deleted: false,
                };

                // Get existing revision if available
                if let Some(existing) = self.couchdb.get_file(path).await? {
                    doc.rev = existing.rev;
                    doc.children = existing.children;
                    doc.ctime = existing.ctime;
                }

                self.couchdb.save_file(&mut doc).await?;
                info!("Resolved conflict (keep-local): {}", path);
            }
            ResolutionStrategy::KeepRemote => {
                // Force download remote version
                let doc = match self.couchdb.get_file(path).await? {
                    Some(d) => d,
                    None => anyhow::bail!("Document not found: {}", path),
                };

                let file_path = self.root_dir.join(path);
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                // Download file content from CouchDB attachment
                let content = self.couchdb.get_file_content(path).await.unwrap_or_default();
                tokio::fs::write(&file_path, &content).await?;
                
                // Update local state
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let state = FileState {
                    path: path.to_string(),
                    hash,
                    size: metadata.len(),
                    modified_at: metadata.modified()?.into(),
                    couch_rev: doc.rev,
                    last_sync_at: Utc::now(),
                };
                self.local_db.save_file_state(&state)?;
                
                info!("Resolved conflict (keep-remote): {}", path);
            }
            ResolutionStrategy::KeepBoth => {
                // Save remote as .remote file
                let doc = match self.couchdb.get_file(path).await? {
                    Some(d) => d,
                    None => anyhow::bail!("Document not found: {}", path),
                };

                let remote_path = format!("{}.remote", path);
                let file_path = self.root_dir.join(&remote_path);
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }

                // Download file content from CouchDB attachment
                let content = self.couchdb.get_file_content(path).await.unwrap_or_default();
                tokio::fs::write(&file_path, &content).await?;
                info!("Saved remote version as: {}", remote_path);
                
                // Local file stays as-is
                // User can manually merge/compare
                
                // Update local state for remote file
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let state = FileState {
                    path: remote_path,
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
        self.local_db.delete_conflict(path)?;
        
        Ok(())
    }
}
