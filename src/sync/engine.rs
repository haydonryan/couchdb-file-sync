use crate::couchdb::CouchDb;
use crate::local::{LocalDb, Scanner, compute_bytes_hash, compute_file_hash};
use crate::models::{
    Change, ChangeType, Checkpoint, Conflict, CouchRev, DownloadCount, ExpectedRemote, FileState,
    IgnoreMatcher, RemoteState, ResolutionStrategy, SyncDirPath, UploadCount,
};
use crate::sync::triage;
use anyhow::Result;
use chrono::{TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info, warn};

/// Maximum number of per-file upload/download operations applied in parallel
/// during the sync phase. Bounds in-flight HTTP + hash + DB work so a large
/// change batch is processed concurrently without unbounded resource use.
const APPLY_CONCURRENCY_LIMIT: usize = 8;

/// The main sync engine
pub struct SyncEngine {
    /// Shared `CouchDB` client; all operations are `&self` so the same client is
    /// used concurrently by the bounded batch apply loops.
    couchdb: Arc<CouchDb>,
    local_db: AsyncLocalDb,
    scanner: Scanner,
    root_dir: SyncDirPath,
    /// Kept for backward compatibility; delegates to scanner.
    ignore_matcher: IgnoreMatcher,
    /// Optional retention window for soft-delete tombstones. When set, each
    /// non-dry-run sync cycle prunes tombstones older than this window.
    tombstone_retention: Option<std::time::Duration>,
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

    /// Run one blocking `LocalDb` operation on the blocking thread pool,
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
        self.run(super::super::local::db::LocalDb::get_all_file_states)
            .await
    }

    async fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        let path = path.to_string();
        self.run(move |db| db.get_file_state(&path)).await
    }

    async fn get_file_states(&self, paths: &[&str]) -> Result<HashMap<String, FileState>> {
        let paths: Vec<String> = paths.iter().map(std::string::ToString::to_string).collect();
        self.run(move |db| {
            db.get_file_states(&paths.iter().map(String::as_str).collect::<Vec<_>>())
        })
        .await
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
        self.run(super::super::local::db::LocalDb::get_checkpoint)
            .await
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
        self.run(super::super::local::db::LocalDb::get_conflicts)
            .await
    }

    async fn get_conflict(&self, path: &str) -> Result<Option<Conflict>> {
        let path = path.to_string();
        self.run(move |db| db.get_conflict(&path)).await
    }

    async fn reset_sync_state(&self) -> Result<()> {
        self.run(super::super::local::db::LocalDb::reset_sync_state)
            .await
    }

    async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.run(move |db| db.get_meta(&key)).await
    }

    async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_string();
        let value = value.to_string();
        self.run(move |db| db.set_meta(&key, &value)).await
    }
}

/// Owned handle to the per-file apply state used by the bounded-concurrency
/// sync loops.
///
/// Cloning this handle is cheap (an `Arc` for the `CouchDB` client, the shared
/// `AsyncLocalDb` handle, and a `PathBuf`), so each task in a sync batch can
/// process its own file without borrowing `SyncEngine`.
#[derive(Clone)]
struct ApplyWorker {
    couchdb: Arc<CouchDb>,
    local_db: AsyncLocalDb,
    root_dir: SyncDirPath,
    /// Incoming-path policy used as a defensive guard at write/delete
    /// boundaries so ignored or internal paths are never written or removed
    /// locally even if they slipped past triage.
    ignore_matcher: IgnoreMatcher,
}

impl ApplyWorker {
    async fn upload_local_file(
        &self,
        local_path: &str,
        remote_path: &str,
        expected_rev: &ExpectedRemote,
    ) -> Result<(usize, Option<String>)> {
        let file_path = self.root_dir.join_contained(local_path)?;
        let metadata = tokio::fs::metadata(&file_path).await?;
        let mtime = u64::try_from(
            metadata
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(0);

        debug!("[UPLOAD] File path: {:?}", file_path);
        debug!("[UPLOAD] Size: {} bytes", metadata.len());
        debug!("[UPLOAD] mtime: {} ms", mtime);

        let content = tokio::fs::read(&file_path).await?;
        let content_len = content.len();
        debug!("[UPLOAD] Read {} bytes from disk", content_len);

        // Hash the in-memory content buffer before moving it into the chunk,
        // so the persisted hash still matches the transferred bytes. Since
        // `content` is exactly the bytes pushed to CouchDB, this keeps both
        // the persisted `FileDoc.hash` and the saved FileState in agreement
        // with the transferred bytes even under a mid-sync modification.
        let hash = compute_bytes_hash(&content);

        let new_chunk_ids = self.couchdb.upload_file_content(content).await?;
        debug!(
            "[UPLOAD] Uploaded content, got {} chunks",
            new_chunk_ids.len()
        );

        // Fetch the current remote doc once. The planned baseline (carried from
        // the triage/live-arbitration decision) decides whether publication is
        // still authorized; the current doc's chunks and ctime are preserved on
        // the success path.
        let current = self.couchdb.get_file(remote_path).await?;

        // Conditional publication: reject a stale baseline instead of
        // overwriting a concurrent writer. This pre-save check catches the
        // common race; a 409 from `save_file` (a race landing between this
        // check and the save) is surfaced as an error by the client and likewise
        // never adopts the latest revision as authorization to overwrite.
        match expected_rev {
            ExpectedRemote::Absent => {
                if let Some(doc) = &current {
                    anyhow::bail!(
                        "concurrent remote creation detected for {remote_path}: expected absent, found rev {:?}; refusing to overwrite",
                        doc.rev
                    );
                }
            }
            ExpectedRemote::At(expected) => match &current {
                Some(doc) if doc.rev.as_deref() == Some(expected.as_str()) => {
                    debug!("[UPLOAD] Remote doc at expected rev {expected}");
                }
                Some(doc) => {
                    anyhow::bail!(
                        "concurrent remote change detected for {remote_path}: expected rev {expected}, found {:?}; refusing to overwrite",
                        doc.rev
                    );
                }
                None => {
                    anyhow::bail!(
                        "remote doc {remote_path} disappeared before upload; expected rev {expected}; refusing to write"
                    );
                }
            },
            ExpectedRemote::Force => {}
        }

        let (existing_rev, existing_ctime, old_chunk_ids) = if let Some(existing) = current {
            debug!("[UPLOAD] Existing doc found, rev: {:?}", existing.rev);
            debug!("[UPLOAD] Existing ctime: {} ms", existing.ctime.as_u64());
            debug!("[UPLOAD] Existing chunks: {}", existing.children.len());
            (existing.rev, existing.ctime.as_u64(), existing.children)
        } else {
            debug!("[UPLOAD] No existing doc, creating new");
            (None, mtime, Vec::new())
        };

        let mut doc = crate::models::FileDoc {
            id: remote_path.to_string(),
            rev: existing_rev,
            children: new_chunk_ids,
            path: remote_path.to_string(),
            ctime: crate::models::TimestampMillis::new(existing_ctime),
            mtime: crate::models::TimestampMillis::new(mtime),
            deleted_at: crate::models::TimestampMillis::default(),
            size: metadata.len(),
            doc_type: crate::models::DocType::Plain,
            deleted: false,
            hash: hash.clone(),
        };

        self.couchdb.save_file(&mut doc).await?;
        debug!("[UPLOAD] Saved doc, new rev: {:?}", doc.rev);

        if !old_chunk_ids.is_empty() {
            debug!("[UPLOAD] Deleting {} old chunks", old_chunk_ids.len());
            self.couchdb.delete_chunks(&old_chunk_ids).await?;
        }

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

        Ok((content_len, doc.rev))
    }

    async fn download_remote_file(
        &self,
        remote_path: &str,
        local_path: &str,
        require_doc: bool,
        protect_local_edits: bool,
    ) -> Result<Option<usize>> {
        let Some(doc) = self.couchdb.fetch_metadata(remote_path).await? else {
            if require_doc {
                anyhow::bail!("Document not found in CouchDB: {remote_path}");
            }
            warn!("Document not found in CouchDB: {}", remote_path);
            return Ok(None);
        };

        let file_path = self.root_dir.join_contained(local_path)?;
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
                anyhow::bail!("failed to fetch content for {remote_path}: {e}");
            }
        };

        // Hash the in-memory content buffer we just fetched instead of
        // re-reading the file back from disk. This avoids a redundant disk
        // read and keeps the saved hash equal to the bytes actually transferred.
        let hash = compute_bytes_hash(&content);

        // When the remote doc records a content hash (uploaded by this tool),
        // verify the downloaded bytes match it. A mismatch means the transfer
        // was partial or corrupted: fail the file rather than writing a bad
        // local copy and recording it as synced. Docs without a stored hash
        // (older or external) are accepted as-is.
        if !doc.hash.is_empty() && doc.hash != hash {
            anyhow::bail!(
                "content hash mismatch for {remote_path}: remote stored {}, downloaded {}; refusing to write partial/corrupt file",
                doc.hash,
                hash
            );
        }

        // Capture the expected local baseline recorded at last sync so we can
        // revalidate the on-disk file immediately before replacement below.
        let stored_state = self.local_db.get_file_state(local_path).await?;

