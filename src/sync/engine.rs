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
        
        let changes = scanner.detect_changes(&current_states, &stored_states);
        
        // Update stored states with computed hashes
        for state in current_states {
            self.local_db.save_file_state(&state)?;
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

        // Check for conflicts (same path modified on both sides)
        for local_change in local_changes {
            if let Some(remote_change) = remote_map.get(&local_change.path) {
                // Both sides changed - potential conflict
                if Self::is_actual_conflict(local_change, remote_change).await? {
                    // Get detailed state for conflict record
                    let local_state = self.get_local_state(&local_change.path).await?;
                    let remote_state = match self.couchdb.get_remote_state(&local_change.path).await? {
                        Some(state) => state,
                        None => continue,
                    };
                    
                    conflicts.push(Conflict::new(
                        local_change.path.clone(),
                        local_state,
                        remote_state,
                    ));
                } else {
                    // Same change on both sides - no conflict
                    debug!("Convergent change on both sides: {}", local_change.path);
                }
            } else {
                // Only local changed
                clean_local.push(local_change.clone());
            }
        }

        // Add remote-only changes
        for remote_change in remote_changes {
            if !local_map.contains_key(&remote_change.path) {
                clean_remote.push(remote_change.clone());
            }
        }

        Ok((clean_local, clean_remote, conflicts))
    }

    /// Determine if two changes are actually in conflict
    async fn is_actual_conflict(local: &Change, remote: &Change) -> Result<bool> {
        // If hashes are the same, it's not a conflict (convergent change)
        match (&local.hash, &remote.hash) {
            (Some(lh), Some(rh)) if lh == rh => Ok(false),
            _ => Ok(true),
        }
    }

    /// Get local file state
    async fn get_local_state(&self, path: &str) -> Result<FileState> {
        let file_path = self.root_dir.join(path);
        let hash = compute_file_hash(&file_path)?;
        let metadata = std::fs::metadata(&file_path)?;
        
        Ok(FileState::new(
            path.to_string(),
            hash,
            metadata.len(),
            metadata.modified()?.into(),
        ))
    }

    /// Apply a change to CouchDB
    async fn apply_to_couchdb(&mut self, change: &Change) -> Result<()> {
        match change.change_type {
            ChangeType::Created | ChangeType::Modified => {
                let file_path = self.root_dir.join(&change.path);
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let modified_at = metadata.modified()?.into();
                
                // Create FileDoc with current file info
                let mut doc = crate::models::FileDoc {
                    id: change.path.clone(),
                    rev: None,  // Will be set by CouchDB
                    content_type: mime_guess::from_path(&change.path)
                        .first_or_octet_stream()
                        .to_string(),
                    size: metadata.len(),
                    modified_at,
                    hash,
                    deleted: false,
                    synced_at: Utc::now(),
                };
                
                // Check if document exists to get its revision
                if let Some(existing) = self.couchdb.get_file(&change.path).await? {
                    doc.rev = existing.rev;
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
        let file_path = self.root_dir.join(&change.path);
        
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
                
                // Note: In a full implementation, this is where we'd download
                // the actual file content. For now, we just create an empty file
                // or touch the file to indicate it was synced.
                // A production version would use CouchDB attachments or
                // a separate file storage backend.
                
                // Ensure parent directory exists
                if let Some(parent) = file_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                
                // Create or update local file (empty for now)
                if !file_path.exists() {
                    tokio::fs::write(&file_path, &[]).await?;
                }
                
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
                let hash = compute_file_hash(&file_path)?;
                let metadata = std::fs::metadata(&file_path)?;
                let modified_at = metadata.modified()?.into();
                
                let mut doc = crate::models::FileDoc {
                    id: path.to_string(),
                    rev: None,
                    content_type: mime_guess::from_path(path)
                        .first_or_octet_stream()
                        .to_string(),
                    size: metadata.len(),
                    modified_at,
                    hash,
                    deleted: false,
                    synced_at: Utc::now(),
                };
                
                // Get existing revision if available
                if let Some(existing) = self.couchdb.get_file(path).await? {
                    doc.rev = existing.rev;
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
                
                // Create/update local file (empty for now - see note in apply_to_filesystem)
                tokio::fs::write(&file_path, &[]).await?;
                
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
                
                // Create remote file (empty for now)
                tokio::fs::write(&file_path, &[]).await?;
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