        // Stage the verified bytes in a sibling temp file in the same
        // directory (same filesystem as the target) so the final install is
        // an atomic rename. A partial write, cancellation or disk-full
        // mid-transfer can therefore only corrupt the temp file — never the
        // live local file. The unique name lets a leftover staging file be
        // identified and cleaned up.
        let parent = file_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cannot determine parent directory for {local_path}"))?;
        let staging = parent.join(format!(
            ".{}.{}.{}.download",
            file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file"),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        ));
        {
            let mut f = tokio::fs::File::create(&staging).await?;
            f.write_all(&content).await?;
            f.flush().await?;
            f.sync_all().await?;
        }

        // Revalidate the expected local state immediately before the atomic
        // replacement: if a user edited the file during the (possibly long)
        // transfer above, the on-disk content no longer matches the baseline
        // recorded at last sync, so we must abort rather than clobber the
        // user's version. On any failure, clean up the staging file so no
        // temp residue is left behind.
        let replace = async {
            if protect_local_edits {
                revalidate_local_baseline(local_path, &file_path, stored_state.as_ref()).await?;
            }
            tokio::fs::rename(&staging, &file_path).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(e) = replace {
            let _ = tokio::fs::remove_file(&staging).await;
            debug!("[DOWNLOAD] Cleaned up staging file for {}", local_path);
            return Err(e);
        }

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

    /// Apply a change to `CouchDB`
    async fn apply_to_couchdb(&self, change: &Change) -> Result<()> {
        let remote_path = self.couchdb.get_remote_path(change.path());
        debug!("[UPLOAD] Starting: {} -> {}", change.path(), remote_path);
        debug!("[UPLOAD] Change type: {:?}", change.change_type());

        match change.change_type() {
            ChangeType::Created | ChangeType::Modified => {
                let (bytes_uploaded, new_rev) = self
                    .upload_local_file(change.path(), &remote_path, &change.expected_rev())
                    .await?;
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
    async fn apply_to_filesystem(&self, change: &Change) -> Result<()> {
        let remote_path = change.path();
        let local_path = self.couchdb.get_local_path(remote_path);
        let file_path = self.root_dir.join_contained(&local_path)?;

        // Defensive guard at the incoming write/delete boundary: never create,
        // overwrite, or delete an ignored/internal local path, even if it
        // slipped past triage (e.g. a live-applied remote change that was not
        // routed through the pre-triage filter).
        if self
            .ignore_matcher
            .should_ignore(std::path::Path::new(&local_path))
        {
            debug!(
                "Skipping ignored remote change at write/delete boundary: {}",
                local_path
            );
            return Ok(());
        }

        debug!("[DOWNLOAD] Remote is newer, downloading chunked file");
        debug!("[DOWNLOAD] {} -> {}", remote_path, local_path);

        match change.change_type() {
            ChangeType::Created | ChangeType::Modified => {
                if let Some(bytes) = self
                    .download_remote_file(remote_path, &local_path, false, true)
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
}

/// Verify the on-disk file at `file_path` still matches the last-synced
/// baseline recorded in `stored_state`, so a concurrent local edit is never
/// clobbered by an atomic download replacement.
///
/// Returns `Ok(())` when it is safe to replace the file: the file does not
/// exist (nothing local to protect), or its current content hash equals the
/// baseline hash. Returns an error when the file exists but diverges from
/// the baseline (a local edit happened during the transfer) or exists with no
/// tracked baseline (an untracked local file). In both error cases the caller
/// must leave the on-disk file untouched.
async fn revalidate_local_baseline(
    local_path: &str,
    file_path: &Path,
    stored_state: Option<&FileState>,
) -> Result<()> {
    let Ok(on_disk) = tokio::fs::read(file_path).await else {
        // No local file to protect: safe to create/install fresh.
        return Ok(());
    };
    let on_disk_hash = compute_bytes_hash(&on_disk);
    match stored_state.map(|s| s.hash.as_str()) {
        Some(expected) if expected != on_disk_hash => anyhow::bail!(
            "local file {local_path} changed on disk during download (no longer matches last-synced state); aborting to protect local edits"
        ),
        Some(_) => Ok(()),
        None => anyhow::bail!(
            "local file {local_path} exists but has no tracked sync state; aborting download to protect untracked local content"
        ),
    }
}

/// Per-file result of one upload in a bounded batch, accumulated into the
/// shared `SyncReport` exactly as the sequential loop did.
struct UploadOutcome {
    uploaded: usize,
    deleted_remote: usize,
    error: Option<String>,
}

/// Per-file result of one download in a bounded batch.
struct DownloadOutcome {
    downloaded: usize,
    deleted_local: usize,
    error: Option<String>,
}

/// Apply one queued upload, returning its report deltas and any per-file
/// error. A fatal state-DB cleanup failure is propagated as `Err` so the sync
/// aborts, matching the sequential loop's `?` on the same path.
async fn apply_one_upload(
    worker: ApplyWorker,
    change: Change,
    dry_run: bool,
) -> Result<UploadOutcome> {
    debug!(
        "  Preparing to upload: {} -> {}",
        change.path(),
        worker.couchdb.get_remote_path(change.path())
    );

    let is_deleted = matches!(change.change_type(), ChangeType::Deleted);
    let mut outcome = UploadOutcome {
        uploaded: usize::from(!is_deleted),
        deleted_remote: usize::from(is_deleted),
        error: None,
    };

    if !dry_run {
        if is_deleted {
            match worker.apply_to_couchdb(&change).await {
                Ok(()) => {
                    // Preserve the original post-delete local state refresh.
                    worker.local_db.delete_file_state(change.path()).await?;
                }
                Err(e) => {
                    error!("Failed to upload {}: {}", change.path(), e);
                    outcome.error = Some(format!("Upload {}: {}", change.path(), e));
                }
            }
        } else if let Err(e) = worker.apply_to_couchdb(&change).await {
            error!("Failed to upload {}: {}", change.path(), e);
            outcome.error = Some(format!("Upload {}: {}", change.path(), e));
        }
    }

    Ok(outcome)
}

/// Apply a batch of uploads with bounded concurrency, collecting every
/// per-file error into `report` (none dropped).
async fn apply_upload_batch(
    worker: ApplyWorker,
    changes: Vec<Change>,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let mut in_flight = tokio::task::JoinSet::new();
    let mut pending = changes.into_iter();

    // Seed the first `APPLY_CONCURRENCY_LIMIT` tasks so the number of
    // in-flight file operations never exceeds the bound.
    for _ in 0..APPLY_CONCURRENCY_LIMIT {
        if let Some(change) = pending.next() {
            let worker = worker.clone();
            in_flight.spawn(async move { apply_one_upload(worker, change, dry_run).await });
        }
    }

    while let Some(joined) = in_flight.join_next().await {
        let outcome = joined.map_err(|e| anyhow::anyhow!("upload task panicked: {e}"))??;
        report.uploaded.0 += outcome.uploaded;
        report.deleted_remote += outcome.deleted_remote;
        if let Some(error) = outcome.error {
            report.errors.push(error);
        }
        // Fill the freed slot with the next pending upload.
        if let Some(change) = pending.next() {
            let worker = worker.clone();
            in_flight.spawn(async move { apply_one_upload(worker, change, dry_run).await });
        }
    }

    Ok(())
}

/// Apply one queued download, returning its report deltas and any per-file error.
async fn apply_one_download(
    worker: ApplyWorker,
    change: Change,
    dry_run: bool,
) -> Result<DownloadOutcome> {
    debug!(
        "  Preparing to download: {} -> {}",
        change.path(),
        worker.couchdb.get_local_path(change.path())
    );

    let is_deleted = matches!(change.change_type(), ChangeType::Deleted);
    let mut outcome = DownloadOutcome {
        downloaded: usize::from(!is_deleted),
        deleted_local: usize::from(is_deleted),
        error: None,
    };

    if !dry_run && let Err(e) = worker.apply_to_filesystem(&change).await {
        error!("Failed to download {}: {}", change.path(), e);
        outcome.error = Some(format!("Download {}: {}", change.path(), e));
    }

    Ok(outcome)
}

/// Apply a batch of downloads with bounded concurrency, collecting every
/// per-file error into `report` (none dropped).
async fn apply_download_batch(
    worker: ApplyWorker,
    changes: Vec<Change>,
    dry_run: bool,
    report: &mut SyncReport,
) -> Result<()> {
    let mut in_flight = tokio::task::JoinSet::new();
    let mut pending = changes.into_iter();

    for _ in 0..APPLY_CONCURRENCY_LIMIT {
        if let Some(change) = pending.next() {
            let worker = worker.clone();
            in_flight.spawn(async move { apply_one_download(worker, change, dry_run).await });
        }
    }

    while let Some(joined) = in_flight.join_next().await {
        let outcome = joined.map_err(|e| anyhow::anyhow!("download task panicked: {e}"))??;
        report.downloaded.0 += outcome.downloaded;
        report.deleted_local += outcome.deleted_local;
        if let Some(error) = outcome.error {
            report.errors.push(error);
        }
        if let Some(change) = pending.next() {
            let worker = worker.clone();
            in_flight.spawn(async move { apply_one_download(worker, change, dry_run).await });
        }
    }

    Ok(())
}

impl SyncEngine {
    /// Create a new sync engine
    pub fn new(couchdb: CouchDb, local_db: LocalDb, root_dir: SyncDirPath) -> Self {
        let ignore_matcher = IgnoreMatcher::empty();
        let scanner = Scanner::new(root_dir.clone(), ignore_matcher.clone());
        Self {
            couchdb: Arc::new(couchdb),
            local_db: AsyncLocalDb::new(local_db),
            scanner,
            root_dir,
            ignore_matcher,
            tombstone_retention: None,
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
            couchdb: Arc::new(couchdb),
            local_db: AsyncLocalDb::new(local_db),
            scanner,
            root_dir,
            ignore_matcher,
            tombstone_retention: None,
        }
    }

    /// Enable pruning of soft-delete tombstones older than `retention` on each
    /// non-dry-run sync cycle. Pass `None` (or omit) to disable pruning.
    #[must_use]
    pub const fn with_tombstone_retention(
        mut self,
        retention: Option<std::time::Duration>,
    ) -> Self {
        self.tombstone_retention = retention;
        self
    }

    /// Cheap handle to the per-file apply state used by the bounded-concurrency
    /// sync loops (clones an `Arc`, a shared DB handle, and a `PathBuf`).
    fn apply_worker(&self) -> ApplyWorker {
        ApplyWorker {
            couchdb: self.couchdb.clone(),
            local_db: self.local_db.clone(),
            root_dir: self.root_dir.clone(),
            ignore_matcher: self.ignore_matcher.clone(),
        }
    }

    /// Perform a full sync cycle.
    ///
    /// # Errors
    ///
    /// Returns an error if any stage of the sync pipeline fails.
    pub async fn sync(&mut self) -> Result<SyncReport> {
        self.run_cycle(false).await
    }

    /// Perform a dry-run sync cycle.
    ///
    /// Walks the full sync pipeline (local scan, remote fetch, triage, and
    /// conflict detection) but skips every write: nothing is written to
    /// `CouchDB`, the local filesystem, or the state database. The returned
    /// `SyncReport` reflects what *would* have been uploaded, downloaded,
    /// deleted, and conflicted.
    ///
    /// # Errors
    ///
    /// Returns an error if any read-only stage of the sync pipeline fails.
    pub async fn sync_dry_run(&mut self) -> Result<SyncReport> {
        self.run_cycle(true).await
    }

    /// Shared sync-cycle implementation.
    ///
    /// When `dry_run` is true every write operation (`CouchDB` writes, local
    /// filesystem writes, and state-DB saves) is skipped while the read-only
    /// triage, conflict detection, and report generation still run.
    async fn run_cycle(&self, dry_run: bool) -> Result<SyncReport> {
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

        // Apply the effective incoming path policy BEFORE remote triage so
        // ignored or internal remote files are never downloaded, deleted, or
        // conflicted. Skipping them here (rather than counting them as errors)
        // is the ignored-change checkpoint behavior: the sync cycle advances
        // the remote change-feed checkpoint past them, so an ignored file is
        // not retried every cycle.
        let remote_changes: Vec<Change> = remote_changes
            .into_iter()
            .filter(|change| {
                let local_path = self.couchdb.get_local_path(change.path());
                if self.incoming_path_is_ignored(&local_path) {
                    info!("Skipping ignored remote change: {}", change.path());
                    false
                } else {
                    true
                }
            })
            .collect();

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

        // 5. Apply clean local changes to remote (skipped in dry-run).
        //    Runs with bounded concurrency so a large batch of uploads does not
        //    serialize every per-file network round trip.
        info!(
            "========== UPLOADING {} FILES ==========",
            local_to_upload.len()
        );
        apply_upload_batch(self.apply_worker(), local_to_upload, dry_run, &mut report).await?;

        // 6. Apply clean remote changes to local (skipped in dry-run).
        //    Runs with bounded concurrency so a large batch of downloads does
        //    not serialize every per-file network round trip.
        info!(
            "========== DOWNLOADING {} FILES ==========",
            remote_to_apply.len()
        );
        apply_download_batch(self.apply_worker(), remote_to_apply, dry_run, &mut report).await?;

        // 7. Update checkpoint (skipped in dry-run).
        //    Advance the remote change-feed checkpoint only when every queued
        //    change was applied without error. A failed download leaves the
        //    local file absent/stale (the local scanner cannot re-detect it),
        //    so advancing past it would permanently drop the change; uploads
        //    also self-heal via the next local scan, so retrying the whole
        //    batch after any failure is always safe and idempotent.
        if !dry_run && report.errors.is_empty() {
            self.local_db.save_checkpoint(&last_seq).await?;
        } else if !dry_run {
            warn!(
                "Not advancing sync checkpoint: {} change(s) failed to apply and will be retried",
                report.errors.len()
            );
        }

        // 8. Prune obsolete soft-delete tombstones (skipped in dry-run).
        //    Best-effort: a pruning failure is logged, not fatal to the sync.
        if !dry_run && let Some(retention) = self.tombstone_retention {
            match self.couchdb.prune_tombstones(retention).await {
                Ok(pruned) if pruned > 0 => {
                    info!("Pruned {pruned} obsolete delete tombstone(s)");
                }
                Ok(_) => {}
                Err(e) => warn!("Tombstone pruning failed: {e}"),
            }
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
    ///
    /// # Errors
    ///
    /// Returns an error if the local scan, remote fetch, or any upload/delete
    /// fails.
    pub async fn rebuild_remote_from_local(&mut self) -> Result<SyncReport> {
        info!("========== REMOTE REBUILD STARTING ==========");

        let outcome = self.scanner.full_scan().await?;
        // A rebuild that deletes remote files must not run on an incomplete
        // scan: an unreadable subtree or missing/unmounted sync root would
        // otherwise tombstone remote files that still exist locally.
        if !outcome.complete {
            anyhow::bail!(
                "Refusing remote rebuild: local scan is incomplete (unreadable \
                 subtree or missing/unmounted sync root); not tombstoning remote files"
            );
        }
        let local_states = outcome.states;
        let remote_docs = self.couchdb.get_all_files().await?;
        let (uploads, remote_deletes) =
            triage::plan_remote_rebuild(&local_states, &remote_docs, self.couchdb.remote_prefix());

        self.local_db.reset_sync_state().await?;

        let mut report = SyncReport::default();

        for local_path in uploads {
            let remote_path = self.couchdb.get_remote_path(&local_path);
            // Remote rebuild is an explicit, unconditional mirror of local state
            // to remote: force-overwrite any remote doc regardless of its rev.
            self.upload_local_file(&local_path, &remote_path, &ExpectedRemote::Force)
                .await?;
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
    ///
    /// # Errors
    ///
    /// Returns an error if the local scan, remote fetch, or any download/delete
    /// fails.
    pub async fn rebuild_local_from_remote(&mut self) -> Result<SyncReport> {
        info!("========== LOCAL REBUILD STARTING ==========");

        let outcome = self.scanner.full_scan().await?;
        // A rebuild that deletes local files must not run on an incomplete
        // scan: files hidden in an unreadable subtree would be missed and
        // orphaned by the rebuild.
        if !outcome.complete {
            anyhow::bail!(
                "Refusing local rebuild: local scan is incomplete (unreadable \
                 subtree or missing/unmounted sync root)"
            );
        }
        let local_states = outcome.states;
        let remote_docs = self.couchdb.get_all_files().await?;
        let (local_deletes, remote_downloads) =
            triage::plan_local_rebuild(&local_states, &remote_docs);

        self.local_db.reset_sync_state().await?;

        let mut report = SyncReport::default();

        for local_path in local_deletes {
            // Defensive guard: never delete an ignored/internal local path
            // during a local rebuild.
            if self.incoming_path_is_ignored(&local_path) {
                debug!(
                    "Skipping ignored local path during rebuild delete: {}",
                    local_path
                );
                continue;
            }
            let file_path = self.root_dir.join_contained(&local_path)?;
            if file_path.exists() {
                tokio::fs::remove_file(&file_path).await?;
                report.deleted_local += 1;
            }
        }

        for remote_path in remote_downloads {
            let local_path = self.couchdb.get_local_path(&remote_path);
            // Defensive guard: never download an ignored/internal remote path
            // during a local rebuild.
            if self.incoming_path_is_ignored(&local_path) {
                debug!(
                    "Skipping ignored remote path during rebuild download: {}",
                    remote_path
                );
                continue;
            }
            if self
                .download_remote_file(&remote_path, &local_path, true, false)
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

    /// One-time, versioned, conservative migration that repairs historical
    /// state entries polluted by the remote prefix.
    ///
    /// This replaces the old lexical prefix purge in [`Self::scan_local_changes`]
    /// (which deleted any path starting with the remote prefix and so destroyed
    /// legitimate local subtrees sharing the prefix's name). It runs at most
    /// once per remote scope (persisted in `sync_meta`) and only removes an
    /// entry that is unambiguously a phantom: its on-disk prefixed path does
    /// not exist while its prefix-stripped local path does (the real file lives
    /// at the stripped path). Valid baselines and ambiguous entries are never
    /// touched.
    async fn migrate_historical_pollution(
        &self,
        stored_states: &[FileState],
        remote_prefix: &str,
    ) -> Result<()> {
        if remote_prefix.is_empty() {
            return Ok(());
        }
        let marker_key = format!("polluted_migration:{remote_prefix}");
        if self.local_db.get_meta(&marker_key).await?.is_some() {
            return Ok(());
        }

        let repairs = triage::polluted_state_repairs(
            stored_states,
            remote_prefix,
            |p| self.root_dir.as_path().join(p).exists(),
            |p| self.root_dir.as_path().join(p).exists(),
        );
        for path in &repairs {
            warn!(
                "Migrating historically polluted state entry for {path}: the real \
                 file lives at its prefix-stripped path; removing phantom prefixed entry"
            );
            self.local_db.delete_file_state(path).await?;
        }
        // Record the migration as run even when nothing was repaired so it is
        // not re-attempted (and re-stat'ed) every cycle.
        self.local_db.set_meta(&marker_key, "done").await?;
        Ok(())
    }

    /// Scan for local changes.
    ///
    /// In dry-run mode the scan still detects changes, but the state-DB
    /// cleanups (removing ignored entries, running the one-time pollution
    /// migration, and re-saving unchanged states) are skipped so that nothing
    /// is written to the state DB.
    async fn scan_local_changes(&self, dry_run: bool) -> Result<Vec<Change>> {
        let stored_states = self.local_db.get_all_file_states().await?;
        let remote_prefix = self.couchdb.remote_prefix();
        let mut valid_stored_states = Vec::with_capacity(stored_states.len());

        // One-time versioned migration replacing the old lexical prefix purge.
        // The old code deleted ANY state whose path started with the remote
        // prefix, which wrongly destroyed legitimate local subtrees that happen
        // to share the prefix's name (e.g. a local notes/a.txt under a remote
        // notes/ scope) — losing the baseline so the file's later deletion
        // stopped propagating. The replacement only repairs entries with
        // explicit filesystem evidence of pollution, never a valid baseline.
        // Skipped in dry-run so the migration is a real (written) change only
        // when state DB writes are actually permitted.
        if !dry_run {
            self.migrate_historical_pollution(&stored_states, remote_prefix)
                .await?;
        }

        for state in stored_states {
            if self
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

        // Scan the tree against the valid stored states so unchanged files
        // (mtime AND size equal, not racily clean) reuse their stored hash
        // instead of re-reading and re-hashing every file on every sync.
        // Ignored states are excluded above, so their hashes are never reused.
        let stored_states = Arc::new(valid_stored_states);
        let outcome = self
            .scanner
            .full_scan_with_stored(stored_states.clone())
            .await?;
        let current_states = outcome.states;

        debug!("Scanned {} files on disk", current_states.len());
        debug!("Found {} files in local database", stored_states.len());

        let changes =
            self.scanner
                .detect_changes(&current_states, &stored_states, outcome.complete);

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
            let stored_map: HashMap<_, _> = stored_states.iter().map(|s| (&s.path, s)).collect();

            // Prebuild a set of changed paths so per-file membership checks
            // are O(1) instead of an O(M) scan over the changes list.
            let changed_paths: HashSet<&str> = changes
                .iter()
                .map(super::super::models::change::Change::path)
                .collect();

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
                    let last_sync_at = stored.map_or(state.last_sync_at, |s| s.last_sync_at);
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

    /// Fetch remote changes from `CouchDB`
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
        let stored_states = self
            .load_stored_states(local_changes, remote_changes)
            .await?;
        let remote_prefix = self.couchdb.remote_prefix();
        let remote_by_path = Self::index_remote_changes(remote_changes);

        self.debug_local_changes(local_changes, &stored_states, &remote_by_path);

        // ── Load persisted unresolved conflicts ───────────────────────────
        // A path with an unresolved conflict is frozen: neither the local edit
        // nor the remote version may be applied by a normal sync cycle, so a
        // plain cycle can never implicitly resolve a stored conflict. The
        // conflict record itself persists the pending remote state, so the
        // path is released only by an explicit `resolve_conflict` call, which
        // deletes the record. (In the cycle where a conflict is *first*
        // detected it is not yet persisted, but its local/remote changes go to
        // `needs_comparison`, not to the upload/download queues, so it is
        // already excluded there.)
        let persisted_conflicts = self.local_db.get_conflicts().await?;
        let conflicted_paths: HashSet<&str> = persisted_conflicts
            .iter()
            .map(|c| c.path.as_str())
            .collect();

        // ── Run the pure triage function ──────────────────────────────────
        let triage_result = triage::triage_changes(
            local_changes,
            remote_changes,
            &stored_states,
            remote_prefix,
            self.root_dir.as_path(),
        );

        // Collect results
        let mut local_to_upload = Vec::new();
        let mut remote_to_apply = triage_result.downloads;
        let mut conflicts: Vec<Conflict> = Vec::new();

        // Hold back any clean local change for a path that still has an
        // unresolved conflict. Otherwise, once the remote change feed has
        // advanced past the conflicting remote edit, the next cycle would see
        // the still-pending local edit as a plain upload and overwrite the
        // remote version, silently resolving the conflict.
        for change in triage_result.uploads {
            if conflicted_paths.contains(change.path()) {
                warn!(
                    "Holding local change for unresolved conflict ({}): {}",
                    change.path(),
                    change.change_type()
                );
            } else {
                local_to_upload.push(change);
            }
        }

        // ── Handle needs_comparison pairs (requires I/O) ──────────────────
        self.resolve_needs_comparison(
            &triage_result.needs_comparison,
            &stored_states,
            dry_run,
            &mut conflicts,
        )
        .await?;

        // ── Handle remote deletes ─────────────────────────────────────────
        self.handle_remote_deletes(
            &triage_result.remote_deletes,
            &stored_states,
            &mut remote_to_apply,
            &mut conflicts,
        )?;

        // Hold back any remote change for a path that still has an unresolved
        // conflict: overwriting the local file with the remote version would
        // silently drop the pending local edit. The path is released only when
        // the conflict is explicitly resolved (which deletes its record).
        remote_to_apply.retain(|change| {
            let keep = !conflicted_paths.contains(change.path());
            if !keep {
                warn!(
                    "Holding remote change for unresolved conflict ({}): {}",
                    change.path(),
                    change.change_type()
                );
            }
            keep
        });

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

    /// Load the stored states for the deduped union of local and remote change
    /// paths with a single batch query instead of one per-path
    /// `get_file_state` call.
    async fn load_stored_states(
        &self,
        local_changes: &[Change],
        remote_changes: &[Change],
    ) -> Result<HashMap<String, FileState>> {
        let mut paths_to_lookup: HashSet<String> = HashSet::new();
        for lc in local_changes {
            paths_to_lookup.insert(lc.path().to_string());
        }
        for rc in remote_changes {
            paths_to_lookup.insert(self.couchdb.get_local_path(rc.path()));
        }
        self.local_db
            .get_file_states(
                &paths_to_lookup
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
            .await
    }

    /// Index remote changes by remote path once so conflict detection is linear
    /// in the number of changes instead of quadratic (a `.find()` per local
    /// change). `or_insert` keeps the first change for a given path, matching
    /// the previous `.find()` which returned the first match; the whole index
    /// is order-independent across local changes.
    fn index_remote_changes(remote_changes: &[Change]) -> HashMap<&str, &Change> {
        let mut remote_by_path: HashMap<&str, &Change> = HashMap::new();
        for rc in remote_changes {
            remote_by_path.entry(rc.path()).or_insert(rc);
        }
        remote_by_path
    }

    fn debug_local_changes(
        &self,
        local_changes: &[Change],
        stored_states: &HashMap<String, FileState>,
        remote_by_path: &HashMap<&str, &Change>,
    ) {
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
            if let Some(rc) = remote_by_path.get(remote_path.as_str()) {
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
                    info!(
                        "  [REMOTE CHANGE DETECTED] {} - no remote mtime available, assuming changed",
                        lc.path()
                    );
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
    }

    async fn resolve_needs_comparison(
        &self,
        decisions: &[triage::TriageDecision],
        stored_states: &HashMap<String, FileState>,
        dry_run: bool,
        conflicts: &mut Vec<Conflict>,
    ) -> Result<()> {
        for decision in decisions {
            // An edit/delete race (local-delete vs remote-edit, or
            // remote-delete vs local-edit/untracked) is inherently a conflict:
            // there is no shared content to compare. Record it directly.
            let local_is_delete = matches!(
                decision.local_change.as_ref().map(Change::change_type),
                Some(ChangeType::Deleted)
            );
            if matches!(
                decision.remote_change.as_ref().map(Change::change_type),
                Some(ChangeType::Deleted)
            ) || local_is_delete
            {
                self.record_edit_delete_conflict(
                    decision,
                    local_is_delete,
                    stored_states,
                    conflicts,
                )
                .await?;
                continue;
            }

            self.resolve_content_comparison(decision, dry_run, conflicts)
                .await?;
        }

        Ok(())
    }

    /// Compare the actual content of a both-edited change pair to decide
    /// between a silent state update (identical content) and a conflict.
    async fn resolve_content_comparison(
        &self,
        decision: &triage::TriageDecision,
        dry_run: bool,
        conflicts: &mut Vec<Conflict>,
    ) -> Result<()> {
        let Some(lc) = &decision.local_change else {
            return Ok(());
        };
        let Some(_rc) = &decision.remote_change else {
            return Ok(());
        };

        let remote_path = self.couchdb.get_remote_path(lc.path());
        debug!("  => Remote is newer, fetching content to compare...");

        // Fetch remote metadata
        let Some(remote_doc) = self.couchdb.fetch_metadata(&remote_path).await? else {
            debug!("  Remote document not found!");
            return Ok(());
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
            let remote_modified_at = Utc
                .timestamp_millis_opt(i64::try_from(remote_doc.mtime.as_u64()).unwrap_or(i64::MAX))
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

        Ok(())
    }

    fn handle_remote_deletes(
        &self,
        remote_deletes: &[Change],
        stored_states: &HashMap<String, FileState>,
        remote_to_apply: &mut Vec<Change>,
        conflicts: &mut Vec<Conflict>,
    ) -> Result<()> {
        for rc in remote_deletes {
            let local_path = self.couchdb.get_local_path(rc.path());
            let file_path = self.root_dir.join_contained(&local_path)?;
            let stored_state = stored_states.get(&local_path);
            let local_mtime = file_path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(Into::into);
            // Protect same-mtime/preserved-mtime content edits: when the on-disk
            // mtime matches the stored sync-time mtime, still compare content so
            // an edit that kept the mtime is not mistaken for "unchanged".
            let local_content_matches = match (stored_state, file_path.exists()) {
                (Some(state), true) => compute_file_hash(&file_path)? == state.hash,
                _ => true,
            };
            match triage::arbitrate_remote_delete(
                stored_state,
                rc.rev(),
                rc.mtime().copied(),
                local_mtime,
                local_content_matches,
            ) {
                triage::RemoteDeleteVerdict::Apply => {
                    debug!("  Remote delete is newer than last sync, scheduling local delete");
                    remote_to_apply.push(rc.clone());
                }
                triage::RemoteDeleteVerdict::Conflict => {
                    // A remote delete racing a local edit (including a
                    // same-mtime content edit the pure triage pass could not
                    // see): record a conflict so the local edit is preserved.
                    debug!("  Remote delete races a local edit, recording conflict");
                    let current_hash = if file_path.exists() {
                        compute_file_hash(&file_path)?
                    } else {
                        String::new()
                    };
                    let local_state = FileState {
                        path: local_path.clone(),
                        hash: current_hash,
                        size: file_path.metadata().map_or(0, |m| m.len()),
                        modified_at: local_mtime.unwrap_or_else(Utc::now),
                        couch_rev: stored_state.and_then(|s| s.couch_rev.clone()),
                        last_sync_at: stored_state.map_or_else(Utc::now, |s| s.last_sync_at),
                    };
                    let remote_state = RemoteState {
                        hash: String::new(),
                        size: 0,
                        modified_at: rc.mtime().copied().unwrap_or_else(Utc::now),
                        couch_rev: rc.rev().and_then(CouchRev::new).unwrap_or_default(),
                        deleted: true,
                    };
                    conflicts.push(Conflict::new(local_path.clone(), local_state, remote_state));
                }
                triage::RemoteDeleteVerdict::Skip => {
                    if file_path.exists() || stored_state.is_some() {
                        debug!("  Remote delete is stale or file is untracked locally, skipping");
                    } else {
                        debug!("  Remote deleted, no local file/state, skipping");
                    }
                }
            }
        }
        Ok(())
    }

    /// Record a conflict for an edit/delete race (local-delete vs remote-edit,
    /// or remote-delete vs local-edit/untracked). There is no shared content to
    /// compare, so the conflict is recorded directly.
    async fn record_edit_delete_conflict(
        &self,
        decision: &triage::TriageDecision,
        local_is_delete: bool,
        stored_states: &HashMap<String, FileState>,
        conflicts: &mut Vec<Conflict>,
    ) -> Result<()> {
        let remote_path = self.couchdb.get_remote_path(&decision.path);
        let remote_state = if let Some(doc) = self.couchdb.fetch_metadata(&remote_path).await? {
            // Remote state from the tombstone/edit document.
            let remote_hash = if doc.hash.is_empty() {
                let content = self.couchdb.get_file_content(&remote_path).await?;
                compute_bytes_hash(&content)
            } else {
                doc.hash.clone()
            };
            RemoteState {
                hash: remote_hash,
                size: doc.size,
                modified_at: doc.modified_at(),
                couch_rev: doc
                    .rev
                    .as_deref()
                    .and_then(CouchRev::new)
                    .unwrap_or_default(),
                deleted: doc.deleted,
            }
        } else {
            // Remote doc gone (e.g. a hard delete with no tombstone body):
            // fall back to the incoming change's metadata.
            let rc = decision.remote_change.as_ref().unwrap();
            RemoteState {
                hash: String::new(),
                size: 0,
                modified_at: rc.mtime().copied().unwrap_or_else(Utc::now),
                couch_rev: rc.rev().and_then(CouchRev::new).unwrap_or_default(),
                deleted: true,
            }
        };
        if let Some(local_state) = self
            .local_state_for_decision(&decision.path, local_is_delete, stored_states)
            .await?
        {
            conflicts.push(Conflict::new(
                decision.path.clone(),
                local_state,
                remote_state,
            ));
        }
        Ok(())
    }

    /// Build the local `FileState` recorded in an edit/delete conflict.
    ///
    /// * Local-delete side: the local file is gone, so use the last stored
    ///   state (the pre-delete baseline). Returns `None` when there is no
    ///   tracked base to arbitrate against (the delete is then propagated).
    /// * Remote-delete side: the local file is edited or untracked and still on
    ///   disk, so prefer the current on-disk state, falling back to the stored
    ///   state if the on-disk read fails.
    async fn local_state_for_decision(
        &self,
        path: &str,
        local_is_delete: bool,
        stored_states: &HashMap<String, FileState>,
    ) -> Result<Option<FileState>> {
        if local_is_delete {
            return Ok(stored_states.get(path).cloned());
        }
        if let Ok(state) = self.get_local_state(path).await {
            return Ok(Some(state));
        }
        Ok(stored_states.get(path).cloned())
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

    /// Upload one local file to `CouchDB` (delegates to the shared apply worker).
    async fn upload_local_file(
        &self,
        local_path: &str,
        remote_path: &str,
        expected_rev: &ExpectedRemote,
    ) -> Result<(usize, Option<String>)> {
        self.apply_worker()
            .upload_local_file(local_path, remote_path, expected_rev)
            .await
    }

    /// Download one remote file to the local filesystem (delegates to the shared apply worker).
    async fn download_remote_file(
        &self,
        remote_path: &str,
        local_path: &str,
        require_doc: bool,
        protect_local_edits: bool,
    ) -> Result<Option<usize>> {
        self.apply_worker()
            .download_remote_file(remote_path, local_path, require_doc, protect_local_edits)
            .await
    }

    /// Apply a change to `CouchDB` (delegates to the shared apply worker).
    async fn apply_to_couchdb(&self, change: &Change) -> Result<()> {
        self.apply_worker().apply_to_couchdb(change).await
    }

    /// Apply a change to the local filesystem (delegates to the shared apply worker).
    async fn apply_to_filesystem(&self, change: &Change) -> Result<()> {
        self.apply_worker().apply_to_filesystem(change).await
    }

    /// Get list of conflicts
    ///
    /// # Errors
    ///
    /// Returns an error if the local database cannot be read.
    pub async fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        self.local_db.get_conflicts().await
    }

    /// Record a conflict in the local database (used by live sync, where an
    /// incoming change is arbitrated immediately rather than via a batch).
    ///
    /// # Errors
    ///
    /// Returns an error if the conflict cannot be persisted.
    pub async fn record_conflict(&self, conflict: &Conflict) -> Result<()> {
        self.local_db.store_conflict(conflict).await
    }

    /// Arbitrate a live local delete against the remote.
    ///
    /// Returns `true` when the remote has been edited since our last sync and a
    /// delete/edit conflict was recorded (the local delete must NOT propagate,
    /// or it would destroy the remote edit). Returns `false` when the delete may
    /// propagate (remote absent, also deleted, or unchanged since our last
    /// sync).
    ///
    /// # Errors
    ///
    /// Returns an error if the remote metadata or content cannot be fetched or
    /// the conflict cannot be persisted.
    pub async fn arbitrate_local_delete(&self, local_path: &str) -> Result<bool> {
        let Some(state) = self.get_file_state(local_path).await? else {
            return Ok(false);
        };
        let remote_path = self.couchdb.get_remote_path(local_path);
        let Some(remote_doc) = self.couchdb.fetch_metadata(&remote_path).await? else {
            return Ok(false);
        };
        if remote_doc.deleted {
            // Both sides deleted → the local delete is an idempotent no-op.
            return Ok(false);
        }
        let remote_rev_changed = remote_doc
            .rev
            .as_deref()
            .is_some_and(|r| triage::remote_revision_changed(Some(r), state.couch_rev.as_deref()));
        if !remote_rev_changed {
            // Remote unchanged since our last sync → propagate the delete.
            return Ok(false);
        }
        // Remote edited since our last sync → record a delete/edit conflict.
        let remote_hash = if remote_doc.hash.is_empty() {
            compute_bytes_hash(&self.couchdb.get_file_content(&remote_path).await?)
        } else {
            remote_doc.hash.clone()
        };
        let remote_state = RemoteState {
            hash: remote_hash,
            size: remote_doc.size,
            modified_at: remote_doc.modified_at(),
            couch_rev: remote_doc
                .rev
                .as_deref()
                .and_then(CouchRev::new)
                .unwrap_or_default(),
            deleted: false,
        };
        self.record_conflict(&Conflict::new(local_path.to_string(), state, remote_state))
            .await?;
        Ok(true)
    }

    /// Apply a local change immediately (live sync)
    ///
    /// # Errors
    ///
    /// Returns an error if the change cannot be applied to `CouchDB`.
    pub async fn apply_local_change(&mut self, change: &Change) -> Result<()> {
        self.apply_to_couchdb(change).await
    }

    /// Apply a remote change immediately (live sync)
    ///
    /// # Errors
    ///
    /// Returns an error if the change cannot be applied to the filesystem.
    pub async fn apply_remote_change(&mut self, change: &Change) -> Result<()> {
        self.apply_to_filesystem(change).await
    }

    /// Get local tracked file state
    ///
    /// # Errors
    ///
    /// Returns an error if the local database cannot be read.
    pub async fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        self.local_db.get_file_state(path).await
    }

    /// Save sync checkpoint
    ///
    /// # Errors
    ///
    /// Returns an error if the checkpoint cannot be persisted.
    pub async fn save_checkpoint(&self, seq: &str) -> Result<()> {
        self.local_db.save_checkpoint(seq).await
    }

    /// Get sync checkpoint
    ///
    /// # Errors
    ///
    /// Returns an error if the local database cannot be read.
    pub async fn get_checkpoint(&self) -> Result<Option<Checkpoint>> {
        self.local_db.get_checkpoint().await
    }

    /// Convert local path to remote path using the configured prefix
    #[must_use]
    pub fn local_to_remote_path(&self, local_path: &str) -> String {
        self.couchdb.get_remote_path(local_path)
    }

    /// Convert remote path to local path by stripping the configured prefix
    #[must_use]
    pub fn remote_to_local_path(&self, remote_path: &str) -> String {
        self.couchdb.get_local_path(remote_path)
    }

    /// Get remote file content (converts local path to remote path)
    ///
    /// # Errors
    ///
    /// Returns an error if the remote file or any of its chunks cannot be
    /// fetched.
    pub async fn get_remote_content(&self, local_path: &str) -> Result<Vec<u8>> {
        let remote_path = self.couchdb.get_remote_path(local_path);
        self.couchdb.get_file_content(&remote_path).await
    }

    /// Get the root directory
    #[must_use]
    pub const fn root_dir(&self) -> &SyncDirPath {
        &self.root_dir
    }

    /// Get the ignore matcher (for testing)
    #[must_use]
    pub const fn ignore_matcher(&self) -> &IgnoreMatcher {
        &self.ignore_matcher
    }

    /// The effective incoming path policy: whether an incoming remote path
    /// (expressed as its local-side path) must be excluded from being applied
    /// locally. This is the single source of truth for incoming exclusions —
    /// it delegates to [`IgnoreMatcher::should_ignore`], which applies the
    /// configured patterns AND unconditionally protects the internal state
    /// paths (`.couchdb-file-sync`, `.couchfs`, `.sync-ignore`) even when a
    /// negation pattern would otherwise re-include them.
    #[must_use]
    fn incoming_path_is_ignored(&self, local_path: &str) -> bool {
        self.ignore_matcher
            .should_ignore(std::path::Path::new(local_path))
    }

    /// Resolve a conflict
    /// Note: `local_path` is the local file path (stored in conflict), which gets
    /// converted to remote path when interacting with `CouchDB`
    ///
    /// # Errors
    ///
    /// Returns an error if no conflict exists for the path or the chosen
    /// resolution strategy fails to apply.
    pub async fn resolve_conflict(
        &mut self,
        local_path: &str,
        strategy: ResolutionStrategy,
    ) -> Result<()> {
        let Some(_conflict) = self.local_db.get_conflict(local_path).await? else {
            anyhow::bail!("No conflict found for path: {local_path}");
        };

        // Convert local path to remote path for CouchDB operations
        let remote_path = self.couchdb.get_remote_path(local_path);
        let file_path = self.root_dir.join_contained(local_path)?;
        let local_exists = file_path.exists();
        // Determine whether the remote side is currently a deletion (tombstone)
        // so "keep remote" / "keep both" delete or skip rather than resurrecting
        // the tombstone's stale content. The stored conflict does not persist
        // this, so it is re-read from the live remote at resolution time.
        let remote_is_tombstone = self
            .couchdb
            .fetch_metadata(&remote_path)
            .await?
            .is_some_and(|doc| doc.deleted);

        match strategy {
            ResolutionStrategy::KeepLocal => {
                if local_exists {
                    // Keep the local edit: upload it (re-creating the remote
                    // file with the edited content). Keep-local is an explicit
                    // resolution, so publication is unconditional (bypasses the
                    // conditional stale-baseline check used by normal sync).
                    self.upload_local_file(local_path, &remote_path, &ExpectedRemote::Force)
                        .await?;
                    info!(
                        "Resolved conflict (keep-local): {} - uploaded to remote",
                        local_path
                    );
                } else {
                    // The local side was deleted: choosing it propagates the
                    // deletion rather than resurrecting anything.
                    self.couchdb.delete_file(&remote_path).await?;
                    info!(
                        "Resolved conflict (keep-local delete): {} - deleted on remote",
                        local_path
                    );
                }
            }
            ResolutionStrategy::KeepRemote => {
                if remote_is_tombstone {
                    // The remote side is a tombstone: "keep remote" means
                    // deleting the local file to match — NOT resurrecting the
                    // tombstone's stale content.
                    if local_exists {
                        tokio::fs::remove_file(&file_path).await?;
                    }
                    self.local_db.delete_file_state(local_path).await?;
                    info!(
                        "Resolved conflict (keep-remote delete): {} - deleted locally",
                        local_path
                    );
                } else {
                    // Force download remote version
                    self.download_remote_file(&remote_path, local_path, true, false)
                        .await?;
                    info!("Resolved conflict (keep-remote): {}", local_path);
                }
            }
            ResolutionStrategy::KeepBoth => {
                if remote_is_tombstone {
                    // Nothing on the remote to merge: the tombstone has no
                    // content, so keep the local file as-is.
                    info!(
                        "Resolved conflict (keep-both, remote deleted): {} - keeping local",
                        local_path
                    );
                } else {
                    // Save remote as .remote file
                    let local_remote_path = format!("{local_path}.remote");
                    self.download_remote_file(&remote_path, &local_remote_path, true, false)
                        .await?;
                    info!("Saved remote version as: {}", local_remote_path);

                    // Local file stays as-is
                    // User can manually merge/compare
                }
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
    use crate::couchdb::CouchDb;
    use crate::couchdb::db::{CannedCouch, ConcurrencyProbe};
    use crate::local::{LocalDb, compute_bytes_hash, compute_file_hash};
    use crate::models::{Change, CouchRev, FileDoc, IgnoreMatcher, TimestampMillis};
    use chrono::{Duration, Utc};
    use std::path::PathBuf;

    /// Create a minimal `CouchDb` instance for testing construction.
    /// Not all methods work without a real server.
    fn test_couchdb() -> CouchDb {
        CouchDb::for_test("test-prefix/")
    }

    fn test_local_db() -> LocalDb {
        LocalDb::open_in_memory().expect("in-memory LocalDb should construct")
    }

    fn test_root(path: &str) -> SyncDirPath {
        std::fs::create_dir_all(path).expect("create test dir");
        SyncDirPath::new(&PathBuf::from(path)).expect("create SyncDirPath")
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
        assert!(
            !engine
                .ignore_matcher()
                .should_ignore(std::path::Path::new("test.txt"))
        );
    }

    #[test]
    fn engine_with_ignore_constructs_with_custom_ignore_matcher() {
        let couch = test_couchdb();
        let local = test_local_db();
        let root = test_root("/tmp/test-sync-engine-with-ignore");

        let matcher = IgnoreMatcher::from_content("*.log\nnode_modules/");
        let engine = SyncEngine::with_ignore(couch, local, root, matcher);

        assert!(
            engine
                .ignore_matcher()
                .should_ignore(std::path::Path::new("debug.log"))
        );
        assert!(
            engine
                .ignore_matcher()
                .should_ignore(std::path::Path::new("node_modules/pkg/index.js"))
        );
        assert!(
            !engine
                .ignore_matcher()
                .should_ignore(std::path::Path::new("src/main.rs"))
        );
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
        assert!(
            engine
                .get_file_state("nonexistent.txt")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            engine
                .get_file_state("other/path.md")
                .await
                .unwrap()
                .is_none()
        );
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
        let root = SyncDirPath::new(dir.path()).unwrap();
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
        let root = SyncDirPath::new(dir.path()).unwrap();
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
        assert_eq!(report.conflicts, 0);
        assert_eq!(report.errors, [] as [String; 0]);

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
    async fn prefix_named_nested_path_survives_cycles_and_deletion_propagates() {
        // A legitimate local subtree whose name coincides with the remote prefix
        // (local notes/a.txt under a remote notes/ scope) must remain tracked
        // across modification and restart, and its deletion must propagate.
        // The old lexical prefix purge deleted any state whose path started with
        // the prefix, dropping the baseline so a later local deletion was never
        // detected (it failed to propagate). Regression for story #6342.
        let dir = tempfile::tempdir().unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // Keep the state DB outside the sync root so the scanner never sees it.
        let state_db = db_dir.path().join("state.db");
        let notes = root.as_path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let file = notes.join("a.txt");
        std::fs::write(&file, "v1").unwrap();

        let remote_prefix = "notes/";
        let make_engine = |local: LocalDb| {
            let canned = CannedCouch {
                changes: Vec::new(),
                last_seq: "0".to_string(),
                ..Default::default()
            };
            SyncEngine::with_ignore(
                test_canned_couch(remote_prefix, canned),
                local,
                root.clone(),
                IgnoreMatcher::empty(),
            )
        };

        // Cycle 1: the new prefix-named file is created and tracked.
        {
            let mut engine = make_engine(LocalDb::open(&state_db).unwrap());
            let report = engine.sync().await.expect("sync cycle 1");
            assert_eq!(report.uploaded.0, 1, "new prefix-named file uploaded");
            assert_eq!(report.deleted_remote, 0);
        }

        // Restart: a fresh engine over the same DB + disk sees no change — the
        // prefix-named state is retained, not purged and re-uploaded.
        {
            let mut engine = make_engine(LocalDb::open(&state_db).unwrap());
            let report = engine.sync().await.expect("sync cycle 2 (restart)");
            assert_eq!(
                report.uploaded.0, 0,
                "unchanged prefix-named file must not be re-uploaded after restart"
            );
            assert_eq!(
                report.deleted_remote, 0,
                "must not infer a remote deletion from the prefix-named path"
            );
            assert!(
                engine
                    .get_file_state("notes/a.txt")
                    .await
                    .unwrap()
                    .is_some(),
                "prefix-named state must be retained across restart"
            );
        }

        // Modify the file → the change propagates as an upload.
        std::fs::write(&file, "a-longer-content-v2").unwrap();
        {
            let mut engine = make_engine(LocalDb::open(&state_db).unwrap());
            let report = engine.sync().await.expect("sync cycle 3 (modify)");
            assert_eq!(report.uploaded.0, 1, "modification propagates");
        }

        // Delete the file → the deletion propagates to the remote scope. With the
        // old purge the baseline had been dropped, so this reported 0 deletions.
        std::fs::remove_file(&file).unwrap();
        {
            let mut engine = make_engine(LocalDb::open(&state_db).unwrap());
            let report = engine.sync().await.expect("sync cycle 4 (delete)");
            assert_eq!(
                report.deleted_remote, 1,
                "deletion of the prefix-named file must propagate"
            );
        }
    }

    #[tokio::test]
    async fn dry_run_counts_downloads_without_writing_files_or_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
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
                    format!("{remote_prefix}foo.txt"),
                    "someremotehash".to_string(),
                    11,
                    Utc::now(),
                    "2-def".to_string(),
                ),
                // Brand new remote file -> download
                Change::remote_created(
                    format!("{remote_prefix}bar.txt"),
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
        assert_eq!(report.conflicts, 0);

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
        let root = SyncDirPath::new(dir.path()).unwrap();
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
            ..CannedCouch::default()
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
    async fn detect_conflicts_is_order_independent_on_mixed_change_set() {
        // Regression for the quadratic conflict scan: detect_conflicts used to
        // run a `.find()` over every remote change for each local change. The
        // refactor builds a HashMap<&str, &Change> index once instead. This
        // test drives a mixed local/remote change set (a local-only upload, a
        // remote-only download, and a both-sides content-differing conflict)
        // and asserts the same triage result regardless of slice ordering.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        // A local file changed on both sides: its stored rev is stale, so the
        // advanced remote rev marks the remote as changed, and its on-disk
        // content differs from the remote content -> conflict.
        std::fs::write(root.as_path().join("b.txt"), b"local-content").unwrap();
        let local = test_local_db();
        seed_file_state(&local, "b.txt", "stalehash", 13);
        let mut stored = local.get_file_state("b.txt").unwrap().unwrap();
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_path = "prefix/b.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 14);
        remote_doc.rev = Some("2-def".to_string());
        remote_doc.mtime = TimestampMillis::now();
        remote_doc.path = remote_path.to_string();
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc.clone(),
            )]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                b"remote-content".to_vec(),
            )]),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let remote_modified = Change::remote_modified(
            remote_path.to_string(),
            "remotehash".to_string(),
            14,
            Utc::now(),
            "2-def".to_string(),
        );

        let sort_paths = |paths: Vec<String>| -> Vec<String> {
            let mut v = paths;
            v.sort();
            v
        };

        // Same logical change set presented in two different slice orderings.
        for (local_order, remote_order) in [
            (vec!["b.txt", "a.txt"], vec!["prefix/c.txt", "prefix/b.txt"]),
            (vec!["a.txt", "b.txt"], vec!["prefix/b.txt", "prefix/c.txt"]),
        ] {
            let local_changes = local_order
                .iter()
                .map(|p| {
                    if *p == "b.txt" {
                        Change::local_modified(p.to_string(), "localhash".to_string(), 13)
                    } else {
                        Change::local_created(p.to_string(), "ahash".to_string(), 3)
                    }
                })
                .collect::<Vec<_>>();
            let remote_changes = remote_order
                .iter()
                .map(|p| {
                    if *p == "prefix/b.txt" {
                        remote_modified.clone()
                    } else {
                        Change::remote_created(
                            p.to_string(),
                            "chash".to_string(),
                            5,
                            Utc::now(),
                            "3-gh".to_string(),
                        )
                    }
                })
                .collect::<Vec<_>>();

            let (uploads, downloads, conflicts) = engine
                .detect_conflicts(&local_changes, &remote_changes, true)
                .await
                .expect("detect_conflicts should succeed");

            assert_eq!(
                sort_paths(uploads.iter().map(|c| c.path().to_string()).collect()),
                vec!["a.txt"],
                "local-only change uploads regardless of ordering"
            );
            assert_eq!(
                sort_paths(downloads.iter().map(|c| c.path().to_string()).collect()),
                vec!["prefix/c.txt"],
                "remote-only change downloads regardless of ordering"
            );
            assert_eq!(
                sort_paths(conflicts.iter().map(|c| c.path.clone()).collect()),
                vec!["b.txt"],
                "both-sides differing change conflicts regardless of ordering"
            );
        }
    }

    #[tokio::test]
    async fn dry_run_does_not_save_state_for_identical_content() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
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
            ..CannedCouch::default()
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
        let root = SyncDirPath::new(dir.path()).unwrap();
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

    #[tokio::test]
    async fn failed_download_does_not_advance_checkpoint() {
        // A failed content download must not advance the remote change-feed
        // checkpoint, or the change would be silently dropped and never retried.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let local = test_local_db();
        local.save_checkpoint("100-before").unwrap();

        let remote_path = "prefix/doc.txt";
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
            last_seq: "200-after".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            content_errors: std::collections::HashSet::from([remote_path.to_string()]),
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine
            .sync()
            .await
            .expect("sync with a failed download must not abort");

        assert!(
            !report.errors.is_empty(),
            "the simulated content-download failure must be collected into report.errors"
        );

        let cp = engine.get_checkpoint().await.unwrap();
        assert_eq!(
            cp.map(|c| c.last_seq),
            Some("100-before".to_string()),
            "checkpoint must not advance past a failed download"
        );
    }

    // ── Unchanged-tree scan reuses stored hashes (#3005) ──────────────────

    #[tokio::test]
    async fn unchanged_tree_sync_reuses_stored_hashes_without_rehashing() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let file_path = root.as_path().join("keep.txt");
        std::fs::write(&file_path, "stable content").unwrap();

        // Make the file not racily clean so the mtime+size shortcut applies.
        let old = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
        filetime::set_file_mtime(&file_path, filetime::FileTime::from_system_time(old)).unwrap();

        // Seed a fully-tracked, unchanged file whose stored mtime/size/hash all
        // match the on-disk scan.
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

        let report = engine.sync().await.expect("sync unchanged tree");
        assert_eq!(report.uploaded.0, 0);
        assert_eq!(report.downloaded.0, 0);
        assert_eq!(report.conflicts, 0);

        // Because the stored mtime AND size match and the file is not racily
        // clean, the full scan reuses the stored hash: zero file re-reads.
        assert_eq!(
            engine.scanner.hash_computations(),
            0,
            "unchanged tree sync must not re-read/re-hash any file"
        );

        // A second sync over the exact same tree still does not re-hash.
        let report2 = engine.sync().await.expect("second sync unchanged tree");
        assert_eq!(report2.uploaded.0, 0);
        assert_eq!(report2.downloaded.0, 0);
        assert_eq!(report2.conflicts, 0);
        assert_eq!(
            engine.scanner.hash_computations(),
            0,
            "repeated sync of an unchanged tree must never re-hash"
        );

        // Observables unchanged: couch_rev, last_sync_at, and hash preserved.
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
        let root = SyncDirPath::new(dir.path()).unwrap();
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
        let root = SyncDirPath::new(dir.path()).unwrap();
        let content: Vec<u8> = b"upload content bytes".to_vec();
        std::fs::write(root.as_path().join("up.txt"), &content).unwrap();

        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let (bytes_sent, _rev) = engine
            .upload_local_file("up.txt", "prefix/up.txt", &ExpectedRemote::Force)
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

    // ── Conditional publication (#6338): reject a stale baseline ──────────

    /// Helper: a remote doc at the given rev, optionally with old chunk ids.
    fn remote_doc_at(remote_path: &str, rev: &str, children: Vec<String>) -> FileDoc {
        let mut doc = FileDoc::new(remote_path.to_string(), String::new(), 4);
        doc.rev = Some(rev.to_string());
        doc.path = remote_path.to_string();
        doc.children = children;
        doc
    }

    #[tokio::test]
    async fn upload_rejects_concurrent_remote_edit_between_plan_and_save() {
        // Planned at rev 1-abc; a concurrent writer moved the remote to 2-def
        // before the metadata save. Publication must be rejected, not overwrite.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("up.txt"), b"local content").unwrap();

        let remote_path = "prefix/up.txt";
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc_at(remote_path, "2-def", vec!["old-chunk".to_string()]),
            )]),
            ..CannedCouch::default()
        };
        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let err = engine
            .upload_local_file(
                "up.txt",
                remote_path,
                &ExpectedRemote::At("1-abc".to_string()),
            )
            .await
            .expect_err("a concurrent remote edit must reject the upload");
        let msg = err.to_string();
        assert!(
            msg.contains("concurrent remote change detected"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("expected rev 1-abc, found Some(\"2-def\")"),
            "error must name the planned vs current revs: {msg}"
        );
        // Only the chunk upload happened; the metadata save (and old-chunk
        // cleanup) never ran, so the concurrent remote edit is preserved.
        assert_eq!(engine.couchdb.test_write_calls(), 1, "no save on rejection");
    }

    #[tokio::test]
    async fn upload_rejects_remote_creation_race() {
        // Planned as a create (remote absent); a concurrent writer created the
        // doc before the metadata save. Must not overwrite the new doc.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("up.txt"), b"local content").unwrap();

        let remote_path = "prefix/up.txt";
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc_at(remote_path, "1-abc", Vec::new()),
            )]),
            ..CannedCouch::default()
        };
        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let err = engine
            .upload_local_file("up.txt", remote_path, &ExpectedRemote::Absent)
            .await
            .expect_err("a concurrent remote creation must reject the upload");
        let msg = err.to_string();
        assert!(
            msg.contains("concurrent remote creation detected"),
            "unexpected error: {msg}"
        );
        assert_eq!(engine.couchdb.test_write_calls(), 1, "no save on rejection");
    }

    #[tokio::test]
    async fn upload_conditional_overwrite_succeeds_when_rev_matches() {
        // Planned at 1-abc and the remote is still at 1-abc: authorized, the
        // overwrite proceeds and the old chunk is cleaned up.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("up.txt"), b"local content").unwrap();

        let remote_path = "prefix/up.txt";
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc_at(remote_path, "1-abc", vec!["old-chunk".to_string()]),
            )]),
            ..CannedCouch::default()
        };
        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let (bytes, rev) = engine
            .upload_local_file(
                "up.txt",
                remote_path,
                &ExpectedRemote::At("1-abc".to_string()),
            )
            .await
            .expect("a matching revision authorizes the overwrite");
        assert_eq!(bytes, b"local content".len());
        assert_eq!(rev, Some("1-abc".to_string()));
        // chunk upload + save + old-chunk cleanup all ran.
        assert_eq!(engine.couchdb.test_write_calls(), 3);
    }

    #[tokio::test]
    async fn upload_force_overwrite_bypasses_rev_check() {
        // Explicit force (keep-local resolution / remote rebuild) must still
        // overwrite regardless of the current remote revision.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("up.txt"), b"local content").unwrap();

        let remote_path = "prefix/up.txt";
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc_at(remote_path, "9-zzz", vec!["old-chunk".to_string()]),
            )]),
            ..CannedCouch::default()
        };
        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let (bytes, rev) = engine
            .upload_local_file("up.txt", remote_path, &ExpectedRemote::Force)
            .await
            .expect("explicit force overwrites any remote revision");
        assert_eq!(bytes, b"local content".len());
        assert_eq!(rev, Some("9-zzz".to_string()));
        assert_eq!(engine.couchdb.test_write_calls(), 3);
    }

    #[tokio::test]
    async fn sync_rejects_concurrent_remote_edit_after_triage() {
        // End-to-end: triage plans an overwrite of remote rev 1-abc, but the
        // remote doc is at 2-def by the time the upload runs. The upload is
        // rejected (recorded as an error) and the concurrent remote edit is
        // preserved instead of overwritten.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("b.txt"), b"local content").unwrap();

        let remote_path = "prefix/b.txt";
        // Triage sees a remote change at rev 1-abc and a stored state matching
        // it, so it plans a conditional overwrite against 1-abc.
        let local = test_local_db();
        seed_file_state(&local, "b.txt", "localhash", 13);
        let mut stored = local.get_file_state("b.txt").unwrap().unwrap();
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let canned = CannedCouch {
            changes: vec![Change::remote_modified(
                remote_path.to_string(),
                "remotehash".to_string(),
                13,
                Utc::now(),
                "1-abc".to_string(),
            )],
            last_seq: "200-after".to_string(),
            // The actual remote doc advanced to 2-def (a concurrent writer).
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc_at(remote_path, "2-def", vec!["old-chunk".to_string()]),
            )]),
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync().await.expect("sync with a racing remote edit");

        assert!(
            !report.errors.is_empty(),
            "the concurrent remote edit must surface as an upload error, not be overwritten"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("concurrent remote change detected")),
            "error must name the conditional rejection: {:?}",
            report.errors
        );
    }

    #[tokio::test]
    async fn sync_rejects_remote_creation_race_after_triage() {
        // End-to-end: triage plans a create (remote absent in the change feed),
        // but a concurrent writer created the doc before the upload runs. The
        // create is rejected instead of overwriting the new remote doc.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("new.txt"), b"local content").unwrap();

        let remote_path = "prefix/new.txt";
        let canned = CannedCouch {
            changes: Vec::new(), // remote absent at triage -> plan a create
            last_seq: "200-after".to_string(),
            // ...but the doc now exists (a concurrent creation).
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc_at(remote_path, "1-abc", Vec::new()),
            )]),
            ..CannedCouch::default()
        };
        let local = test_local_db();
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine
            .sync()
            .await
            .expect("sync with a racing remote create");

        assert!(
            !report.errors.is_empty(),
            "the concurrent remote creation must surface as an upload error, not be overwritten"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("concurrent remote creation detected")),
            "error must name the creation race: {:?}",
            report.errors
        );
    }

    #[tokio::test]
    async fn download_saved_hash_matches_transferred_content_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
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
            ..CannedCouch::default()
        };
        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes_written = engine
            .download_remote_file(remote_path, "dl.txt", true, true)
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
        let root = SyncDirPath::new(dir.path()).unwrap();

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
            ..CannedCouch::default()
        };
        let engine_a = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned_a),
            local_a,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let err = engine_a
            .download_remote_file(remote_path, local_path, true, true)
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
            ..CannedCouch::default()
        };
        let engine_b = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned_b),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        engine_b
            .download_remote_file(remote_path2, local_path2, true, true)
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
        let root = SyncDirPath::new(dir.path()).unwrap();

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
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes = engine
            .download_remote_file(remote_path, "empty.txt", true, true)
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

    #[tokio::test]
    async fn download_remote_file_fails_on_hash_mismatch() {
        // Guard for #3624: when the remote doc records a content hash, a
        // download whose bytes do not match it must fail the file — no corrupt
        // local copy written, no sync state saved.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_path = "prefix/corrupt.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), "stored-remote-hash".into(), 3);
        remote_doc.rev = Some("1-abc".to_string());
        remote_doc.path = remote_path.to_string();

        // Canned content is "abc" whose real SHA-256 is not
        // "stored-remote-hash", so the verification must reject it.
        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::from([(remote_path.to_string(), b"abc".to_vec())]),
            content_errors: std::collections::HashSet::new(),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        engine
            .download_remote_file(remote_path, "corrupt.txt", true, true)
            .await
            .expect_err("hash mismatch must fail the download");

        assert!(
            !root.as_path().join("corrupt.txt").exists(),
            "hash mismatch must not write a corrupt local file"
        );
        assert!(
            engine
                .get_file_state("corrupt.txt")
                .await
                .unwrap()
                .is_none(),
            "hash mismatch must not save any FileState"
        );
    }

    #[tokio::test]
    async fn download_remote_file_succeeds_when_hash_matches() {
        // A download whose bytes match the stored remote hash must succeed and
        // save sync state (verification must not break the healthy path).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_path = "prefix/good.txt";
        let content = b"hello world";
        let stored_hash = compute_bytes_hash(content);
        let mut remote_doc = FileDoc::new(
            remote_path.to_string(),
            stored_hash.clone(),
            content.len() as u64,
        );
        remote_doc.rev = Some("1-good".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                content.to_vec(),
            )]),
            content_errors: std::collections::HashSet::new(),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes = engine
            .download_remote_file(remote_path, "good.txt", true, true)
            .await
            .expect("matching hash download should succeed")
            .expect("matching hash download should write a file");

        assert_eq!(bytes, content.len());
        assert_eq!(
            std::fs::read(root.as_path().join("good.txt")).unwrap(),
            content,
            "matching-hash download must write the correct bytes"
        );
        let stored = engine
            .get_file_state("good.txt")
            .await
            .unwrap()
            .expect("matching-hash download should save FileState");
        assert_eq!(
            stored.hash, stored_hash,
            "FileState hash must equal the verified remote hash"
        );
    }

    /// Helper: list any leftover `.download` staging residue in `dir`.
    fn staging_residue(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.ends_with(".download"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn download_protects_local_edit_during_delayed_transfer() {
        // Regression for #6340: a user edit that lands while a download is in
        // flight (delayed network fetch) must not be clobbered by the atomic
        // replacement. The download aborts, the user's bytes stay on disk,
        // the recorded sync state is unchanged, and no staging file leaks.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_path = "prefix/edit.txt";
        let local_path = "edit.txt";
        let baseline: Vec<u8> = b"baseline-content".to_vec();
        std::fs::write(root.as_path().join(local_path), &baseline).unwrap();

        let local = test_local_db();
        let mut stored = FileState::new(
            local_path.to_string(),
            compute_bytes_hash(&baseline),
            baseline.len() as u64,
            Utc::now() - chrono::Duration::days(1),
        );
        stored.couch_rev = Some(CouchRev::new("1-base").unwrap());
        stored.last_sync_at = Utc::now() - chrono::Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_content: Vec<u8> = b"remote-new-content".to_vec();
        let mut remote_doc = FileDoc::new(
            remote_path.to_string(),
            compute_bytes_hash(&remote_content),
            remote_content.len() as u64,
        );
        remote_doc.rev = Some("2-new".to_string());
        remote_doc.path = remote_path.to_string();

        // Delay the content fetch so the user edit lands mid-transfer.
        let probe = std::sync::Arc::new(ConcurrencyProbe::default());
        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "2".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_content.clone(),
            )]),
            content_errors: std::collections::HashSet::new(),
            probe: Some(probe),
            batch_delay: Some(std::time::Duration::from_millis(500)),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        // Simulate the user editing the file while the (delayed) download is
        // transferring: write new user bytes shortly after the fetch starts.
        let edit_dir = root.as_path().to_path_buf();
        let user_edit: Vec<u8> = b"user-edit-during-download".to_vec();
        let task_edit = user_edit.clone();
        let edit_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            std::fs::write(edit_dir.join("edit.txt"), &task_edit).unwrap();
        });

        let err = engine
            .download_remote_file(remote_path, local_path, true, true)
            .await
            .expect_err("download must abort rather than clobber the user edit");
        assert!(
            format!("{err:#}").contains("changed on disk"),
            "error should identify the protected local edit: {err:#}"
        );
        edit_task.await.unwrap();

        assert_eq!(
            std::fs::read(root.as_path().join(local_path)).unwrap(),
            user_edit,
            "user bytes edited during the download must remain intact"
        );
        assert!(
            staging_residue(dir.path()).is_empty(),
            "aborted download must leave no staging residue"
        );
        let state_after = engine
            .get_file_state(local_path)
            .await
            .unwrap()
            .expect("seeded baseline state must still be present");
        assert_eq!(
            state_after.hash,
            compute_bytes_hash(&baseline),
            "aborted download must not update the recorded sync state"
        );
    }

    #[tokio::test]
    async fn download_protects_untracked_local_file() {
        // A local file that exists without any tracked sync state is treated
        // as untracked local content and must not be overwritten by a
        // download. The file stays intact, no state is saved, no staging
        // residue remains.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_path = "prefix/untracked.txt";
        let local_path = "untracked.txt";
        std::fs::write(root.as_path().join(local_path), b"precious-local").unwrap();

        let remote_content: Vec<u8> = b"remote-content".to_vec();
        let mut remote_doc = FileDoc::new(
            remote_path.to_string(),
            compute_bytes_hash(&remote_content),
            remote_content.len() as u64,
        );
        remote_doc.rev = Some("1-remote".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::from([(remote_path.to_string(), remote_content)]),
            content_errors: std::collections::HashSet::new(),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let err = engine
            .download_remote_file(remote_path, local_path, true, true)
            .await
            .expect_err("download must abort to protect the untracked local file");
        assert!(
            format!("{err:#}").contains("untracked"),
            "error should identify the untracked local file: {err:#}"
        );
        assert_eq!(
            std::fs::read(root.as_path().join(local_path)).unwrap(),
            b"precious-local",
            "untracked local file must remain untouched"
        );
        assert!(
            staging_residue(dir.path()).is_empty(),
            "aborted download must leave no staging residue"
        );
        assert!(
            engine.get_file_state(local_path).await.unwrap().is_none(),
            "aborted download must not save any FileState"
        );
    }

    /// `get_conflicts()` synthesizes `local_state.last_sync_at` with `Utc::now()`
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
        assert_eq!(async_db.get_all_file_states().await.unwrap(), []);
    }

    #[tokio::test]
    async fn sync_engine_async_accessors_return_identical_state_after_blocking_move() {
        // Exercise the converted public accessors (get_file_state,
        // save_checkpoint, get_checkpoint, get_conflicts) end-to-end through a
        // SyncEngine: reads and writes go through spawn_blocking and must
        // return/update identical data.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
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
        assert!(
            engine
                .get_file_state("nonexistent.txt")
                .await
                .unwrap()
                .is_none()
        );
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
        let root = SyncDirPath::new(dir.path()).unwrap();
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
            None,
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

    #[tokio::test]
    async fn first_sync_downloads_pre_existing_remote_files() {
        // A fresh client with no checkpoint must materialize the pre-existing
        // remote file set on its first sync instead of pulling nothing (the
        // historical bug where get_changes returned empty on run #1 and remote
        // files only appeared on sync #2).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        // Three remote files already exist in scope before this client starts.
        let remote_prefix = "prefix/";
        let mut changes = Vec::new();
        let mut metadata = std::collections::HashMap::new();
        let mut contents = std::collections::HashMap::new();
        for (name, size, content) in [
            ("a.txt", 5usize, b"aaaaa".to_vec()),
            ("sub/b.txt", 5, b"bbbbb".to_vec()),
            ("c.md", 5, b"ccccc".to_vec()),
        ] {
            let remote = format!("{remote_prefix}{name}");
            changes.push(Change::remote_created(
                remote.clone(),
                format!("rhash-{name}"),
                size as u64,
                Utc::now(),
                format!("1-{name}"),
            ));
            let mut doc = FileDoc::new(remote.clone(), String::new(), size as u64);
            doc.rev = Some(format!("1-{name}"));
            doc.path = remote.clone();
            metadata.insert(remote.clone(), doc);
            contents.insert(remote.clone(), content);
        }

        let canned = CannedCouch {
            changes,
            last_seq: "3".to_string(),
            metadata,
            contents,
            ..CannedCouch::default()
        };
        // Empty in-memory state DB: no checkpoint, no file states (first sync).
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch(remote_prefix, canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync().await.expect("first sync should succeed");

        // Every pre-existing remote file is downloaded and materialized locally.
        assert_eq!(report.downloaded.0, 3, "all pre-existing files downloaded");
        assert_eq!(report.uploaded.0, 0);
        assert_eq!(report.conflicts, 0);
        assert_eq!(report.errors, [] as [String; 0]);
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("a.txt")).unwrap(),
            "aaaaa",
            "pre-existing a.txt must be materialized on first sync"
        );
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("sub/b.txt")).unwrap(),
            "bbbbb",
            "pre-existing nested b.txt must be materialized on first sync"
        );
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("c.md")).unwrap(),
            "ccccc",
            "pre-existing c.md must be materialized on first sync"
        );

        // The bootstrap run checkpoints the DB sequence so the next sync is
        // incremental rather than re-fetching the whole remote set.
        let cp = engine
            .get_checkpoint()
            .await
            .expect("checkpoint read should succeed")
            .expect("first sync must save a checkpoint");
        assert_eq!(
            cp.last_seq, "3",
            "first sync must checkpoint the remote seq"
        );
    }

    /// Seed client B's divergent local edit (LOCAL-B) against the earlier
    /// remote-A sync, so the scanner sees it as a local modification.
    fn seed_diverged_client(root: &SyncDirPath, state_db: &std::path::Path) {
        std::fs::write(root.as_path().join("doc.txt"), b"LOCAL-B").unwrap();
        let seed_db = LocalDb::open(state_db).unwrap();
        seed_file_state(&seed_db, "doc.txt", &compute_bytes_hash(b"REMOTE-A"), 7);
    }

    /// Build the `CannedCouch` for the divergence: client A pushed remote-A (rev
    /// 2-bbb) over the version the local client last synced.
    fn diverged_canned_couch(remote_path: &str, remote_a: &[u8]) -> CannedCouch {
        let mut meta = FileDoc::new(
            remote_path.to_string(),
            String::new(),
            remote_a.len() as u64,
        );
        meta.rev = Some("2-bbb".to_string());
        meta.path = remote_path.to_string();
        CannedCouch {
            changes: vec![Change::remote_modified(
                remote_path.to_string(),
                compute_bytes_hash(remote_a),
                remote_a.len() as u64,
                Utc::now(),
                "2-bbb".to_string(),
            )],
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), meta)]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_a.to_vec(),
            )]),
            ..CannedCouch::default()
        }
    }

    #[tokio::test]
    async fn unresolved_conflict_is_preserved_across_cycles_and_restarts() {
        // Regression (#6337): a stored conflict used to be implicitly resolved
        // on the next sync cycle — run_cycle stored the conflict and advanced
        // the checkpoint, then triage re-scanned the still-pending local edit
        // and, with the remote feed now empty, uploaded it over the remote
        // version. An unresolved-conflict path must be held (neither uploaded
        // nor downloaded) until explicit resolution; unrelated files keep syncing.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // Keep the state DB outside the sync root so it is never scanned/uploaded.
        let db_dir = tempfile::tempdir().unwrap();
        let state_db = db_dir.path().join("state.db");
        seed_diverged_client(&root, &state_db);

        let remote_prefix = "prefix/";
        let remote_path = format!("{remote_prefix}doc.txt");
        let remote_a = b"REMOTE-A".to_vec();

        // ── Cycle 1: the two clients diverge → conflict detected & stored ──
        let mut engine1 = SyncEngine::new(
            test_canned_couch(
                remote_prefix,
                diverged_canned_couch(&remote_path, &remote_a),
            ),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        );
        let report1 = engine1.sync().await.expect("cycle 1 sync");
        assert_eq!(report1.conflicts, 1, "cycle 1 must detect the conflict");
        assert_eq!(report1.uploaded.0, 0);
        assert_eq!(report1.downloaded.0, 0);
        assert_eq!(
            engine1.couchdb.test_write_calls(),
            0,
            "no remote write in cycle 1"
        );
        let conflicts1 = engine1.get_conflicts().await.unwrap();
        assert_eq!(conflicts1.len(), 1, "conflict must be persisted");
        assert_eq!(conflicts1[0].path, "doc.txt");
        // The persisted record preserves remote-A (the pending remote context)
        // so the path stays protected after the checkpoint advances past it.
        assert_eq!(
            conflicts1[0].remote_state.hash,
            compute_bytes_hash(b"REMOTE-A")
        );

        // ── Cycles 2 & 3 with a restart: remote feed now empty ─────────────
        // The pending local edit must be held, not uploaded; a brand-new
        // unrelated local file must still sync normally.
        std::fs::write(root.as_path().join("unrelated.txt"), b"U").unwrap();
        let empty = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            ..CannedCouch::default()
        };
        for restart in 0..2 {
            let mut engine = SyncEngine::new(
                test_canned_couch(remote_prefix, empty.clone()),
                // Fresh DB over the same state file simulates a process restart.
                LocalDb::open(&state_db).unwrap(),
                root.clone(),
            );
            let report = engine.sync().await.unwrap();
            // Only the unrelated file uploads, and only on the first restart.
            assert_eq!(
                report.uploaded.0,
                usize::from(restart == 0),
                "pending conflict edit must be held (restart {restart})"
            );
            assert_eq!(report.conflicts, 0, "no *new* conflict with an empty feed");
            assert_eq!(
                std::fs::read_to_string(root.as_path().join("doc.txt")).unwrap(),
                "LOCAL-B",
                "local version survives (restart {restart})"
            );
            assert_eq!(
                engine.get_conflicts().await.unwrap().len(),
                1,
                "conflict record persists across restarts"
            );
        }
    }

    #[tokio::test]
    async fn explicit_resolution_releases_an_unresolved_conflict_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let state_db = db_dir.path().join("state.db");
        seed_diverged_client(&root, &state_db);

        let remote_prefix = "prefix/";
        let remote_path = format!("{remote_prefix}doc.txt");
        let remote_a = b"REMOTE-A".to_vec();

        // Detect & store the conflict first.
        let mut engine = SyncEngine::new(
            test_canned_couch(
                remote_prefix,
                diverged_canned_couch(&remote_path, &remote_a),
            ),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        );
        assert_eq!(engine.sync().await.unwrap().conflicts, 1);

        // Only an explicit resolution removes the conflict record.
        engine
            .resolve_conflict("doc.txt", ResolutionStrategy::KeepLocal)
            .await
            .expect("resolve_conflict should succeed");
        assert!(
            engine.get_conflicts().await.unwrap().is_empty(),
            "explicit resolution removes the conflict record"
        );

        // The path is now released and synced: a plain sync makes no change
        // (KeepLocal already uploaded the edit during resolution).
        let report = engine.sync().await.unwrap();
        assert_eq!(
            report.uploaded.0, 0,
            "path no longer pending after resolution"
        );
        assert_eq!(report.downloaded.0, 0);
        assert_eq!(report.conflicts, 0);
    }

    // ── Bounded-concurrency batch apply (#3006) ──────────────────────────

    #[tokio::test]
    async fn upload_batch_applies_with_bounded_concurrency_and_collects_every_error() {
        // Larger than APPLY_CONCURRENCY_LIMIT so the loop is forced to run in
        // waves; a couple of uploads fail at the canned `save_file` step so we
        // can verify every per-file error is still collected (none dropped).
        const N: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let mut changes = Vec::with_capacity(N);
        let mut save_errors = std::collections::HashSet::new();
        for i in 0..N {
            let path = format!("up{i}.txt");
            std::fs::write(root.as_path().join(&path), format!("content-{i}")).unwrap();
            changes.push(Change::local_created(path.clone(), format!("hash-{i}"), 8));
            if i == 3 || i == 11 {
                save_errors.insert(format!("prefix/{path}"));
            }
        }

        let probe = std::sync::Arc::new(ConcurrencyProbe::default());
        let canned = CannedCouch {
            save_errors,
            probe: Some(probe.clone()),
            batch_delay: Some(std::time::Duration::from_millis(20)),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let mut report = SyncReport::default();
        apply_upload_batch(engine.apply_worker(), changes, false, &mut report)
            .await
            .expect("batch upload must not abort the sync");

        // Every file is accounted for and every failure is collected.
        assert_eq!(report.uploaded.0, N, "all uploads accounted for");
        assert_eq!(report.deleted_remote, 0);
        assert_eq!(
            report.errors.len(),
            2,
            "both simulated upload failures collected"
        );
        for i in [3, 11] {
            let expected = format!("Upload up{i}.txt: simulated save failure for prefix/up{i}.txt");
            let errors = &report.errors;
            assert!(
                errors.iter().any(|e| e == &expected),
                "upload batch dropped the error for up{i}.txt; got {errors:?}"
            );
        }

        // Real concurrency happened (more than one file in flight) but never
        // above the configured bound.
        let max = probe.max_concurrent();
        assert!(
            (2..=APPLY_CONCURRENCY_LIMIT).contains(&max),
            "upload batch max concurrency {max}, expected 2..={APPLY_CONCURRENCY_LIMIT}"
        );
    }

    #[tokio::test]
    async fn download_batch_applies_with_bounded_concurrency_and_collects_every_error() {
        const N: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let mut changes = Vec::with_capacity(N);
        let mut metadata = std::collections::HashMap::new();
        let mut contents = std::collections::HashMap::new();
        let mut content_errors = std::collections::HashSet::new();
        for i in 0..N {
            let remote = format!("prefix/dl{i}.txt");
            changes.push(Change::remote_created(
                remote.clone(),
                format!("rhash-{i}"),
                16,
                Utc::now(),
                format!("1-{i}"),
            ));
            let mut doc = FileDoc::new(remote.clone(), String::new(), 16);
            doc.rev = Some(format!("1-{i}"));
            doc.path = remote.clone();
            metadata.insert(remote.clone(), doc);
            contents.insert(remote.clone(), format!("remote-content-{i}").into_bytes());
            if i == 3 || i == 11 {
                content_errors.insert(remote);
            }
        }

        let probe = std::sync::Arc::new(ConcurrencyProbe::default());
        let canned = CannedCouch {
            metadata,
            contents,
            content_errors,
            probe: Some(probe.clone()),
            batch_delay: Some(std::time::Duration::from_millis(20)),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let mut report = SyncReport::default();
        apply_download_batch(engine.apply_worker(), changes, false, &mut report)
            .await
            .expect("batch download must not abort the sync");

        assert_eq!(report.downloaded.0, N, "all downloads accounted for");
        assert_eq!(report.deleted_local, 0);
        assert_eq!(
            report.errors.len(),
            2,
            "both simulated download failures collected"
        );
        for i in [3, 11] {
            let remote = format!("prefix/dl{i}.txt");
            let expected = format!(
                "Download {remote}: failed to fetch content for {remote}: \
                 simulated content fetch failure for {remote}"
            );
            let errors = &report.errors;
            assert!(
                errors.iter().any(|e| e == &expected),
                "download batch dropped the error for {remote}; got {errors:?}"
            );
        }

        let max = probe.max_concurrent();
        assert!(
            (2..=APPLY_CONCURRENCY_LIMIT).contains(&max),
            "download batch max concurrency {max}, expected 2..={APPLY_CONCURRENCY_LIMIT}"
        );
    }

    #[test]
    fn index_remote_changes_keeps_first_change_per_path() {
        let first = Change::remote_created(
            "prefix/a.txt".to_string(),
            "hash1".to_string(),
            1,
            Utc::now(),
            "1-a".to_string(),
        );
        let duplicate = Change::remote_created(
            "prefix/a.txt".to_string(),
            "hash2".to_string(),
            2,
            Utc::now(),
            "2-b".to_string(),
        );
        let other = Change::remote_created(
            "prefix/b.txt".to_string(),
            "hash3".to_string(),
            3,
            Utc::now(),
            "3-c".to_string(),
        );

        let changes = [first, duplicate, other];
        let index = SyncEngine::index_remote_changes(&changes);

        assert_eq!(index.len(), 2);
        assert_eq!(index["prefix/a.txt"].path(), "prefix/a.txt");
        // The first change for a duplicated remote path wins (matches the old
        // `.find()` which returned the first match).
        assert_eq!(index["prefix/a.txt"].size(), Some(1));
        assert_eq!(index["prefix/b.txt"].size(), Some(3));
    }

    #[tokio::test]
    async fn load_stored_states_queries_union_of_local_and_remote_paths() {
        let local = test_local_db();
        seed_file_state(&local, "a.txt", "hasha", 1);
        seed_file_state(&local, "b.txt", "hashb", 2);

        let engine = SyncEngine::new(
            test_couchdb(),
            local,
            test_root("/tmp/test-load-stored-states"),
        );

        // A local-only change path plus a remote-only change whose path maps to
        // a second local file under the configured prefix.
        let local_changes = vec![Change::local_created(
            "a.txt".to_string(),
            "hasha".to_string(),
            1,
        )];
        let remote_changes = vec![Change::remote_created(
            "test-prefix/b.txt".to_string(),
            "hashb".to_string(),
            2,
            Utc::now(),
            "1-r".to_string(),
        )];

        let states = engine
            .load_stored_states(&local_changes, &remote_changes)
            .await
            .expect("load stored states should succeed");

        assert_eq!(states.len(), 2);
        assert!(states.contains_key("a.txt"));
        assert!(states.contains_key("b.txt"));
    }

    #[tokio::test]
    async fn handle_remote_deletes_schedules_fresh_delete_and_skips_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // Tracked file that no longer exists locally. A remote delete newer
        // than the last sync should be scheduled; an older one must not.
        let local = test_local_db();
        seed_file_state(&local, "fresh.txt", "h1", 1);
        let stored_states = local
            .get_file_state("fresh.txt")
            .unwrap()
            .map(|s| (s.path.clone(), s))
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let engine = SyncEngine::new(test_couchdb(), local, root);

        let fresh_delete = Change::remote_deleted(
            "test-prefix/fresh.txt".to_string(),
            Some(Utc::now() + Duration::hours(1)),
            None,
        );
        let mut to_apply = Vec::new();
        let mut conflicts = Vec::new();
        engine
            .handle_remote_deletes(
                &[fresh_delete],
                &stored_states,
                &mut to_apply,
                &mut conflicts,
            )
            .unwrap();

        assert_eq!(to_apply.len(), 1, "newer remote delete is scheduled");
        assert_eq!(to_apply[0].path(), "test-prefix/fresh.txt");
        assert!(
            conflicts.is_empty(),
            "unchanged local file must not conflict"
        );

        let stale_delete = Change::remote_deleted(
            "test-prefix/fresh.txt".to_string(),
            Some(Utc::now() - Duration::days(2)),
            None,
        );
        let mut to_apply = Vec::new();
        let mut conflicts = Vec::new();
        engine
            .handle_remote_deletes(
                &[stale_delete],
                &stored_states,
                &mut to_apply,
                &mut conflicts,
            )
            .unwrap();

        assert!(to_apply.is_empty(), "stale remote delete is skipped");
        assert!(conflicts.is_empty(), "stale delete must not conflict");
    }

    #[tokio::test]
    async fn detect_conflicts_records_local_delete_remote_edit_conflict() {
        // Local-delete / remote-edit: the local delete must not blindly upload
        // (destroying the remote edit); it is recorded as a conflict.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local = test_local_db();
        seed_file_state(&local, "b.txt", "stalehash", 13);
        let mut stored = local.get_file_state("b.txt").unwrap().unwrap();
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_path = "prefix/b.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 14);
        remote_doc.rev = Some("2-def".to_string());
        remote_doc.mtime = TimestampMillis::now();
        remote_doc.path = remote_path.to_string();
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc.clone(),
            )]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                b"remote".to_vec(),
            )]),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let local_changes = vec![Change::local_deleted("b.txt".to_string())];
        let remote_changes = vec![Change::remote_modified(
            remote_path.to_string(),
            "remotehash".to_string(),
            14,
            Utc::now(),
            "2-def".to_string(),
        )];

        let (uploads, _downloads, conflicts) = engine
            .detect_conflicts(&local_changes, &remote_changes, true)
            .await
            .expect("detect_conflicts should succeed");

        assert!(
            uploads.is_empty(),
            "local delete racing a remote edit must not upload"
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "b.txt");
        assert!(!conflicts[0].remote_state.deleted);
    }

    #[tokio::test]
    async fn handle_remote_deletes_records_conflict_for_same_mtime_edit() {
        // A remote delete racing a same-mtime content edit (mtime preserved but
        // content changed) must record a conflict, not delete the local edit.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let file_path = root.as_path().join("f.txt");
        std::fs::write(&file_path, b"edited content").unwrap();
        let mtime: chrono::DateTime<Utc> = std::fs::metadata(&file_path)
            .unwrap()
            .modified()
            .unwrap()
            .into();

        let local = test_local_db();
        let mut state = FileState::new("f.txt".to_string(), "storedhash".to_string(), 1, mtime);
        state.couch_rev = Some(CouchRev::new("3-abc").unwrap());
        state.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&state).unwrap();
        let stored_states = local
            .get_file_state("f.txt")
            .unwrap()
            .map(|s| (s.path.clone(), s))
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let engine = SyncEngine::new(test_couchdb(), local, root);

        let fresh_delete = Change::remote_deleted(
            "test-prefix/f.txt".to_string(),
            Some(Utc::now() + Duration::hours(1)),
            Some("4-def".to_string()),
        );
        let mut to_apply = Vec::new();
        let mut conflicts = Vec::new();
        engine
            .handle_remote_deletes(
                &[fresh_delete],
                &stored_states,
                &mut to_apply,
                &mut conflicts,
            )
            .unwrap();

        assert!(to_apply.is_empty(), "same-mtime edit must not be deleted");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "f.txt");
        assert!(conflicts[0].remote_state.deleted);
    }

    #[tokio::test]
    async fn resolve_conflict_keep_remote_tombstone_deletes_local_not_resurrect() {
        // Choosing the remote deletion during resolution must delete the local
        // file, NOT resurrect the tombstone's stale content.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local_path = "conflict.txt";
        std::fs::write(root.as_path().join(local_path), b"local edit").unwrap();

        let local = test_local_db();
        let local_state = FileState::new(
            local_path.to_string(),
            "localhash".to_string(),
            10,
            Utc::now() - Duration::days(1),
        );
        let remote_state = RemoteState {
            hash: String::new(),
            size: 0,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("4-def").unwrap(),
            deleted: true, // tombstone
        };
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local_state,
                remote_state,
            ))
            .unwrap();

        // The remote side is a live tombstone: resolve_conflict must detect it
        // via fetch_metadata (the stored conflict does not persist `deleted`).
        let mut tombstone = FileDoc::new(format!("test-prefix/{local_path}"), String::new(), 0);
        tombstone.deleted = true;
        tombstone.rev = Some("4-def".to_string());
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                format!("test-prefix/{local_path}"),
                tombstone,
            )]),
            ..CannedCouch::default()
        };

        let mut engine = SyncEngine::new(
            test_canned_couch("test-prefix/", canned),
            local,
            root.clone(),
        );
        engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepRemote)
            .await
            .unwrap();

        assert!(
            !root.as_path().join(local_path).exists(),
            "keep-remote on a tombstone must delete the local file, not resurrect it"
        );
        assert!(
            engine.get_conflicts().await.unwrap().is_empty(),
            "conflict must be removed after resolution"
        );
    }

    #[tokio::test]
    async fn resolve_conflict_keep_local_on_deleted_local_deletes_remote() {
        // Choosing the local deletion during resolution (local file already
        // gone) must propagate the deletion to the remote instead of trying to
        // upload a missing file.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local_path = "conflict.txt";
        // Local file is absent (local side deleted).

        let local = test_local_db();
        let local_state = FileState::new(
            local_path.to_string(),
            "localhash".to_string(),
            10,
            Utc::now() - Duration::days(1),
        );
        let remote_state = RemoteState {
            hash: "remotehash".to_string(),
            size: 11,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-def").unwrap(),
            deleted: false, // remote edit
        };
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local_state,
                remote_state,
            ))
            .unwrap();

        let mut engine = SyncEngine::new(
            test_canned_couch("test-prefix/", CannedCouch::default()),
            local,
            root.clone(),
        );
        engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepLocal)
            .await
            .unwrap();

        assert!(
            engine.get_conflicts().await.unwrap().is_empty(),
            "conflict must be removed after resolution"
        );
    }

    #[tokio::test]
    async fn arbitrate_local_delete_records_conflict_when_remote_edited() {
        // A live local delete racing a remote edit must record a conflict
        // rather than propagate the delete (which would destroy the remote
        // edit).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local = test_local_db();
        seed_file_state(&local, "f.txt", "stalehash", 10);
        let mut stored = local.get_file_state("f.txt").unwrap().unwrap();
        stored.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        stored.last_sync_at = Utc::now() - Duration::days(1);
        local.save_file_state(&stored).unwrap();

        let remote_path = "test-prefix/f.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 11);
        remote_doc.rev = Some("2-def".to_string());
        remote_doc.mtime = TimestampMillis::now();
        remote_doc.path = remote_path.to_string();
        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([(
                remote_path.to_string(),
                remote_doc.clone(),
            )]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                b"remote edit".to_vec(),
            )]),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::new(test_canned_couch("test-prefix/", canned), local, root);

        let conflict = engine.arbitrate_local_delete("f.txt").await.unwrap();
        assert!(conflict, "remote edit must be surfaced as a conflict");

        let conflicts = engine.get_conflicts().await.unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "f.txt");
        assert!(!conflicts[0].remote_state.deleted);
    }

    // ── Incoming ignore / internal-path policy ─────────────────────────

    #[tokio::test]
    async fn dry_run_skips_ignored_and_internal_remote_changes_before_triage() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local = test_local_db();
        local.save_checkpoint("100-before").unwrap();

        let remote_prefix = "prefix/";
        let canned = CannedCouch {
            changes: vec![
                // Ignored by the configured pattern (*.log)
                Change::remote_created(
                    format!("{remote_prefix}debug.log"),
                    "hash".to_string(),
                    9,
                    Utc::now(),
                    "1-a".to_string(),
                ),
                // Internal state path — unconditionally protected
                Change::remote_created(
                    format!("{remote_prefix}.couchdb-file-sync/injected.txt"),
                    "hash".to_string(),
                    3,
                    Utc::now(),
                    "1-b".to_string(),
                ),
                // .sync-ignore — unconditionally protected
                Change::remote_created(
                    format!("{remote_prefix}.sync-ignore"),
                    "hash".to_string(),
                    3,
                    Utc::now(),
                    "1-c".to_string(),
                ),
                // Normal file — must still be triaged as a download
                Change::remote_created(
                    format!("{remote_prefix}ok.txt"),
                    "hash".to_string(),
                    6,
                    Utc::now(),
                    "1-d".to_string(),
                ),
            ],
            last_seq: "200-after".to_string(),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch(remote_prefix, canned),
            local,
            root.clone(),
            IgnoreMatcher::from_content("*.log"),
        );

        let report = engine.sync_dry_run().await.expect("dry run");

        assert_eq!(
            report.downloaded.0, 1,
            "only ok.txt should be queued for download"
        );
        assert_eq!(report.errors, [] as [String; 0]);
        assert_eq!(report.conflicts, 0);
        assert!(!root.as_path().join("debug.log").exists());
        assert!(
            !root
                .as_path()
                .join(".couchdb-file-sync/injected.txt")
                .exists()
        );
        assert!(!root.as_path().join(".sync-ignore").exists());
    }

    #[tokio::test]
    async fn sync_skips_ignored_remote_change_and_advances_checkpoint_past_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local = test_local_db();
        local.save_checkpoint("100-before").unwrap();

        let remote_prefix = "prefix/";
        let ok_path = format!("{remote_prefix}ok.txt");
        let canned = CannedCouch {
            changes: vec![
                Change::remote_created(
                    format!("{remote_prefix}debug.log"),
                    "hash".to_string(),
                    9,
                    Utc::now(),
                    "1-a".to_string(),
                ),
                Change::remote_created(
                    ok_path.clone(),
                    "hash".to_string(),
                    6,
                    Utc::now(),
                    "1-b".to_string(),
                ),
            ],
            last_seq: "200-after".to_string(),
            metadata: std::collections::HashMap::from([(
                ok_path.clone(),
                FileDoc::new(ok_path.clone(), String::new(), 6),
            )]),
            contents: std::collections::HashMap::from([(ok_path, b"ok.txt".to_vec())]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch(remote_prefix, canned),
            local,
            root.clone(),
            IgnoreMatcher::from_content("*.log"),
        );

        let report = engine.sync().await.expect("sync");

        assert_eq!(report.downloaded.0, 1, "only ok.txt downloads");
        assert!(root.as_path().join("ok.txt").exists());
        assert!(
            !root.as_path().join("debug.log").exists(),
            "ignored remote file must never be written"
        );
        // The ignored change is skipped (not an error), so the checkpoint
        // advances past it instead of retrying forever.
        assert_eq!(
            engine.get_checkpoint().await.unwrap().unwrap().last_seq,
            "200-after"
        );
    }

    #[tokio::test]
    async fn live_apply_refuses_ignored_and_internal_remote_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::from_content("*.log"),
        );

        // Ignored by pattern.
        engine
            .apply_remote_change(&Change::remote_created(
                "prefix/debug.log".to_string(),
                "hash".to_string(),
                9,
                Utc::now(),
                "1-a".to_string(),
            ))
            .await
            .expect("apply ignored change");
        assert!(
            !root.as_path().join("debug.log").exists(),
            "ignored remote change must not be applied live"
        );

        // Internal state path — must hold even though *.log doesn't match.
        engine
            .apply_remote_change(&Change::remote_created(
                "prefix/.couchdb-file-sync/injected.txt".to_string(),
                "hash".to_string(),
                3,
                Utc::now(),
                "1-b".to_string(),
            ))
            .await
            .expect("apply internal change");
        assert!(
            !root
                .as_path()
                .join(".couchdb-file-sync/injected.txt")
                .exists(),
            "internal remote change must not be applied live"
        );

        // .sync-ignore internal filename.
        engine
            .apply_remote_change(&Change::remote_created(
                "prefix/.sync-ignore".to_string(),
                "hash".to_string(),
                3,
                Utc::now(),
                "1-c".to_string(),
            ))
            .await
            .expect("apply .sync-ignore change");
        assert!(
            !root.as_path().join(".sync-ignore").exists(),
            ".sync-ignore remote change must not be applied live"
        );

        // A remote delete of an ignored path must not remove anything.
        std::fs::write(root.as_path().join("keep.log"), "data").unwrap();
        engine
            .apply_remote_change(&Change::remote_deleted(
                "prefix/keep.log".to_string(),
                Some(Utc::now()),
                None,
            ))
            .await
            .expect("apply ignored delete");
        assert!(
            root.as_path().join("keep.log").exists(),
            "ignored remote delete must not remove the local file"
        );
    }

    #[tokio::test]
    async fn internal_paths_stay_protected_even_with_negation_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // A negation attempts to re-include the internal state paths.
        let matcher =
            IgnoreMatcher::from_content("!.couchdb-file-sync/**\n!.couchfs/**\n!.sync-ignore");
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            test_local_db(),
            root.clone(),
            matcher,
        );

        engine
            .apply_remote_change(&Change::remote_created(
                "prefix/.couchdb-file-sync/injected.txt".to_string(),
                "hash".to_string(),
                3,
                Utc::now(),
                "1-a".to_string(),
            ))
            .await
            .expect("apply internal change under negation");
        engine
            .apply_remote_change(&Change::remote_created(
                "prefix/.couchfs/state.db".to_string(),
                "hash".to_string(),
                3,
                Utc::now(),
                "1-b".to_string(),
            ))
            .await
            .expect("apply .couchfs change under negation");
        engine
            .apply_remote_change(&Change::remote_created(
                "prefix/.sync-ignore".to_string(),
                "hash".to_string(),
                3,
                Utc::now(),
                "1-c".to_string(),
            ))
            .await
            .expect("apply .sync-ignore under negation");

        assert!(
            !root
                .as_path()
                .join(".couchdb-file-sync/injected.txt")
                .exists(),
            "internal path must stay protected under negation"
        );
        assert!(
            !root.as_path().join(".couchfs/state.db").exists(),
            ".couchfs must stay protected under negation"
        );
        assert!(
            !root.as_path().join(".sync-ignore").exists(),
            ".sync-ignore must stay protected under negation"
        );
    }
}
