use crate::couchdb::CouchDb;
use crate::local::{LocalDb, RemoteRebuildJournal, Scanner, compute_bytes_hash, compute_file_hash};
use crate::models::{
    Change, ChangeFingerprint, ChangeType, Checkpoint, Conflict, CouchRev, DownloadCount,
    ExpectedRemote, FileDoc, FileState, IgnoreMatcher, RemoteState, ResolutionStrategy,
    SyncDirPath, UploadCount,
};
use crate::sync::triage;
use anyhow::Result;
use chrono::{TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info, warn};

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
    /// Optional in-flight publication grace period for orphan chunk
    /// collection. When set, each non-dry-run sync cycle reclaims unreferenced
    /// chunks older than this grace window.
    orphan_chunk_grace: Option<std::time::Duration>,
    /// Per-cycle bound on the number of orphan chunks collected. Unreferenced
    /// chunks beyond this limit are deferred to a later cycle so collection
    /// stays bounded and resumable.
    orphan_chunk_batch: u64,
    /// Maximum size (bytes) of a file that sync will upload or download.
    /// `None` means unlimited. Files over the limit are rejected before full
    /// allocation/transfer, reported in the sync report, and never recorded as
    /// synced state (so a skipped file never becomes a deletion candidate).
    max_file_size: Option<u64>,
    /// Whether per-file upload/download applies may run in parallel. When
    /// `false`, applies run strictly serially (one file at a time), regardless
    /// of `apply_max_parallel`.
    apply_parallel: bool,
    /// Maximum number of per-file upload/download applies in flight at once
    /// when `apply_parallel` is enabled. Only consulted when parallel is on;
    /// it must be `>= 1` (validated before any apply work).
    apply_max_parallel: usize,
    /// Number of queued changes applied per scheduling wave. The apply loops
    /// process this many changes before moving on, bounding the queue of work
    /// held in memory. Must be `>= 1` (validated before any apply work).
    apply_batch_size: usize,
    /// Root-relative destination directory for keep-both preservation copies.
    /// When set, `write_keep_both_preservation` stores the remote version under
    /// `<root>/<conflict_dir>/<relative-dir>/<name>.remote` instead of a
    /// dot-prefixed copy next to the original file. The directory is excluded
    /// from sync so preserved copies can never reenter a cycle accidentally.
    conflict_dir: Option<PathBuf>,
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

/// A single remote file fetched, content-verified, and staged for an
/// authoritative local rebuild. `staging` is the path to the verified bytes in
/// the (scanner-ignored) staging area; `local_path` is where they will be
/// installed at commit time.
#[derive(Debug, Clone)]
struct StagedDownload {
    local_path: String,
    staging: PathBuf,
    hash: String,
    size: u64,
    couch_rev: Option<CouchRev>,
}

/// Persisted commit plan for an authoritative local rebuild. Written to the
/// state dir before the live tree is mutated so an interrupted commit can be
/// resumed on the next run; every commit step (install / delete / reset) is
/// idempotent, so re-running the plan is safe.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RebuildJournal {
    staged: Vec<JournalStaged>,
    deletes: Vec<String>,
}

/// Serde-friendly staged entry persisted in the [`RebuildJournal`] (paths and
/// hashes survive the restart; `CouchRev` is stored as its raw string).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct JournalStaged {
    local_path: String,
    staging: PathBuf,
    hash: String,
    size: u64,
    couch_rev: Option<String>,
}

impl From<&StagedDownload> for JournalStaged {
    fn from(s: &StagedDownload) -> Self {
        Self {
            local_path: s.local_path.clone(),
            staging: s.staging.clone(),
            hash: s.hash.clone(),
            size: s.size,
            couch_rev: s.couch_rev.as_ref().map(ToString::to_string),
        }
    }
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

    async fn get_remote_rebuild_journal(&self) -> Result<Option<RemoteRebuildJournal>> {
        self.run(super::super::local::db::LocalDb::get_remote_rebuild_journal)
            .await
    }

    async fn set_remote_rebuild_journal(&self, journal: &RemoteRebuildJournal) -> Result<()> {
        let journal = journal.clone();
        self.run(move |db| db.set_remote_rebuild_journal(&journal))
            .await
    }

    async fn clear_remote_rebuild_journal(&self) -> Result<()> {
        self.run(super::super::local::db::LocalDb::clear_remote_rebuild_journal)
            .await
    }

    /// Atomically clear the sync baseline and install `states` (see
    /// [`LocalDb::commit_rebuild_state`]).
    async fn commit_rebuild_state(&self, states: Vec<FileState>) -> Result<()> {
        let states = states.clone();
        self.run(move |db| db.commit_rebuild_state(&states)).await
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
    /// Maximum size (bytes) of a file this worker will upload or download.
    /// `None` means unlimited. Enforced at the transfer boundary so growth
    /// during a transfer cannot bypass the limit and a malicious oversized
    /// remote size is rejected before any allocation.
    max_file_size: Option<u64>,
}

impl ApplyWorker {
    /// Enforce the configured upload size limit, rejecting before any content
    /// is read/allocated and again after a read so growth during transfer can
    /// never bypass the bound.
    fn bound_upload_size(&self, local_path: &str, size: u64) -> Result<()> {
        if let Some(limit) = self.max_file_size
            && size > limit
        {
            anyhow::bail!(
                "refusing to upload {local_path}: {size} bytes exceeds configured max_file_size {limit}"
            );
        }
        Ok(())
    }

    /// Enforce the configured download size limit against remote metadata
    /// before any content is fetched/allocated, and again against the actually
    /// transferred length so growth during transfer can never bypass the bound.
    fn bound_download_size(&self, remote_path: &str, size: u64) -> Result<()> {
        if let Some(limit) = self.max_file_size
            && size > limit
        {
            anyhow::bail!(
                "refusing to download {remote_path}: {size} bytes exceeds configured max_file_size {limit}"
            );
        }
        Ok(())
    }

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

        // Reject an oversized local file before allocating/reading the whole
        // content buffer (reported by the caller).
        self.bound_upload_size(local_path, metadata.len())?;

        let content = tokio::fs::read(&file_path).await?;
        let content_len = content.len();
        debug!("[UPLOAD] Read {} bytes from disk", content_len);

        // Re-check after the read so a file that grew past the limit during
        // the transfer is aborted rather than uploaded.
        self.bound_upload_size(local_path, u64::try_from(content_len).unwrap_or(u64::MAX))?;

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
        // still authorized; the current doc's ctime is preserved on the success
        // path. The current children are NOT presumed to be owned exclusively by
        // this document: they may be shared with other documents (e.g. a copied
        // document), so they are deliberately left in place and never eagerly
        // deleted here (see the cleanup note below).
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

        let (existing_rev, existing_ctime) = if let Some(existing) = current {
            debug!("[UPLOAD] Existing doc found, rev: {:?}", existing.rev);
            debug!("[UPLOAD] Existing ctime: {} ms", existing.ctime.as_u64());
            (existing.rev, existing.ctime.as_u64())
        } else {
            debug!("[UPLOAD] No existing doc, creating new");
            (None, mtime)
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

        // Successful publication is recorded immediately, independent of any
        // best-effort cleanup. The previous child chunk set is intentionally
        // left in place rather than eagerly deleted: this document may not
        // exclusively own those chunks (another document may share them, e.g.
        // a copied document), so deleting them here would corrupt that other
        // file. Collection of truly-unreachable chunks is a separate, dedicated
        // garbage-collection outcome; keeping them here means a cleanup failure
        // can never erase the record of this successful publication.

        let state = FileState {
            path: local_path.to_string(),
            hash,
            size: metadata.len(),
            modified_at: metadata.modified()?.into(),
            couch_rev: doc.rev.as_deref().and_then(CouchRev::new),
            last_sync_at: Utc::now(),
            change_fingerprint: ChangeFingerprint::from_metadata(&metadata),
        };
        self.local_db.save_file_state(&state).await?;
        debug!("[UPLOAD] Updated local state with rev: {:?}", doc.rev);

        Ok((content_len, doc.rev))
    }

    #[allow(clippy::too_many_lines)] // staged-download pipeline spans many sequential steps
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

        // A soft-deleted tombstone is not downloadable content: it must not be
        // materialized locally nor counted as a completed download. The change
        // stream surfaces deletions as `Deleted` changes (routed elsewhere), but
        // guard here too so a deleted doc can never be installed as a file.
        if doc.deleted {
            if require_doc {
                anyhow::bail!("Document deleted in CouchDB: {remote_path}");
            }
            warn!("Document deleted in CouchDB: {}", remote_path);
            return Ok(None);
        }

        self.bound_download_size(remote_path, doc.size)?;

        let file_path = self.root_dir.join_contained(local_path)?;
        // Filesystem type transition: an incoming remote file arriving where a
        // directory currently exists (a remote directory replaced by a file, or
        // a leftover empty directory from a move). Remove an empty directory so
        // the file can be installed; preserve a nonempty directory because its
        // untracked local content must never be destroyed by a download.
        if file_path.is_dir() {
            let empty = tokio::fs::read_dir(&file_path)
                .await?
                .next_entry()
                .await?
                .is_none();
            if empty {
                debug!(
                    "[DOWNLOAD] Replacing empty local directory {:?} with remote file {}",
                    file_path, remote_path
                );
                tokio::fs::remove_dir(&file_path).await?;
            } else {
                warn!(
                    "Preserving nonempty local directory {:?} for incoming remote file {} (untracked content must not be removed)",
                    file_path, remote_path
                );
                return Ok(None);
            }
        }
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        debug!("[DOWNLOAD] Downloading {} chunks...", doc.children.len());
        let content = match self.couchdb.get_file_content_for_doc(&doc).await {
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

        // Bound the actually-transferred length too (growth during transfer).
        let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
        self.bound_download_size(remote_path, len)?;

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
            change_fingerprint: ChangeFingerprint::from_metadata(&metadata),
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
                if file_path.is_dir() {
                    // Directory-to-file transition on the delete side: a remote
                    // file was replaced such that only an empty leftover local
                    // directory remains. Remove it to converge; preserve a
                    // nonempty directory because its untracked local content
                    // must never be destroyed.
                    let empty = tokio::fs::read_dir(&file_path)
                        .await?
                        .next_entry()
                        .await?
                        .is_none();
                    if empty {
                        tokio::fs::remove_dir(&file_path).await?;
                    } else {
                        warn!(
                            "Preserving nonempty local directory {:?} for remote delete of {} (untracked content must not be removed)",
                            file_path, remote_path
                        );
                    }
                } else if file_path.is_file() {
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

    // Dry-run reports the *planned* outcome (what would happen) without
    // applying anything, so counters retain the full planned counts.
    if dry_run {
        return Ok(UploadOutcome {
            uploaded: usize::from(!is_deleted),
            deleted_remote: usize::from(is_deleted),
            error: None,
        });
    }

    // Apply the change, counting it only when it actually succeeds so a failed
    // upload is never reported as uploaded/deleted. The per-file error is
    // preserved in `outcome.error` so the batch collects every failure without
    // dropping per-path information.
    let mut outcome = UploadOutcome {
        uploaded: 0,
        deleted_remote: 0,
        error: None,
    };
    if is_deleted {
        match worker.apply_to_couchdb(&change).await {
            Ok(()) => {
                // Preserve the original post-delete local state refresh.
                worker.local_db.delete_file_state(change.path()).await?;
                outcome.deleted_remote = 1;
            }
            Err(e) => {
                error!("Failed to upload {}: {}", change.path(), e);
                outcome.error = Some(format!("Upload {}: {}", change.path(), e));
            }
        }
    } else if let Err(e) = worker.apply_to_couchdb(&change).await {
        error!("Failed to upload {}: {}", change.path(), e);
        outcome.error = Some(format!("Upload {}: {}", change.path(), e));
    } else {
        outcome.uploaded = 1;
    }

    Ok(outcome)
}

/// Apply a batch of uploads with bounded concurrency, collecting every
/// per-file error into `report` (none dropped).
///
/// Changes are processed in scheduling waves of `batch_size` (an observable
/// bound on the queue of work held at once). Within each wave, up to
/// `concurrency` per-file applies run in flight; `concurrency == 1` runs
/// strictly serially. Callers must pass a validated `concurrency >= 1` and
/// `batch_size >= 1` (see [`SyncEngine::validate_apply_settings`]).
async fn apply_upload_batch(
    worker: ApplyWorker,
    changes: Vec<Change>,
    dry_run: bool,
    concurrency: usize,
    batch_size: usize,
    report: &mut SyncReport,
) -> Result<()> {
    for wave in changes.chunks(batch_size) {
        let mut in_flight = tokio::task::JoinSet::new();
        let mut pending = wave.iter().cloned();

        // Seed the first `concurrency` tasks so the number of in-flight file
        // operations never exceeds the bound.
        for _ in 0..concurrency {
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

    // Dry-run reports the *planned* outcome (what would happen) without
    // applying anything, so counters retain the full planned counts.
    if dry_run {
        return Ok(DownloadOutcome {
            downloaded: usize::from(!is_deleted),
            deleted_local: usize::from(is_deleted),
            error: None,
        });
    }

    // Apply the change, counting it only when it actually succeeds so a failed
    // download is never reported as downloaded/deleted. The per-file error is
    // preserved in `outcome.error` so the batch collects every failure without
    // dropping per-path information.
    let mut outcome = DownloadOutcome {
        downloaded: 0,
        deleted_local: 0,
        error: None,
    };
    if let Err(e) = worker.apply_to_filesystem(&change).await {
        error!("Failed to download {}: {}", change.path(), e);
        outcome.error = Some(format!("Download {}: {}", change.path(), e));
    } else if is_deleted {
        outcome.deleted_local = 1;
    } else {
        outcome.downloaded = 1;
    }

    Ok(outcome)
}

/// Apply a batch of downloads with bounded concurrency, collecting every
/// per-file error into `report` (none dropped).
///
/// Changes are processed in scheduling waves of `batch_size` (an observable
/// bound on the queue of work held at once). Within each wave, up to
/// `concurrency` per-file applies run in flight; `concurrency == 1` runs
/// strictly serially. Callers must pass a validated `concurrency >= 1` and
/// `batch_size >= 1` (see [`SyncEngine::validate_apply_settings`]).
async fn apply_download_batch(
    worker: ApplyWorker,
    changes: Vec<Change>,
    dry_run: bool,
    concurrency: usize,
    batch_size: usize,
    report: &mut SyncReport,
) -> Result<()> {
    for wave in changes.chunks(batch_size) {
        let mut in_flight = tokio::task::JoinSet::new();
        let mut pending = wave.iter().cloned();

        for _ in 0..concurrency {
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
            orphan_chunk_grace: None,
            orphan_chunk_batch: 0,
            max_file_size: None,
            apply_parallel: true,
            apply_max_parallel: 4,
            apply_batch_size: 100,
            conflict_dir: None,
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
            orphan_chunk_grace: None,
            orphan_chunk_batch: 0,
            max_file_size: None,
            apply_parallel: true,
            apply_max_parallel: 4,
            apply_batch_size: 100,
            conflict_dir: None,
        }
    }

    /// Set the root-relative destination directory for keep-both preservation
    /// copies. When set, `write_keep_both_preservation` stores the remote
    /// version under `<root>/<conflict_dir>`; when `None`, it keeps the legacy
    /// dot-prefixed copy next to the original file. The directory is expected
    /// to be excluded from sync by the caller's ignore policy.
    #[must_use]
    pub fn with_conflict_dir(mut self, conflict_dir: Option<PathBuf>) -> Self {
        self.conflict_dir = conflict_dir;
        self
    }

    /// Replace the engine's ignore policy (both the scanner and the incoming
    /// remote-path guard). Used by the live sync loop to apply a policy that
    /// the watcher reloaded after an ignore-file change, so the next scan
    /// re-evaluates the tree under the new rules.
    pub fn reload_ignore(&mut self, ignore_matcher: IgnoreMatcher) {
        self.scanner = Scanner::new(self.root_dir.clone(), ignore_matcher.clone());
        self.ignore_matcher = ignore_matcher;
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

    /// Enable orphan chunk collection with an in-flight publication grace
    /// period. When `grace` is `Some`, each non-dry-run sync cycle reclaims
    /// unreferenced chunks older than `grace`, deferring younger ones so a
    /// concurrent in-flight publication is never collected. `batch` bounds the
    /// number of orphans collected per cycle (unreferenced chunks beyond it
    /// are deferred to a later cycle). Pass `None` (or omit) to disable
    /// collection.
    #[must_use]
    pub const fn with_orphan_chunk_collection(
        mut self,
        grace: Option<std::time::Duration>,
        batch: u64,
    ) -> Self {
        self.orphan_chunk_grace = grace;
        self.orphan_chunk_batch = batch;
        self
    }

    /// Bound the size (bytes) of files sync will upload or download. Pass
    /// `Some(limit)` to reject any file over `limit` before full allocation or
    /// transfer; pass `None` (or omit) for unlimited.
    #[must_use]
    pub const fn with_max_file_size(mut self, max_file_size: Option<u64>) -> Self {
        self.max_file_size = max_file_size;
        self
    }

    /// Configure the effective per-file apply parallelism and batching.
    ///
    /// * `parallel` — when `false`, uploads and downloads are applied strictly
    ///   serially (one file at a time) regardless of `max_parallel`.
    /// * `max_parallel` — maximum number of per-file applies in flight at once
    ///   when `parallel` is enabled. Must be `>= 1` (validated before apply).
    /// * `batch_size` — number of queued changes applied per scheduling wave.
    ///   Must be `>= 1` (validated before apply).
    ///
    /// Invalid bounds (`max_parallel == 0` with parallel enabled, or
    /// `batch_size == 0`) are rejected by [`SyncEngine::run_cycle`] before any
    /// apply work runs.
    #[must_use]
    pub const fn with_apply_settings(
        mut self,
        parallel: bool,
        max_parallel: usize,
        batch_size: usize,
    ) -> Self {
        self.apply_parallel = parallel;
        self.apply_max_parallel = max_parallel;
        self.apply_batch_size = batch_size;
        self
    }

    /// Effective number of per-file applies allowed in flight. `1` when
    /// parallelism is disabled, otherwise the configured `max_parallel` bound.
    /// Callers must ensure [`SyncEngine::validate_apply_settings`] succeeded so
    /// the returned value is `>= 1`.
    const fn effective_apply_concurrency(&self) -> usize {
        if self.apply_parallel {
            self.apply_max_parallel
        } else {
            1
        }
    }

    /// Validate the configured apply/batch bounds. Returns an error before any
    /// apply work runs when the configuration is unusable: `batch_size == 0`,
    /// or `max_parallel == 0` while parallelism is enabled. A `max_parallel`
    /// of `0` with parallelism disabled is harmless (applies are serial) and
    /// accepted.
    fn validate_apply_settings(&self) -> Result<()> {
        if self.apply_batch_size == 0 {
            anyhow::bail!("sync.batch_size must be >= 1 (got 0); refusing to apply changes");
        }
        if self.apply_parallel && self.apply_max_parallel == 0 {
            anyhow::bail!(
                "sync.max_parallel must be >= 1 when sync.parallel is enabled (got 0); refusing to apply changes"
            );
        }
        Ok(())
    }

    /// Best-effort orphan chunk collection. Runs after uploads/downloads (so
    /// this cycle's publications are committed) and after tombstone pruning.
    /// Reclaims unreferenced chunks older than the in-flight grace window; a
    /// failure is logged, never fatal to the sync.
    async fn collect_orphans(&self) {
        let Some(grace) = self.orphan_chunk_grace else {
            return;
        };
        match self
            .couchdb
            .collect_orphan_chunks(self.orphan_chunk_batch, grace)
            .await
        {
            Ok(result) if result.collected > 0 => {
                info!(
                    "Collected {} orphan chunk(s) ({} scanned, {} deferred)",
                    result.collected, result.scanned, result.deferred
                );
            }
            Ok(_) => {}
            Err(e) => warn!("Orphan chunk collection failed: {e}"),
        }
    }

    /// Cheap handle to the per-file apply state used by the bounded-concurrency
    /// sync loops (clones an `Arc`, a shared DB handle, and a `PathBuf`).
    fn apply_worker(&self) -> ApplyWorker {
        ApplyWorker {
            couchdb: self.couchdb.clone(),
            local_db: self.local_db.clone(),
            root_dir: self.root_dir.clone(),
            ignore_matcher: self.ignore_matcher.clone(),
            max_file_size: self.max_file_size,
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
    #[allow(clippy::too_many_lines)] // full sync pipeline spans many sequential steps
    async fn run_cycle(&self, dry_run: bool) -> Result<SyncReport> {
        // Fail fast on invalid apply/batch bounds before any work runs.
        self.validate_apply_settings()?;

        // Refuse a real sync while an authoritative remote rebuild is
        // incomplete. Publication may have failed partway, leaving the remote
        // partially rebuilt while the previous local baseline is still
        // authoritative; ordinary triage against that state would misclassify
        // surviving remote data as new edits or conflicts. The user must
        // resume (and the rebuild will reconcile) the declared plan via
        // `rebuild-remote`. A dry-run is read-only and safe to inspect.
        if !dry_run && let Some(journal) = self.local_db.get_remote_rebuild_journal().await? {
            anyhow::bail!(
                "A remote rebuild is incomplete ({}/{} uploads, {}/{} deletes \
                 published). Run `couchdb-file-sync rebuild-remote` to resume or \
                 reconcile the authoritative mirror before syncing; a normal sync \
                 would misclassify surviving remote data as ordinary new edits.",
                journal.uploads_done,
                journal.uploads.len(),
                journal.deletes_done,
                journal.deletes.len()
            );
        }

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

        // Enforce the configured size limit at the planning boundary: an
        // oversized local/remote change is excluded from the apply queues,
        // reported as an error, and (for local) has its stored state dropped so
        // it never becomes a deletion candidate or recorded as synced state.
        // The per-transfer checks in the apply worker remain as defense-in-depth
        // for growth-during-transfer and paths that bypass triage (live sync,
        // rebuilds).
        let (local_to_upload, remote_to_apply) = self
            .filter_oversized(local_to_upload, remote_to_apply, dry_run, &mut report)
            .await?;

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
        apply_upload_batch(
            self.apply_worker(),
            local_to_upload,
            dry_run,
            self.effective_apply_concurrency(),
            self.apply_batch_size,
            &mut report,
        )
        .await?;

        // 6. Apply clean remote changes to local (skipped in dry-run).
        //    Runs with bounded concurrency so a large batch of downloads does
        //    not serialize every per-file network round trip.
        info!(
            "========== DOWNLOADING {} FILES ==========",
            remote_to_apply.len()
        );
        apply_download_batch(
            self.apply_worker(),
            remote_to_apply,
            dry_run,
            self.effective_apply_concurrency(),
            self.apply_batch_size,
            &mut report,
        )
        .await?;

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

        // 9. Collect orphan chunks (skipped in dry-run). Pruning a tombstone
        //    hard-deletes only the metadata, orphaning the chunks it referenced,
        //    and a failed metadata publication leaves freshly-uploaded chunks
        //    unreferenced. Reclaim those unreferenced chunks without touching
        //    live, shared, recoverable, or in-flight content.
        if !dry_run {
            self.collect_orphans().await;
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

        // Record the declared authoritative plan BEFORE the first upload while
        // keeping the existing baseline intact. If an upload or delete fails
        // partway, the journal (and the pre-rebuild tracked state) survive, so
        // a later restart can resume or reconcile the rebuild and a normal sync
        // is refused rather than misclassifying surviving remote data as
        // ordinary new edits. State is only reset once publication completes.
        let mut journal = RemoteRebuildJournal {
            uploads: uploads.clone(),
            deletes: remote_deletes.clone(),
            uploads_done: 0,
            deletes_done: 0,
        };
        self.local_db.set_remote_rebuild_journal(&journal).await?;
        let mut report = SyncReport::default();

        for local_path in &uploads {
            let remote_path = self.couchdb.get_remote_path(local_path);
            // Remote rebuild is an explicit, unconditional mirror of local state
            // to remote: force-overwrite any remote doc regardless of its rev.
            self.upload_local_file(local_path, &remote_path, &ExpectedRemote::Force)
                .await?;
            journal.uploads_done += 1;
            report.uploaded.0 += 1;
        }
        // Persist upload progress so an interrupted run leaves an accurate
        // journal for resumption/reconciliation.
        self.local_db.set_remote_rebuild_journal(&journal).await?;

        for remote_path in &remote_deletes {
            self.couchdb.delete_file(remote_path).await?;
            journal.deletes_done += 1;
            report.deleted_remote += 1;
        }
        self.local_db.set_remote_rebuild_journal(&journal).await?;

        // Publication is complete: the remote mirror now matches local
        // authority. Only now is it safe to reset tracked state and clear the
        // journal so a normal sync resumes normally. The reset is delayed so a
        // failed rebuild never leaves local state cleared while the remote
        // scope is only partially published (a recoverable baseline must
        // survive a failed rebuild).
        self.local_db.reset_sync_state().await?;
        self.local_db.clear_remote_rebuild_journal().await?;

        info!(
            "========== REMOTE REBUILD COMPLETE: {} uploaded, {} remote deletes ==========",
            report.uploaded, report.deleted_remote
        );

        Ok(report)
    }

    /// Rebuild the local filesystem so it exactly matches the remote scope.
    ///
    /// The rebuild is staged so a failure is never destructive:
    ///
    /// 1. **Stage** — every remote file is fetched, content-verified, and
    ///    written to a sibling staging area outside the live tree, without
    ///    touching the live files or the sync baseline. A fetch/validation
    ///    failure here cleans up the staging files and aborts, leaving the
    ///    prior local tree and baseline exactly intact.
    /// 2. **Commit** — the install/delete plan is persisted to a journal, then
    ///    the verified staged content is installed, obsolete local files are
    ///    removed, and the sync baseline is reset + repopulated in a single
    ///    `SQLite` transaction. Obsolete files are removed only after their
    ///    replacement data is verified. An interruption mid-commit is resumable
    ///    from the journal on the next run.
    ///
    /// # Errors
    ///
    /// Returns an error if the local scan, remote fetch, any download/delete,
    /// or the staged commit fails.
    pub async fn rebuild_local_from_remote(&mut self) -> Result<SyncReport> {
        info!("========== LOCAL REBUILD STARTING ==========");

        // A prior commit may have been interrupted (process crash / cancellation)
        // after the journal was written. Resume it first: the journal records
        // already-verified staged content and the pending deletes, so completing
        // it restores a consistent tree + baseline without re-downloading.
        if let Some(report) = self.resume_local_rebuild_commit().await? {
            info!("Resumed an interrupted local rebuild commit");
            return Ok(report);
        }

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

        let mut report = SyncReport::default();

        // Stage & verify every remote download before mutating anything.
        let staged = self.stage_local_rebuild(&remote_downloads).await?;

        // Commit the verified, staged content with a resumable journal.
        self.commit_local_rebuild(staged, &local_deletes, &mut report)
            .await?;

        info!(
            "========== LOCAL REBUILD COMPLETE: {} deleted locally, {} downloaded ==========",
            report.deleted_local, report.downloaded
        );

        Ok(report)
    }

    /// Enforce the configured download size limit against remote metadata
    /// before any content is fetched/allocated, and again against the actually
    /// transferred length so growth during transfer can never bypass the bound
    /// (see [`ApplyWorker::bound_download_size`]).
    fn bound_download_size(&self, remote_path: &str, size: u64) -> Result<()> {
        if let Some(limit) = self.max_file_size
            && size > limit
        {
            anyhow::bail!(
                "refusing to download {remote_path}: {size} bytes exceeds configured max_file_size {limit}"
            );
        }
        Ok(())
    }

    /// Absolute path to the internal state directory (`.couchdb-file-sync`,
    /// falling back to the legacy `.couchfs`), which is unconditionally ignored
    /// by the scanner. It is a safe home for rebuild journals and staging
    /// because it is on the same filesystem as the sync root but never part of
    /// the synced content.
    fn state_dir(&self) -> PathBuf {
        let new_dir = self.root_dir.as_path().join(".couchdb-file-sync");
        if new_dir.exists() {
            return new_dir;
        }
        let old_dir = self.root_dir.as_path().join(".couchfs");
        if old_dir.exists() {
            return old_dir;
        }
        new_dir
    }

    /// Path to the directory that holds verified-but-uninstalled rebuild
    /// downloads. It lives under the (unconditionally ignored) state dir, so a
    /// half-finished rebuild never leaks a staged file into the synced tree.
    fn rebuild_stage_dir(&self) -> PathBuf {
        self.state_dir().join("rebuild-stage")
    }

    /// Path to the local-rebuild commit journal.
    fn rebuild_journal_path(&self) -> PathBuf {
        self.state_dir().join("rebuild-local.journal")
    }

    /// Stage and verify every remote download for a local rebuild, without
    /// mutating the live tree or the sync baseline.
    ///
    /// On any fetch/validation failure, all staging files already written are
    /// removed and the error is propagated, so the prior local tree and
    /// baseline survive untouched.
    async fn stage_local_rebuild(
        &self,
        remote_downloads: &[String],
    ) -> Result<Vec<StagedDownload>> {
        let mut staged = Vec::new();
        for remote_path in remote_downloads {
            let local_path = self.couchdb.get_local_path(remote_path);
            // Defensive guard: never download an ignored/internal remote path
            // during a local rebuild.
            if self.incoming_path_is_ignored(&local_path) {
                debug!(
                    "Skipping ignored remote path during rebuild download: {}",
                    remote_path
                );
                continue;
            }
            match self.stage_remote_download(remote_path, &local_path).await {
                Ok(Some(entry)) => staged.push(entry),
                Ok(None) => {}
                Err(e) => {
                    for entry in &staged {
                        let _ = tokio::fs::remove_file(&entry.staging).await;
                    }
                    return Err(e);
                }
            }
        }
        Ok(staged)
    }

    /// Fetch a single remote file, verify its content, and write it to the
    /// staging area. Returns `Ok(None)` when the file must be preserved as-is
    /// (a nonempty untracked local directory occupies its destination).
    async fn stage_remote_download(
        &self,
        remote_path: &str,
        local_path: &str,
    ) -> Result<Option<StagedDownload>> {
        let Some(doc) = self.couchdb.fetch_metadata(remote_path).await? else {
            anyhow::bail!("Document not found in CouchDB: {remote_path}");
        };
        // A soft-deleted tombstone is not downloadable content and must never be
        // materialized locally.
        if doc.deleted {
            anyhow::bail!("Document deleted in CouchDB: {remote_path}");
        }

        self.bound_download_size(remote_path, doc.size)?;

        let file_path = self.root_dir.join_contained(local_path)?;
        // Filesystem type transition: an incoming remote file arriving where a
        // directory currently exists. Preserve a nonempty directory because its
        // untracked local content must never be destroyed by a rebuild (mirrors
        // `download_remote_file`). An empty directory is safely replaced at
        // commit time.
        if file_path.is_dir() {
            let empty = tokio::fs::read_dir(&file_path)
                .await?
                .next_entry()
                .await?
                .is_none();
            if !empty {
                debug!(
                    "Preserving nonempty local directory {:?} for incoming remote file {} (untracked content must not be removed)",
                    file_path, remote_path
                );
                return Ok(None);
            }
        }

        let content = match self.couchdb.get_file_content_for_doc(&doc).await {
            Ok(data) => data,
            Err(e) => {
                anyhow::bail!("failed to fetch content for {remote_path}: {e}");
            }
        };
        let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
        self.bound_download_size(remote_path, len)?;

        let hash = compute_bytes_hash(&content);
        if !doc.hash.is_empty() && doc.hash != hash {
            anyhow::bail!(
                "content hash mismatch for {remote_path}: remote stored {}, downloaded {}; refusing to stage partial/corrupt file",
                doc.hash,
                hash
            );
        }

        // Write the verified bytes into the staging area (outside the live
        // tree), so a failed rebuild leaves no partial content in the synced
        // directories. `sync_all` makes the staged bytes durable before the
        // journal is written.
        let staging = self.rebuild_stage_dir().join(local_path);
        if let Some(parent) = staging.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        {
            let mut f = tokio::fs::File::create(&staging).await?;
            f.write_all(&content).await?;
            f.flush().await?;
            f.sync_all().await?;
        }

        Ok(Some(StagedDownload {
            local_path: local_path.to_string(),
            staging,
            hash,
            size: content.len() as u64,
            couch_rev: doc.rev.as_deref().and_then(CouchRev::new),
        }))
    }

    /// Commit a staged local rebuild: journal the plan, install the verified
    /// staged content, remove obsolete files, and reset + repopulate the sync
    /// baseline transactionally at the verified transition.
    async fn commit_local_rebuild(
        &self,
        staged: Vec<StagedDownload>,
        local_deletes: &[String],
        report: &mut SyncReport,
    ) -> Result<()> {
        let journal = RebuildJournal {
            staged: staged.iter().map(JournalStaged::from).collect(),
            deletes: local_deletes.to_vec(),
        };
        self.write_rebuild_journal(&journal).await?;

        // Install the verified staged content into the live tree. Idempotent so
        // a resumed journal re-run is safe.
        let mut installed_states = Vec::new();
        for entry in &journal.staged {
            self.install_staged(entry).await?;
            installed_states.push(self.staged_file_state(entry).await?);
            report.downloaded.0 += 1;
        }

        // Remove obsolete local files only after their replacement data has
        // been verified and staged.
        for local_path in &journal.deletes {
            // Never delete a path we just installed (defensive; the rebuild
            // plan keeps deletes and downloads disjoint).
            if journal.staged.iter().any(|s| &s.local_path == local_path) {
                continue;
            }
            // Defensive guard: never delete an ignored/internal local path
            // during a local rebuild.
            if self.incoming_path_is_ignored(local_path) {
                debug!(
                    "Skipping ignored local path during rebuild delete: {}",
                    local_path
                );
                continue;
            }
            let file_path = self.root_dir.join_contained(local_path)?;
            if file_path.exists() {
                tokio::fs::remove_file(&file_path).await?;
                report.deleted_local += 1;
            }
        }

        // Reset the sync baseline and install the new one atomically at the
        // verified transition.
        self.local_db.commit_rebuild_state(installed_states).await?;

        // The commit is complete: drop the journal and staging residue.
        self.remove_rebuild_artifacts().await?;
        Ok(())
    }

    /// Install one staged download into the live tree (creating parents and
    /// replacing an empty directory in the way). Idempotent: if the staging
    /// file was already moved, a target that exists means it is already
    /// installed.
    async fn install_staged(&self, entry: &JournalStaged) -> Result<()> {
        let file_path = self.root_dir.join_contained(&entry.local_path)?;
        // Empty directory in the way (remote directory replaced by a file).
        if file_path.is_dir() {
            let empty = tokio::fs::read_dir(&file_path)
                .await?
                .next_entry()
                .await?
                .is_none();
            if empty {
                debug!(
                    "[REBUILD] Replacing empty local directory {:?} with remote file {}",
                    file_path, entry.local_path
                );
                tokio::fs::remove_dir(&file_path).await?;
            }
        }
        if let Some(parent) = file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if tokio::fs::try_exists(&entry.staging).await? {
            tokio::fs::rename(&entry.staging, &file_path).await?;
        } else if !file_path.exists() {
            anyhow::bail!(
                "staged content for {} is missing (staging {} vanished); cannot complete rebuild install",
                entry.local_path,
                entry.staging.display()
            );
        }
        Ok(())
    }

    /// Persist the local-rebuild commit plan so an interruption can be resumed.
    async fn write_rebuild_journal(&self, journal: &RebuildJournal) -> Result<()> {
        let path = self.rebuild_journal_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(journal)?;
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    /// Resume an interrupted local-rebuild commit if a journal is present.
    /// Returns `Ok(None)` when there is nothing to resume.
    async fn resume_local_rebuild_commit(&self) -> Result<Option<SyncReport>> {
        let path = self.rebuild_journal_path();
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        let bytes = tokio::fs::read(&path).await?;
        let journal: RebuildJournal = serde_json::from_slice(&bytes)?;

        let mut report = SyncReport::default();
        // Re-run the idempotent commit steps: install any not-yet-installed
        // staged content, remove obsolete files, and reset + repopulate the
        // baseline transactionally.
        for entry in &journal.staged {
            self.install_staged(entry).await?;
            report.downloaded.0 += 1;
        }
        for local_path in &journal.deletes {
            if journal.staged.iter().any(|s| &s.local_path == local_path) {
                continue;
            }
            if self.incoming_path_is_ignored(local_path) {
                debug!(
                    "Skipping ignored local path during rebuild delete: {}",
                    local_path
                );
                continue;
            }
            let file_path = self.root_dir.join_contained(local_path)?;
            if file_path.exists() {
                tokio::fs::remove_file(&file_path).await?;
                report.deleted_local += 1;
            }
        }
        let mut installed_states = Vec::new();
        for entry in &journal.staged {
            installed_states.push(self.staged_file_state(entry).await?);
        }
        self.local_db.commit_rebuild_state(installed_states).await?;
        self.remove_rebuild_artifacts().await?;
        Ok(Some(report))
    }

    /// Build the [`FileState`] to record for a staged entry after it has been
    /// installed, using the verified content hash/rev from the journal and the
    /// on-disk metadata of the installed file.
    async fn staged_file_state(&self, entry: &JournalStaged) -> Result<FileState> {
        let file_path = self.root_dir.join_contained(&entry.local_path)?;
        let metadata = tokio::fs::metadata(&file_path).await?;
        Ok(FileState {
            path: entry.local_path.clone(),
            hash: entry.hash.clone(),
            size: metadata.len(),
            modified_at: metadata.modified()?.into(),
            couch_rev: entry.couch_rev.as_deref().and_then(CouchRev::new),
            last_sync_at: Utc::now(),
            change_fingerprint: ChangeFingerprint::from_metadata(&metadata),
        })
    }

    /// Remove the commit journal and any remaining staging residue after a
    /// completed (or successfully resumed) commit.
    async fn remove_rebuild_artifacts(&self) -> Result<()> {
        let journal = self.rebuild_journal_path();
        if tokio::fs::try_exists(&journal).await? {
            tokio::fs::remove_file(&journal).await?;
        }
        let stage_dir = self.rebuild_stage_dir();
        if tokio::fs::try_exists(&stage_dir).await? {
            tokio::fs::remove_dir_all(&stage_dir).await?;
        }
        Ok(())
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
                    // The fingerprint is freshly read from the on-disk file by
                    // the scanner, so it already reflects the current ctime
                    // (unlike couch_rev/last_sync_at, which the scanner does
                    // not know and must be preserved from the stored row).
                    let preserved_state = FileState {
                        path: state.path.clone(),
                        hash: state.hash.clone(),
                        size: state.size,
                        modified_at: state.modified_at,
                        couch_rev,
                        last_sync_at,
                        change_fingerprint: state.change_fingerprint,
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

    /// Exclude oversized changes from the apply queues, reporting each one as
    /// an error and dropping the local stored state of an oversized local file
    /// so it never becomes a deletion candidate or is recorded as synced
    /// state. A `None` limit (unlimited) is a no-op.
    async fn filter_oversized(
        &self,
        local_to_upload: Vec<Change>,
        remote_to_apply: Vec<Change>,
        dry_run: bool,
        report: &mut SyncReport,
    ) -> Result<(Vec<Change>, Vec<Change>)> {
        let Some(limit) = self.max_file_size else {
            return Ok((local_to_upload, remote_to_apply));
        };

        let mut uploads = Vec::with_capacity(local_to_upload.len());
        for change in local_to_upload {
            match change.size() {
                Some(size) if size > limit => {
                    report.errors.push(format!(
                        "Oversized local file skipped: {} ({} bytes exceeds max_file_size {limit})",
                        change.path(),
                        size
                    ));
                    if !dry_run {
                        // Drop any stored sync state so the skipped file never
                        // becomes a deletion candidate nor is treated as synced.
                        self.local_db.delete_file_state(change.path()).await?;
                    }
                }
                _ => uploads.push(change),
            }
        }

        let mut downloads = Vec::with_capacity(remote_to_apply.len());
        for change in remote_to_apply {
            match change.size() {
                Some(size) if size > limit => {
                    report.errors.push(format!(
                        "Oversized remote file skipped: {} ({} bytes exceeds max_file_size {limit})",
                        change.path(),
                        size
                    ));
                }
                _ => downloads.push(change),
            }
        }

        Ok((uploads, downloads))
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
                    change_fingerprint: local_state.change_fingerprint,
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
                        change_fingerprint: file_path
                            .metadata()
                            .ok()
                            .as_ref()
                            .and_then(ChangeFingerprint::from_metadata),
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

    /// Whether a local watcher change targets a directory rather than a single
    /// tracked file.
    ///
    /// A rename/move-in/move-out of a populated directory surfaces from the
    /// watcher as events for the top-level directory path only — never for its
    /// children. Such an event cannot be applied as a single file (an upload
    /// would try to read the directory and fail), so the live loop routes it
    /// into the bounded full reconciliation rescan, which expands it into a
    /// tracked subtree reconciliation (removing old remote child paths and
    /// uploading new ones) and safely orders filesystem type transitions.
    ///
    /// # Errors
    ///
    /// Returns an error if the local path cannot be resolved against the root.
    pub(crate) async fn is_directory_scoped(&self, change: &Change) -> Result<bool> {
        match change.change_type() {
            ChangeType::Created | ChangeType::Modified => {
                Ok(self.root_dir.join_contained(change.path())?.is_dir())
            }
            ChangeType::Deleted => {
                // A tracked file delete is a normal single-file event. A path
                // that is neither tracked nor present as a file is a
                // directory-scoped event (a directory move-out or delete): the
                // directory itself is never tracked as a file, and the top-level
                // path no longer exists on disk after the move.
                let tracked = self.get_file_state(change.path()).await?.is_some();
                let on_disk = self.root_dir.join_contained(change.path())?.is_file();
                Ok(!tracked && !on_disk)
            }
        }
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

    /// Whether the live remote has advanced to a non-deleted version the user
    /// did NOT preview (per the conflict snapshot). Used to guard destructive
    /// keep-local resolution: we never overwrite a remote version that changed
    /// since the conflict was detected. A missing remote (`None`) or a
    /// tombstone carries no content, so there is nothing unseen to clobber —
    /// those are always safe to proceed with.
    fn remote_version_changed(live: Option<&FileDoc>, snapshot: &RemoteState) -> bool {
        let Some(doc) = live else {
            return false;
        };
        if doc.deleted {
            // Tombstone: no content on the remote to clobber.
            return false;
        }
        // Live remote has content. Changed if the snapshot was a tombstone
        // (unseen content is now present) or the revision/content moved on.
        if snapshot.deleted {
            return true;
        }
        let snap_rev = snapshot.couch_rev.as_str();
        if snap_rev != CouchRev::DEFAULT_REV && !snap_rev.is_empty() {
            return doc.rev.as_deref() != Some(snap_rev);
        }
        if !snapshot.hash.is_empty() {
            return !doc.hash.is_empty() && doc.hash != snapshot.hash;
        }
        false
    }

    /// Whether the local file's current content differs from the conflict
    /// snapshot (i.e. was edited after the conflict was previewed). A file that
    /// existed at preview but is now gone is also a change. Used to guard
    /// destructive keep-remote resolution.
    fn local_version_changed(current_hash: Option<&str>, snapshot: &FileState) -> bool {
        // File present: changed iff its content differs from the snapshot; file
        // absent: changed iff the snapshot represented present content.
        current_hash.map_or(!snapshot.hash.is_empty(), |h| h != snapshot.hash.as_str())
    }

    /// Current on-disk content hash of `local_path`, or `None` if the file is
    /// absent. Detects local edits made after the conflict was previewed.
    async fn current_local_hash(&self, local_path: &str) -> Result<Option<String>> {
        let file_path = self.root_dir.join_contained(local_path)?;
        if !file_path.exists() {
            return Ok(None);
        }
        let state = self.get_local_state(local_path).await?;
        Ok(Some(state.hash))
    }

    /// Build the remote state currently live on the server (metadata + content
    /// hash), mirroring conflict-detection so a refreshed conflict shows the
    /// version the user must re-review.
    async fn live_remote_state(&self, remote_path: &str) -> Result<Option<RemoteState>> {
        let Some(doc) = self.couchdb.fetch_metadata(remote_path).await? else {
            return Ok(None);
        };
        let content = self.couchdb.get_file_content(remote_path).await?;
        let hash = compute_bytes_hash(&content);
        let modified_at = Utc
            .timestamp_millis_opt(i64::try_from(doc.mtime.as_u64()).unwrap_or(i64::MAX))
            .single()
            .unwrap_or_else(Utc::now);
        Ok(Some(RemoteState {
            hash,
            size: content.len() as u64,
            modified_at,
            couch_rev: CouchRev::new(doc.rev.as_deref().unwrap_or(CouchRev::DEFAULT_REV))
                .unwrap_or_default(),
            deleted: doc.deleted,
        }))
    }

    /// Write the remote version's content to a unique, deliberately local-only
    /// preservation copy. By default the destination is dot-prefixed so the
    /// default ignore matcher excludes it from scanning/upload; when a
    /// `conflict_dir` is configured the copy is written under
    /// `<root>/<conflict_dir>/<relative-dir>/<name>.remote` instead (that
    /// directory is excluded from sync by the caller's ignore policy). A free
    /// numeric suffix keeps it collision-safe so an existing backup always
    /// survives. No file state is recorded, so it is never queued for any
    /// remote path. Returns the relative local path written.
    async fn write_keep_both_preservation(
        &self,
        local_path: &str,
        remote_path: &str,
    ) -> Result<String> {
        let content = self.couchdb.get_file_content(remote_path).await?;

        let (dir, name) = match local_path.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", local_path),
        };
        // Resolve the preservation path. With a configured `conflict_dir` the
        // copy lives under `<conflict_dir>/<dir>/<name>.remote`, mirroring the
        // original's subdirectory so same-named files in different folders
        // never collide. Without one it keeps the legacy dot-prefixed copy
        // next to the original file.
        let base = format!("{name}.remote");
        let candidate_at = |suffix: u64| -> String {
            let suffixed = if suffix == 0 {
                base.clone()
            } else {
                format!("{base}.{suffix}")
            };
            match &self.conflict_dir {
                Some(conflict_dir) if dir.is_empty() => {
                    format!("{}/{}", conflict_dir.display(), suffixed)
                }
                Some(conflict_dir) => {
                    format!("{}/{dir}/{suffixed}", conflict_dir.display())
                }
                None if dir.is_empty() => format!(".{suffixed}"),
                None => format!("{dir}/.{suffixed}"),
            }
        };
        let mut candidate = candidate_at(0);
        let mut n = 1u64;
        while self.root_dir.join_contained(&candidate)?.exists() {
            candidate = candidate_at(n);
            n += 1;
        }

        let target = self.root_dir.join_contained(&candidate)?;
        let parent = target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("cannot determine parent for preservation copy"))?;
        tokio::fs::create_dir_all(parent).await?;
        let staging = parent.join(format!(
            ".{}.{}.{}.preserve",
            target
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("copy"),
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
        if let Err(e) = tokio::fs::rename(&staging, &target).await {
            let _ = tokio::fs::remove_file(&staging).await;
            return Err(e.into());
        }
        Ok(candidate)
    }

    /// Returns an error if no conflict exists for the path, if a destructive
    /// resolution detects that the side being overwritten changed since the
    /// conflict was previewed, or if the chosen strategy fails to apply.
    ///
    /// # Errors
    ///
    /// - `No conflict found for path: {local_path}` if there is no stored
    ///   conflict for the path.
    /// - A refresh error when **keep-local** would overwrite a remote version
    ///   that changed since preview, or **keep-remote** would overwrite a local
    ///   edit made after preview (the conflict is refreshed for re-review).
    /// - Any upload/download/filesystem error from applying the strategy.
    pub async fn resolve_conflict(
        &mut self,
        local_path: &str,
        strategy: ResolutionStrategy,
    ) -> Result<()> {
        let Some(conflict) = self.local_db.get_conflict(local_path).await? else {
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
        let remote_doc = self.couchdb.fetch_metadata(&remote_path).await?;
        let remote_is_tombstone = remote_doc.as_ref().is_some_and(|doc| doc.deleted);

        match strategy {
            ResolutionStrategy::KeepLocal => {
                // Keep-local overwrites/removes the remote side, so it must not
                // clobber a remote version that changed since the conflict was
                // previewed. If it did, refresh the conflict's remote snapshot
                // and bail so the user re-reviews before a destructive apply.
                if Self::remote_version_changed(remote_doc.as_ref(), &conflict.remote_state) {
                    if let Some(fresh) = self.live_remote_state(&remote_path).await? {
                        let mut refreshed = conflict.clone();
                        refreshed.remote_state = fresh;
                        self.local_db.store_conflict(&refreshed).await?;
                    }
                    anyhow::bail!(
                        "Remote changed since the conflict was previewed; conflict refreshed. \
                         Re-review before resolving (keep-local would overwrite a newer remote version)."
                    );
                }
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
                // Keep-remote overwrites/deletes the local side, so it must not
                // clobber a local edit made after the conflict was previewed.
                let current_local_hash = self.current_local_hash(local_path).await?;
                if Self::local_version_changed(current_local_hash.as_deref(), &conflict.local_state)
                {
                    if let Some(hash) = current_local_hash {
                        // Refresh the conflict's local snapshot so the next
                        // preview reflects the current local version.
                        let mut refreshed = conflict.clone();
                        refreshed.local_state.hash = hash;
                        self.local_db.store_conflict(&refreshed).await?;
                    }
                    anyhow::bail!(
                        "Local file changed since the conflict was previewed; conflict refreshed. \
                         Re-review before resolving (keep-remote would overwrite a newer local edit)."
                    );
                }
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
                    // Preserve the remote version as a unique, deliberately
                    // local-only copy (dot-prefixed → excluded from sync). Never
                    // clobber an existing backup and never mark it as synced to
                    // a remote path it does not own.
                    let preserved = self
                        .write_keep_both_preservation(local_path, &remote_path)
                        .await?;
                    info!("Saved remote version as: {}", preserved);

                    // Keep the local file and converge its sync state so the
                    // next cycle does not re-upload the kept local version over
                    // the preserved remote one.
                    let mut state = self.get_local_state(local_path).await?;
                    state.couch_rev = remote_doc
                        .as_ref()
                        .and_then(|d| d.rev.as_deref())
                        .and_then(CouchRev::new);
                    state.last_sync_at = Utc::now();
                    self.local_db.save_file_state(&state).await?;
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
    use crate::models::{
        Change, ChangeFingerprint, ChunkCodec, ChunkDoc, CouchRev, DocType, FileDoc, IgnoreMatcher,
        TimestampMillis,
    };
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
    fn engine_reload_ignore_replaces_policy_and_scanner() {
        // reload_ignore must update both the engine's incoming-path guard and
        // the scanner used for local scans, so a live reload re-evaluates the
        // tree under the new rules.
        let couch = test_couchdb();
        let local = test_local_db();
        let root = test_root("/tmp/test-engine-reload-ignore");
        let mut engine = SyncEngine::new(couch, local, root);

        assert!(
            !engine
                .ignore_matcher()
                .should_ignore(std::path::Path::new("debug.log"))
        );

        engine.reload_ignore(IgnoreMatcher::from_content("*.log"));

        assert!(
            engine
                .ignore_matcher()
                .should_ignore(std::path::Path::new("debug.log"))
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

    // ── Authoritative local rebuild (staged + journaled) ──────────────────

    /// Build a remote `FileDoc` for `prefix/` scope with the given content and
    /// the content's own hash (so content verification passes).
    fn remote_doc(remote_path: &str, content: &[u8]) -> FileDoc {
        let mut doc = FileDoc::new(
            remote_path.to_string(),
            compute_bytes_hash(content),
            content.len() as u64,
        );
        doc.rev = Some("1-abc".to_string());
        doc.mtime = TimestampMillis::now();
        doc.path = remote_path.to_string();
        doc
    }

    #[tokio::test]
    async fn rebuild_local_fetch_failure_preserves_tree_and_baseline() {
        // A fetch/validation failure during the stage phase must leave the
        // prior local tree and the sync baseline exactly intact.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // Obsolete local file that a full mirror would delete.
        std::fs::create_dir_all(root.as_path().join("sub")).unwrap();
        std::fs::write(root.as_path().join("sub/old.txt"), b"precious local").unwrap();

        let local = test_local_db();
        seed_file_state(&local, "sub/old.txt", "localhash", 14);
        local.save_checkpoint("99-before").expect("checkpoint");
        let conflict = Conflict::new(
            "sub/old.txt".to_string(),
            FileState::new("sub/old.txt".to_string(), "lh".to_string(), 14, Utc::now()),
            RemoteState {
                hash: "rh".to_string(),
                size: 1,
                modified_at: Utc::now(),
                couch_rev: CouchRev::default(),
                deleted: false,
            },
        );
        local.store_conflict(&conflict).expect("conflict");

        let remote = remote_doc("prefix/new.txt", b"replacement remote");
        let canned = CannedCouch {
            all_files: vec![remote.clone()],
            metadata: std::collections::HashMap::from([("prefix/new.txt".to_string(), remote)]),
            contents: std::collections::HashMap::from([(
                "prefix/new.txt".to_string(),
                b"replacement remote".to_vec(),
            )]),
            content_errors: std::collections::HashSet::from(["prefix/new.txt".to_string()]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine.rebuild_local_from_remote().await;
        assert!(result.is_err(), "fetch failure must fail the rebuild");

        // Prior local tree preserved, including the obsolete file.
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("sub/old.txt")).unwrap(),
            "precious local"
        );
        // Baseline preserved: file state, conflict and checkpoint all survive.
        assert_eq!(
            engine
                .get_file_state("sub/old.txt")
                .await
                .unwrap()
                .map(|s| s.path),
            Some("sub/old.txt".to_string())
        );
        assert_eq!(engine.get_conflicts().await.unwrap().len(), 1);
        assert!(engine.get_checkpoint().await.unwrap().is_some());
        // No staging residue leaked into the live tree.
        assert!(!root.as_path().join("new.txt").exists());
        // No journal left behind.
        assert!(
            !root
                .as_path()
                .join(".couchdb-file-sync")
                .join("rebuild-local.journal")
                .exists()
        );
    }

    #[tokio::test]
    async fn rebuild_local_success_deletes_obsolete_only_after_verified() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("old.txt"), b"obsolete local").unwrap();

        let local = test_local_db();
        seed_file_state(&local, "old.txt", "oldhash", 14);

        let remote = remote_doc("prefix/new.txt", b"replacement remote");
        let canned = CannedCouch {
            all_files: vec![remote.clone()],
            metadata: std::collections::HashMap::from([("prefix/new.txt".to_string(), remote)]),
            contents: std::collections::HashMap::from([(
                "prefix/new.txt".to_string(),
                b"replacement remote".to_vec(),
            )]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine
            .rebuild_local_from_remote()
            .await
            .expect("rebuild succeeds");
        assert_eq!(report.deleted_local, 1);
        assert_eq!(report.downloaded.0, 1);

        // Obsolete file removed, replacement installed with verified content.
        assert!(!root.as_path().join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("new.txt")).unwrap(),
            "replacement remote"
        );
        // Baseline replaced: new state present, old gone.
        let new_state = engine.get_file_state("new.txt").await.unwrap().unwrap();
        assert_eq!(new_state.hash, compute_bytes_hash(b"replacement remote"));
        assert!(engine.get_file_state("old.txt").await.unwrap().is_none());
        // Journal cleaned up after a successful commit.
        assert!(
            !root
                .as_path()
                .join(".couchdb-file-sync")
                .join("rebuild-local.journal")
                .exists()
        );
    }

    #[tokio::test]
    async fn rebuild_local_content_hash_mismatch_preserves_tree() {
        // A remote doc whose stored hash does not match the downloaded bytes
        // must abort staging and leave the prior tree intact.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("old.txt"), b"keep me").unwrap();
        let local = test_local_db();
        seed_file_state(&local, "old.txt", "localhash", 7);

        // Doc stores a wrong hash so verification fails.
        let mut remote = remote_doc("prefix/new.txt", b"the real content");
        remote.hash = "not-the-hash".to_string();
        let canned = CannedCouch {
            all_files: vec![remote.clone()],
            metadata: std::collections::HashMap::from([("prefix/new.txt".to_string(), remote)]),
            contents: std::collections::HashMap::from([(
                "prefix/new.txt".to_string(),
                b"the real content".to_vec(),
            )]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine.rebuild_local_from_remote().await;
        assert!(result.is_err(), "hash mismatch must fail the rebuild");
        // Nothing mutated: the obsolete file is still present, and the
        // replacement was never installed.
        assert_eq!(
            std::fs::read_to_string(root.as_path().join("old.txt")).unwrap(),
            "keep me"
        );
        assert!(!root.as_path().join("new.txt").exists());
        assert_eq!(
            engine
                .get_file_state("old.txt")
                .await
                .unwrap()
                .unwrap()
                .path,
            "old.txt"
        );
    }

    #[tokio::test]
    async fn rebuild_local_resumes_interrupted_commit_from_journal() {
        // Simulate a crash mid-commit: a journal + verified staged file already
        // exist, and the live tree still holds the obsolete file. The next
        // rebuild must resume the commit (no re-download) and finish it.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("old.txt"), b"obsolete local").unwrap();

        let local = test_local_db();
        seed_file_state(&local, "old.txt", "oldhash", 14);

        // Create the state dir and write a journal + staged content exactly as
        // a crashed commit would have left them.
        let state_path = root.as_path().join(".couchdb-file-sync");
        std::fs::create_dir_all(&state_path).unwrap();
        let rebuild_stage = state_path.join("rebuild-stage");
        std::fs::create_dir_all(&rebuild_stage).unwrap();
        std::fs::write(rebuild_stage.join("new.txt"), b"replacement remote").unwrap();
        let journal = serde_json::json!({
            "staged": [{
                "local_path": "new.txt",
                "staging": rebuild_stage.join("new.txt"),
                "hash": compute_bytes_hash(b"replacement remote"),
                "size": 17,
                "couch_rev": "1-abc"
            }],
            "deletes": ["old.txt"]
        });
        std::fs::write(
            state_path.join("rebuild-local.journal"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();

        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine
            .rebuild_local_from_remote()
            .await
            .expect("resumed rebuild succeeds");
        assert_eq!(report.downloaded.0, 1, "resume installs the staged content");
        assert_eq!(report.deleted_local, 1, "resume removes obsolete files");

        assert_eq!(
            std::fs::read_to_string(root.as_path().join("new.txt")).unwrap(),
            "replacement remote"
        );
        assert!(!root.as_path().join("old.txt").exists());
        // Baseline repopulated and the journal is gone.
        assert!(engine.get_file_state("new.txt").await.unwrap().is_some());
        assert!(!state_path.join("rebuild-local.journal").exists());
    }

    #[tokio::test]
    async fn rebuild_remote_failure_preserves_local_baseline() {
        // A failed remote rebuild (upload error) must not clear the local sync
        // baseline: it is only reset after publication fully succeeds.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("up.txt"), b"local content").unwrap();
        let local = test_local_db();
        seed_file_state(&local, "up.txt", "localhash", 13);
        local.save_checkpoint("50-before").expect("checkpoint");

        let canned = CannedCouch {
            all_files: Vec::new(),
            save_errors: std::collections::HashSet::from(["prefix/up.txt".to_string()]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine.rebuild_remote_from_local().await;
        assert!(
            result.is_err(),
            "upload failure must fail the remote rebuild"
        );
        // Baseline preserved despite the failure.
        assert!(engine.get_file_state("up.txt").await.unwrap().is_some());
        assert!(engine.get_checkpoint().await.unwrap().is_some());
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
    async fn sync_counts_only_successful_uploads_in_a_mixed_cycle() {
        // A one-shot sync with one upload that succeeds and one that fails must
        // report the successful count only, surface the per-path error, and
        // leave the failed file retryable (no saved state, no checkpoint advance).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // Two new local files: ok.txt uploads, fail.txt fails at save.
        std::fs::write(root.as_path().join("ok.txt"), "ok content").unwrap();
        std::fs::write(root.as_path().join("fail.txt"), "fail content").unwrap();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "500-after".to_string(),
            save_errors: std::collections::HashSet::from(["prefix/fail.txt".to_string()]),
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync().await.expect("mixed upload sync");

        assert_eq!(
            report.uploaded.0, 1,
            "only the successful upload counts, not the failed one"
        );
        assert_eq!(report.errors.len(), 1, "the failed upload is surfaced");
        assert!(
            report.errors.iter().any(|e| e.contains("fail.txt")),
            "error must name the failed path: {:?}",
            report.errors
        );
        assert_eq!(
            report.deleted_remote, 0,
            "no remote deletion planned in this cycle"
        );

        // The successful upload is recorded; the failed one is left unrecorded
        // so the next cycle retries it (no checkpoint advance past a failure).
        assert!(
            engine.get_file_state("ok.txt").await.unwrap().is_some(),
            "successful upload must be recorded"
        );
        assert!(
            engine.get_file_state("fail.txt").await.unwrap().is_none(),
            "failed upload must not be recorded (retryable next cycle)"
        );
        assert!(
            engine.get_checkpoint().await.unwrap().is_none(),
            "checkpoint must not advance when any apply failed"
        );
    }

    #[tokio::test]
    async fn sync_counts_only_successful_downloads_in_a_mixed_cycle() {
        // A one-shot sync with one download that succeeds and one that fails
        // must report the successful count only, surface the per-path error,
        // and leave the failed file absent (retryable next cycle).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_prefix = "prefix/";
        let ok_path = format!("{remote_prefix}ok.txt");
        let fail_path = format!("{remote_prefix}fail.txt");
        let mut metadata = std::collections::HashMap::new();
        let mut contents = std::collections::HashMap::new();
        for (i, p) in [&ok_path, &fail_path].into_iter().enumerate() {
            let mut doc = FileDoc::new(p.clone(), String::new(), 3);
            doc.rev = Some(format!("1-{i}"));
            doc.path = p.clone();
            metadata.insert(p.clone(), doc);
        }
        contents.insert(ok_path.clone(), b"ok!".to_vec());

        let canned = CannedCouch {
            changes: vec![
                Change::remote_created(
                    ok_path.clone(),
                    "okhash".to_string(),
                    3,
                    Utc::now(),
                    "1-0".to_string(),
                ),
                Change::remote_created(
                    fail_path.clone(),
                    "failhash".to_string(),
                    3,
                    Utc::now(),
                    "1-1".to_string(),
                ),
            ],
            last_seq: "500-after".to_string(),
            metadata,
            contents,
            content_errors: std::collections::HashSet::from([fail_path.clone()]),
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch(remote_prefix, canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let report = engine.sync().await.expect("mixed download sync");

        assert_eq!(
            report.downloaded.0, 1,
            "only the successful download counts, not the failed one"
        );
        assert_eq!(report.errors.len(), 1, "the failed download is surfaced");
        assert!(
            report.errors.iter().any(|e| e.contains("fail.txt")),
            "error must name the failed path: {:?}",
            report.errors
        );

        // The successful download is written; the failed one is left absent so
        // the next cycle retries it (no checkpoint advance past a failure).
        assert_eq!(
            std::fs::read(root.as_path().join("ok.txt")).unwrap(),
            b"ok!",
            "successful download must be written"
        );
        assert!(
            !root.as_path().join("fail.txt").exists(),
            "failed download must not be written"
        );
        assert!(
            engine.get_checkpoint().await.unwrap().is_none(),
            "checkpoint must not advance when any apply failed"
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
        // match the on-disk scan, exactly as a real sync records it: the change
        // fingerprint (Unix ctime) is captured from the on-disk metadata and
        // `last_sync_at` stays at the sync time (NEWER than the file's past
        // mtime). No artificial `last_sync_at` backdating is applied.
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
        stored.change_fingerprint = ChangeFingerprint::from_metadata(&meta);
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

        // Because the stored mtime/size/fingerprint all match and the file is
        // not racily clean, the full scan reuses the stored hash on Unix: zero
        // file re-reads. On platforms without a change fingerprint the
        // conservative fallback re-hashes (mtime older than last_sync_at).
        #[cfg(unix)]
        assert_eq!(
            engine.scanner.hash_computations(),
            0,
            "unchanged tree sync must not re-read/re-hash any file (Unix ctime fingerprint)"
        );
        #[cfg(not(unix))]
        assert_eq!(
            engine.scanner.hash_computations(),
            1,
            "no change evidence: unchanged tree re-hashed once"
        );

        // A second sync over the exact same tree still does not re-hash on
        // Unix (and re-hashes once more on platforms without change evidence).
        let report2 = engine.sync().await.expect("second sync unchanged tree");
        assert_eq!(report2.uploaded.0, 0);
        assert_eq!(report2.downloaded.0, 0);
        assert_eq!(report2.conflicts, 0);
        #[cfg(unix)]
        assert_eq!(
            engine.scanner.hash_computations(),
            0,
            "repeated sync of an unchanged tree must never re-hash (Unix)"
        );
        #[cfg(not(unix))]
        assert_eq!(
            engine.scanner.hash_computations(),
            2,
            "no change evidence: unchanged tree re-hashed once per sync"
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
        // overwrite proceeds. The old chunk is left in place (not deleted) so a
        // document sharing it stays readable; chunk collection is a follow-on GC.
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
        // chunk upload + save only; old chunks are NOT deleted (shared-safe).
        assert_eq!(engine.couchdb.test_write_calls(), 2);
    }

    #[tokio::test]
    async fn sync_runs_orphan_collection_when_enabled_and_skips_when_disabled() {
        // An empty cycle with orphan collection disabled must not invoke the
        // collector, and enabling it must invoke it once per non-dry-run cycle
        // without failing the sync (reachable content is never at risk because
        // the collector only touches unreferenced chunks).
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        // Disabled (default): the collector is not invoked.
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );
        let report = engine.sync().await.expect("sync with collection disabled");
        assert_eq!(report.errors, [] as [String; 0]);
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "orphan collection is disabled by default"
        );

        // Enabled: the collector runs each non-dry-run cycle without breaking
        // the sync.
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        )
        .with_orphan_chunk_collection(Some(std::time::Duration::from_mins(5)), 100);
        let report = engine.sync().await.expect("sync with collection enabled");
        assert_eq!(report.errors, [] as [String; 0]);
        assert_eq!(
            engine.couchdb.test_write_calls(),
            1,
            "orphan collection runs once when enabled"
        );
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
        // chunk upload + save only; old chunks are NOT deleted (shared-safe).
        assert_eq!(engine.couchdb.test_write_calls(), 2);
    }

    // ── Shared chunks: overwrite must not corrupt a copied doc (#6351) ──────

    /// Spawn an in-process fake `CouchDB` that stores file documents and chunks
    /// in memory, so a document overwrite can be exercised through the real HTTP
    /// path. `seed` is the initial `{id -> JSON doc}` store. When `fail_delete`
    /// is set, any DELETE returns 500, simulating a best-effort cleanup failure.
    /// Returns the base database URL, the server thread handle, and a shutdown
    /// flag.
    fn spawn_stateful_couch(
        seed: HashMap<String, serde_json::Value>,
        fail_delete: bool,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stateful couch");
        let addr = listener.local_addr().expect("stateful couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let store: std::sync::Arc<parking_lot::Mutex<HashMap<String, serde_json::Value>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(seed));
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("stateful couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set stateful couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("stateful couch tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(10),
                        listener.accept(),
                    )
                    .await
                    {
                        Ok(Ok((stream, _))) => {
                            let store = store.clone();
                            let fail_delete = fail_delete;
                            let service = hyper::service::service_fn(move |req| {
                                let store = store.clone();
                                async move { handle_stateful_couch_req(req, store, fail_delete).await }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {} // no connection yet; re-check shutdown
                    }
                }
            });
        });
        (base_db_url, handle, shutdown)
    }

    /// Serve `GET`/`PUT`/`DELETE` for document ids under `/test_db/` from the
    /// in-memory store. `PUT` upserts the JSON body; `GET` returns it (or 404);
    /// `DELETE` removes it (or fails with 500 when `fail_delete`).
    #[allow(clippy::unused_async)]
    async fn handle_stateful_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        store: std::sync::Arc<parking_lot::Mutex<HashMap<String, serde_json::Value>>>,
        fail_delete: bool,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let id = percent_encoding::percent_decode_str(
            parts.uri.path().split('/').next_back().unwrap_or(""),
        )
        .decode_utf8_lossy()
        .into_owned();
        match (parts.method, id.as_str()) {
            (hyper::Method::PUT, id) if !id.is_empty() => {
                use http_body_util::BodyExt;
                let bytes = body.collect().await.expect("collect body").to_bytes();
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("parse stored doc");
                store.lock().insert(id.to_string(), value);
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    format!(r#"{{"ok":true,"id":"{id}","rev":"1-abc"}}"#),
                ));
            }
            (hyper::Method::GET, id) if !id.is_empty() => {
                let value = store.lock().get(id).cloned();
                if let Some(value) = value {
                    *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                        serde_json::to_string(&value).expect("serialize stored doc"),
                    ));
                } else {
                    *response.status_mut() = hyper::StatusCode::NOT_FOUND;
                }
            }
            (hyper::Method::DELETE, _) if !id.is_empty() => {
                if fail_delete {
                    *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
                    *response.body_mut() = http_body_util::Full::new(
                        hyper::body::Bytes::from_static(br#"{"error":"simulated delete failure"}"#),
                    );
                } else {
                    store.lock().remove(&id);
                    *response.body_mut() = http_body_util::Full::new(
                        hyper::body::Bytes::from_static(br#"{"ok":true}"#),
                    );
                }
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn overwrite_of_doc_shared_with_another_preserves_other_docs_chunks() {
        // Regression (#6351): uploading/overwriting one of two documents that
        // share a child chunk must leave the other document readable. The old
        // code eagerly deleted the overwritten doc's previous children, which
        // corrupted a copied doc sharing those chunks. Old chunks are now left
        // in place (collection of unreachable chunks is a separate follow-on GC),
        // so the shared chunk survives. DELETE is injected to fail, so the test
        // also proves publication state is recorded regardless of a best-effort
        // cleanup failure.
        let shared_chunk_id = "shared1";
        let mut doc_a = FileDoc::new("docA".to_string(), String::new(), 5);
        doc_a.rev = Some("1-aaa".to_string());
        doc_a.path = "docA".to_string();
        doc_a.children = vec![shared_chunk_id.to_string()];
        let mut doc_b = FileDoc::new("docB".to_string(), String::new(), 5);
        doc_b.rev = Some("1-bbb".to_string());
        doc_b.path = "docB".to_string();
        doc_b.children = vec![shared_chunk_id.to_string()];

        let mut seed = HashMap::new();
        seed.insert("docA".to_string(), serde_json::to_value(&doc_a).unwrap());
        seed.insert("docB".to_string(), serde_json::to_value(&doc_b).unwrap());
        let shared_chunk = ChunkDoc {
            id: shared_chunk_id.to_string(),
            rev: Some("1-ccc".to_string()),
            data: "YWFhYWE=".to_string(), // base64("aaaaa")
            doc_type: DocType::Leaf,
            created_at: TimestampMillis::new(1000),
            codec: Some(ChunkCodec::Base64),
        };
        seed.insert(
            shared_chunk_id.to_string(),
            serde_json::to_value(&shared_chunk).unwrap(),
        );

        // Best-effort cleanup failures are injected: a DELETE on any chunk fails.
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, true);

        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("docA"), b"bbbbb").unwrap();
        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        // Overwrite docA with new content: uploads a fresh chunk and repoints
        // docA at it. docB's reference to the shared chunk must survive.
        let (bytes, rev) = engine
            .upload_local_file("docA", "docA", &ExpectedRemote::At("1-aaa".to_string()))
            .await
            .expect("overwrite of a matching rev must succeed");
        assert_eq!(bytes, b"bbbbb".len());
        assert!(rev.is_some(), "overwrite must return the new revision");

        // docB remains readable: its shared chunk is intact.
        let b_content = engine
            .couchdb
            .get_file_content("docB")
            .await
            .expect("docB must remain readable");
        assert_eq!(
            b_content, b"aaaaa",
            "docB must still resolve the shared chunk after docA is overwritten"
        );

        // docA's new content is readable too.
        let a_content = engine
            .couchdb
            .get_file_content("docA")
            .await
            .expect("docA must be readable after overwrite");
        assert_eq!(a_content, b"bbbbb");

        // Successful publication is recorded even though best-effort cleanup
        // (deleting the old shared chunk) is injected to fail.
        let state = engine
            .get_file_state("docA")
            .await
            .unwrap()
            .expect("docA's published state must be recorded");
        assert_eq!(state.hash, compute_bytes_hash(b"bbbbb"));

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch join");
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

    // -----------------------------------------------------------------------
    // Story 6355: downloads validate content against one consistent snapshot.
    // These run against the real chunk-reassembly path (via spawn_stateful_couch
    // + CouchDb::for_test_at) so the shape/length validation in
    // get_file_content_for_doc is exercised, not the canned-content shortcut.
    // -----------------------------------------------------------------------

    /// Build a `FileDoc` seeded for the stateful fake couch (single-segment id,
    /// hashless so the download is not short-circuited by the hash check).
    fn stateful_doc(id: &str, size: u64, children: Vec<&str>) -> FileDoc {
        let mut doc = FileDoc::new(id.to_string(), String::new(), size);
        doc.rev = Some("1-abc".to_string());
        doc.path = id.to_string();
        doc.children = children.into_iter().map(str::to_string).collect();
        doc
    }

    /// Build a `Leaf` chunk seeded for the stateful fake couch.
    fn stateful_chunk(id: &str, data: &[u8]) -> ChunkDoc {
        let mut chunk = ChunkDoc::new_encoded(id.to_string(), data.to_vec());
        chunk.rev = Some("1-abc".to_string());
        chunk
    }

    #[tokio::test]
    async fn download_rejects_declared_nonempty_file_with_no_chunks() {
        // The probe: a doc declaring size 123 with no chunks/hash must fail,
        // not be accepted as an empty file.
        let mut seed = HashMap::new();
        seed.insert(
            "ghost.txt".to_string(),
            serde_json::to_value(stateful_doc("ghost.txt", 123, vec![])).unwrap(),
        );
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, false);
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine
            .download_remote_file("ghost.txt", "ghost.txt", true, true)
            .await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch server join");

        let err = result
            .expect_err("a declared-nonempty file with no chunks must fail the download")
            .to_string();
        assert!(
            err.contains("no chunks"),
            "error must identify the absent content: {err}"
        );
        assert!(
            !root.as_path().join("ghost.txt").exists(),
            "failed download must not write an empty local file"
        );
        assert!(
            engine.get_file_state("ghost.txt").await.unwrap().is_none(),
            "failed download must not save FileState"
        );
    }

    #[tokio::test]
    async fn download_rejects_truncated_bytes() {
        // Hashless legacy doc declaring 10 bytes whose chunks only carry 6.
        let mut seed = HashMap::new();
        seed.insert(
            "trunc.txt".to_string(),
            serde_json::to_value(stateful_doc("trunc.txt", 10, vec!["h:c1", "h:c2"])).unwrap(),
        );
        seed.insert(
            "h:c1".to_string(),
            serde_json::to_value(stateful_chunk("h:c1", b"abc")).unwrap(),
        );
        seed.insert(
            "h:c2".to_string(),
            serde_json::to_value(stateful_chunk("h:c2", b"def")).unwrap(),
        );
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, false);
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine
            .download_remote_file("trunc.txt", "trunc.txt", true, true)
            .await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch server join");

        let err = result
            .expect_err("truncated bytes must fail the download via the size mismatch")
            .to_string();
        assert!(
            err.contains("size mismatch") && err.contains("10") && err.contains('6'),
            "error must name expected/downloaded sizes: {err}"
        );
        assert!(!root.as_path().join("trunc.txt").exists());
        assert!(engine.get_file_state("trunc.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn download_rejects_wrong_type_chunk_record() {
        let mut seed = HashMap::new();
        seed.insert(
            "wt.txt".to_string(),
            serde_json::to_value(stateful_doc("wt.txt", 3, vec!["h:c1"])).unwrap(),
        );
        let mut bad = stateful_chunk("h:c1", b"abc");
        bad.doc_type = DocType::Plain;
        seed.insert("h:c1".to_string(), serde_json::to_value(bad).unwrap());
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, false);
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine
            .download_remote_file("wt.txt", "wt.txt", true, true)
            .await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch server join");

        let err = result
            .expect_err("a wrong-type chunk record must fail the download")
            .to_string();
        assert!(
            err.contains("non-leaf"),
            "error must name the wrong chunk type: {err}"
        );
        assert!(!root.as_path().join("wt.txt").exists());
        assert!(engine.get_file_state("wt.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn download_rejects_duplicate_chunk_ids() {
        let mut seed = HashMap::new();
        seed.insert(
            "dup.txt".to_string(),
            serde_json::to_value(stateful_doc("dup.txt", 3, vec!["h:c1", "h:c1"])).unwrap(),
        );
        seed.insert(
            "h:c1".to_string(),
            serde_json::to_value(stateful_chunk("h:c1", b"abc")).unwrap(),
        );
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, false);
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine
            .download_remote_file("dup.txt", "dup.txt", true, true)
            .await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch server join");

        let err = result
            .expect_err("a duplicate chunk id must fail the download")
            .to_string();
        assert!(
            err.contains("Duplicate chunk"),
            "error must name the duplicate chunk: {err}"
        );
        assert!(!root.as_path().join("dup.txt").exists());
        assert!(engine.get_file_state("dup.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn download_accepts_zero_length_valid_file() {
        let mut seed = HashMap::new();
        seed.insert(
            "empty.txt".to_string(),
            serde_json::to_value(stateful_doc("empty.txt", 0, vec![])).unwrap(),
        );
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, false);
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes = engine
            .download_remote_file("empty.txt", "empty.txt", true, true)
            .await
            .expect("zero-length download should succeed")
            .expect("zero-length download should write a file");
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch server join");

        assert_eq!(bytes, 0, "zero-length download should write 0 bytes");
        assert_eq!(
            std::fs::read(root.as_path().join("empty.txt")).unwrap(),
            b"",
            "zero-length doc must produce an empty local file"
        );
        let stored = engine
            .get_file_state("empty.txt")
            .await
            .unwrap()
            .expect("zero-length download should save FileState");
        assert_eq!(stored.size, 0);
    }

    #[tokio::test]
    async fn download_does_not_count_deleted_doc() {
        // A soft-deleted tombstone must not be materialized nor counted as a
        // completed download.
        let mut doc = stateful_doc("gone.txt", 0, vec![]);
        doc.deleted = true;
        let mut seed = HashMap::new();
        seed.insert("gone.txt".to_string(), serde_json::to_value(doc).unwrap());
        let (base_db_url, server, shutdown) = spawn_stateful_couch(seed, false);
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let engine = SyncEngine::with_ignore(
            CouchDb::for_test_at(&base_db_url, ""),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine
            .download_remote_file("gone.txt", "gone.txt", false, true)
            .await
            .expect("a deleted doc must not error");
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("stateful couch server join");

        assert!(
            result.is_none(),
            "a deleted doc must not be counted as a completed download"
        );
        assert!(
            !root.as_path().join("gone.txt").exists(),
            "a deleted doc must not be materialized locally"
        );
        assert!(engine.get_file_state("gone.txt").await.unwrap().is_none());
    }

    // ── Configured file-size limits (#6358) ────────────────────────────────

    /// Upload of a local file over the configured limit must be rejected before
    /// any content is read or uploaded, must not save sync state, and the
    /// skipped file must never become a deletion candidate on a later sync.
    #[tokio::test]
    async fn sync_skips_oversized_local_upload_and_never_becomes_deletion_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // 6 bytes > limit 5.
        std::fs::write(root.as_path().join("big.txt"), b"123456").unwrap();

        let local = test_local_db();
        local.save_checkpoint("100-before").unwrap();
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        )
        .with_max_file_size(Some(5));

        let report = engine.sync().await.expect("sync with oversized local file");
        assert_eq!(report.uploaded.0, 0, "oversized file must not be uploaded");
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("Oversized local file skipped: big.txt")),
            "oversized file must be reported: {:#?}",
            report.errors
        );
        assert!(
            engine.get_file_state("big.txt").await.unwrap().is_none(),
            "oversized skipped file must never be recorded as synced state"
        );

        // Now the local file is deleted; because it was never synced, its
        // deletion must not propagate to remote (it never became a deletion
        // candidate).
        std::fs::remove_file(root.as_path().join("big.txt")).unwrap();
        let report2 = engine.sync().await.expect("second sync after local delete");
        assert_eq!(
            report2.deleted_remote, 0,
            "skipped oversized file must not become a deletion candidate"
        );
        assert_eq!(report2.uploaded.0, 0);
    }

    /// A sync with an oversized remote change must reject the download before
    /// any allocation/transfer, write nothing locally, and save no state.
    #[tokio::test]
    async fn sync_skips_oversized_remote_download() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_path = "prefix/big.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 200);
        remote_doc.rev = Some("1-big".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: vec![Change::remote_modified(
                remote_path.to_string(),
                "hash".to_string(),
                200,
                Utc::now(),
                "1-big".to_string(),
            )],
            last_seq: "200-after".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::new(),
            ..CannedCouch::default()
        };
        let local = test_local_db();
        local.save_checkpoint("100-before").unwrap();
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        )
        .with_max_file_size(Some(100));

        let report = engine
            .sync()
            .await
            .expect("sync with oversized remote file");
        assert_eq!(
            report.downloaded.0, 0,
            "oversized remote file must not be downloaded"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("Oversized remote file skipped: prefix/big.txt")),
            "oversized remote file must be reported: {:#?}",
            report.errors
        );
        assert!(
            !root.as_path().join("big.txt").exists(),
            "oversized remote file must not be written locally"
        );
        assert!(
            engine.get_file_state("big.txt").await.unwrap().is_none(),
            "oversized remote file must not save sync state"
        );
    }

    /// A local file that grows past the limit by the time it is transferred
    /// (length changed between scan and apply) must be rejected before content
    /// is uploaded, with zero `CouchDB` writes.
    #[tokio::test]
    async fn upload_rejects_oversized_file_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("grew.txt"), b"123456").unwrap();

        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        )
        .with_max_file_size(Some(5));

        let err = engine
            .upload_local_file("grew.txt", "prefix/grew.txt", &ExpectedRemote::Force)
            .await
            .expect_err("oversized upload must be rejected before allocation");
        assert!(
            format!("{err:#}").contains("max_file_size"),
            "error must cite the size limit: {err:#}"
        );
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "oversized upload must not read content nor write to CouchDB"
        );
        assert!(
            engine.get_file_state("grew.txt").await.unwrap().is_none(),
            "rejected upload must not save sync state"
        );
    }

    /// A maliciously oversized remote `doc.size` must be rejected from the
    /// metadata alone, before any content is fetched/allocated.
    #[tokio::test]
    async fn download_rejects_malicious_oversized_remote_metadata_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let remote_path = "prefix/huge.txt";
        let mut remote_doc = FileDoc::new(remote_path.to_string(), String::new(), 1_000_000_000);
        remote_doc.rev = Some("1-huge".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            // No content entry: if the size check is skipped, the empty-content
            // path would succeed — the rejection must come from metadata.
            contents: std::collections::HashMap::new(),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        )
        .with_max_file_size(Some(100));

        let err = engine
            .download_remote_file(remote_path, "huge.txt", true, true)
            .await
            .expect_err("oversized remote metadata must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("max_file_size") && msg.contains("1000000000"),
            "rejection must cite the remote size and limit: {msg}"
        );
        assert!(
            !root.as_path().join("huge.txt").exists(),
            "malicious oversized metadata must not allocate/write any content"
        );
    }

    /// A file exactly at the configured limit uploads successfully (boundary
    /// inclusive).
    #[tokio::test]
    async fn upload_at_limit_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("at.txt"), b"12345").unwrap();

        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        )
        .with_max_file_size(Some(5));

        let (bytes, _rev) = engine
            .upload_local_file("at.txt", "prefix/at.txt", &ExpectedRemote::Force)
            .await
            .expect("a file exactly at the limit must upload");
        assert_eq!(bytes, 5);
    }

    /// No configured limit (unlimited, i.e. config `0`) transfers files that
    /// would exceed a bounded default.
    #[tokio::test]
    async fn unlimited_engine_transfers_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // 20 bytes — would exceed any small bounded default but is fine when
        // the engine carries no limit (max_file_size = None / config 0).
        std::fs::write(root.as_path().join("unlimited.txt"), b"x".repeat(20)).unwrap();

        let local = test_local_db();
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", CannedCouch::default()),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let (bytes, _rev) = engine
            .upload_local_file(
                "unlimited.txt",
                "prefix/unlimited.txt",
                &ExpectedRemote::Force,
            )
            .await
            .expect("an unlimited engine must transfer the file");
        assert_eq!(bytes, 20);
    }

    /// A local create/modify change for a path that is a directory on disk is
    /// directory-scoped: it must be routed to subtree reconciliation, not
    /// applied as a single file.
    #[tokio::test]
    async fn is_directory_scoped_created_directory_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::create_dir_all(root.as_path().join("moved")).unwrap();

        let engine = SyncEngine::new(test_couchdb(), test_local_db(), root);
        assert!(
            engine
                .is_directory_scoped(&Change::local_created("moved".into(), String::new(), 0))
                .await
                .unwrap(),
            "a created directory must be directory-scoped"
        );
    }

    /// A local create/modify change for a path that is a real file is a normal
    /// single-file event and must not be routed to reconciliation.
    #[tokio::test]
    async fn is_directory_scoped_created_file_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("f.txt"), b"content").unwrap();

        let engine = SyncEngine::new(test_couchdb(), test_local_db(), root);
        assert!(
            !engine
                .is_directory_scoped(&Change::local_created("f.txt".into(), String::new(), 0))
                .await
                .unwrap(),
            "a created file must not be directory-scoped"
        );
    }

    /// A local delete of a tracked file is a normal single-file event.
    #[tokio::test]
    async fn is_directory_scoped_deleted_tracked_file_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local = test_local_db();
        let stored = FileState::new("gone.txt".to_string(), "h".to_string(), 3, Utc::now());
        local.save_file_state(&stored).unwrap();

        let engine = SyncEngine::new(test_couchdb(), local, root);
        assert!(
            !engine
                .is_directory_scoped(&Change::local_deleted("gone.txt".into()))
                .await
                .unwrap(),
            "a tracked file delete must not be directory-scoped"
        );
    }

    /// A local delete of a path that is neither tracked nor present as a file
    /// is a directory-scoped event (a directory move-out or delete).
    #[tokio::test]
    async fn is_directory_scoped_deleted_missing_path_is_true() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();

        let engine = SyncEngine::new(test_couchdb(), test_local_db(), root);
        assert!(
            engine
                .is_directory_scoped(&Change::local_deleted("moved".into()))
                .await
                .unwrap(),
            "a deleted untracked/missing path must be directory-scoped"
        );
    }

    /// Downloading a remote file onto an existing empty local directory must
    /// remove the leftover directory and install the file (directory-to-file
    /// transition converges).
    #[tokio::test]
    async fn download_replaces_empty_local_directory_with_remote_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::create_dir_all(root.as_path().join("dl.txt")).unwrap();

        let remote_path = "prefix/dl.txt";
        let content: Vec<u8> = b"file replaces dir".to_vec();
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
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let bytes = engine
            .download_remote_file(remote_path, "dl.txt", true, true)
            .await
            .expect("download should succeed")
            .expect("download should write content");

        assert_eq!(bytes, content.len());
        assert!(
            root.as_path().join("dl.txt").is_file(),
            "the empty directory must be replaced by the downloaded file"
        );
        assert_eq!(
            std::fs::read(root.as_path().join("dl.txt")).unwrap(),
            content,
            "downloaded file must hold the remote content"
        );
    }

    /// Downloading a remote file onto a nonempty local directory must preserve
    /// the directory and its untracked content, not destroy it.
    #[tokio::test]
    async fn download_preserves_nonempty_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::create_dir_all(root.as_path().join("dl.txt")).unwrap();
        std::fs::write(root.as_path().join("dl.txt/child.txt"), b"untracked").unwrap();

        let remote_path = "prefix/dl.txt";
        let content: Vec<u8> = b"file replaces dir".to_vec();
        let mut remote_doc =
            FileDoc::new(remote_path.to_string(), String::new(), content.len() as u64);
        remote_doc.rev = Some("1-abc".to_string());
        remote_doc.path = remote_path.to_string();

        let canned = CannedCouch {
            changes: Vec::new(),
            last_seq: "1".to_string(),
            metadata: std::collections::HashMap::from([(remote_path.to_string(), remote_doc)]),
            contents: std::collections::HashMap::from([(remote_path.to_string(), content)]),
            content_errors: std::collections::HashSet::new(),
            ..CannedCouch::default()
        };
        let engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = engine
            .download_remote_file(remote_path, "dl.txt", true, true)
            .await
            .expect("preserving a nonempty directory must not error");

        assert!(result.is_none(), "nothing should be written");
        assert!(
            root.as_path().join("dl.txt").is_dir(),
            "the nonempty directory must be preserved"
        );
        assert_eq!(
            std::fs::read(root.as_path().join("dl.txt/child.txt")).unwrap(),
            b"untracked",
            "untracked content inside the directory must survive"
        );
    }

    /// A remote delete converging onto an empty leftover local directory must
    /// remove it (directory-to-file transition on the delete side).
    #[tokio::test]
    async fn remote_delete_removes_empty_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::create_dir_all(root.as_path().join("gone")).unwrap();

        let mut engine = SyncEngine::new(test_couchdb(), test_local_db(), root.clone());
        engine
            .apply_remote_change(&Change::remote_deleted(
                "gone".into(),
                None,
                Some("2-abc".into()),
            ))
            .await
            .expect("delete of an empty directory should succeed");

        assert!(
            !root.as_path().join("gone").exists(),
            "the empty leftover directory must be removed"
        );
    }

    /// A remote delete converging onto a nonempty local directory must preserve
    /// the directory and its untracked content.
    #[tokio::test]
    async fn remote_delete_preserves_nonempty_local_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::create_dir_all(root.as_path().join("gone")).unwrap();
        std::fs::write(root.as_path().join("gone/child.txt"), b"untracked").unwrap();

        let mut engine = SyncEngine::new(test_couchdb(), test_local_db(), root.clone());
        engine
            .apply_remote_change(&Change::remote_deleted(
                "gone".into(),
                None,
                Some("2-abc".into()),
            ))
            .await
            .expect("delete over a nonempty directory should not error");

        assert!(
            root.as_path().join("gone").is_dir(),
            "the nonempty directory must be preserved"
        );
        assert_eq!(
            std::fs::read(root.as_path().join("gone/child.txt")).unwrap(),
            b"untracked",
            "untracked content must survive the delete"
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
        // Larger than the parallel bound so the loop is forced to run in
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
        apply_upload_batch(engine.apply_worker(), changes, false, 4, 100, &mut report)
            .await
            .expect("batch upload must not abort the sync");

        // Every failure is collected and only the successful uploads count.
        assert_eq!(report.uploaded.0, N - 2, "only successful uploads counted");
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
        // above the configured bound (max_parallel = 4).
        let max = probe.max_concurrent();
        assert!(
            (2..=4).contains(&max),
            "upload batch max concurrency {max}, expected 2..=4"
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
        apply_download_batch(engine.apply_worker(), changes, false, 4, 100, &mut report)
            .await
            .expect("batch download must not abort the sync");

        assert_eq!(
            report.downloaded.0,
            N - 2,
            "only successful downloads counted"
        );
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
            (2..=4).contains(&max),
            "download batch max concurrency {max}, expected 2..=4"
        );
    }

    #[tokio::test]
    async fn upload_batch_runs_serially_when_parallel_is_disabled() {
        const N: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let mut changes = Vec::with_capacity(N);
        let mut save_errors = std::collections::HashSet::new();
        for i in 0..N {
            let path = format!("ser{i}.txt");
            std::fs::write(root.as_path().join(&path), format!("content-{i}")).unwrap();
            changes.push(Change::local_created(path.clone(), format!("hash-{i}"), 8));
            if i == 2 || i == 5 {
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
        // Concurrency 1 (what the engine derives when parallel=false) means
        // applies never overlap: max in-flight is exactly one.
        apply_upload_batch(engine.apply_worker(), changes, false, 1, 100, &mut report)
            .await
            .expect("batch upload must not abort the sync");

        assert_eq!(report.uploaded.0, N - 2, "only successful uploads counted");
        assert_eq!(
            report.errors.len(),
            2,
            "both serial-mode failures collected"
        );
        assert_eq!(
            probe.max_concurrent(),
            1,
            "serial mode must never run more than one apply at once"
        );
    }

    #[tokio::test]
    async fn upload_batch_bounded_concurrency_across_multiple_batches_collects_every_error() {
        // batch_size < N forces multiple scheduling waves; a couple of uploads
        // fail inside each wave so we verify the concurrency bound holds across
        // waves and every per-file error is still collected (none dropped).
        const N: usize = 12;
        const BATCH: usize = 5;
        const CONCURRENCY: usize = 3;
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let mut changes = Vec::with_capacity(N);
        let mut save_errors = std::collections::HashSet::new();
        for i in 0..N {
            let path = format!("mb{i}.txt");
            std::fs::write(root.as_path().join(&path), format!("content-{i}")).unwrap();
            changes.push(Change::local_created(path.clone(), format!("hash-{i}"), 8));
            // Fail one file in each of the three waves.
            if i == 1 || i == 7 || i == 11 {
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
        apply_upload_batch(
            engine.apply_worker(),
            changes,
            false,
            CONCURRENCY,
            BATCH,
            &mut report,
        )
        .await
        .expect("batch upload must not abort the sync");

        assert_eq!(
            report.uploaded.0,
            N - 3,
            "only successful uploads counted across waves"
        );
        assert_eq!(
            report.errors.len(),
            3,
            "one failure per wave collected across multiple batches"
        );
        for i in [1, 7, 11] {
            let expected = format!("Upload mb{i}.txt: simulated save failure for prefix/mb{i}.txt");
            assert!(
                report.errors.iter().any(|e| e == &expected),
                "multi-batch upload dropped the error for mb{i}.txt; got {:?}",
                report.errors
            );
        }

        // Bounded in every wave: in-flight never exceeds CONCURRENCY.
        let max = probe.max_concurrent();
        assert!(
            (2..=CONCURRENCY).contains(&max),
            "multi-batch upload max concurrency {max}, expected 2..={CONCURRENCY}"
        );
    }

    #[tokio::test]
    async fn zero_batch_size_fails_before_any_apply_work() {
        let mut engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            test_root("/tmp/apply-zbatch"),
        )
        .with_apply_settings(true, 4, 0);
        let err = engine
            .sync()
            .await
            .expect_err("zero batch_size must fail sync");
        assert!(
            err.to_string().contains("batch_size"),
            "zero batch_size error should mention batch_size; got {err}"
        );
    }

    #[tokio::test]
    async fn parallel_with_zero_max_parallel_fails_before_any_apply_work() {
        let mut engine = SyncEngine::new(
            test_couchdb(),
            test_local_db(),
            test_root("/tmp/apply-zmax"),
        )
        .with_apply_settings(true, 0, 100);
        let err = engine
            .sync()
            .await
            .expect_err("parallel with zero max_parallel must fail sync");
        assert!(
            err.to_string().contains("max_parallel"),
            "zero max_parallel error should mention max_parallel; got {err}"
        );
    }

    #[tokio::test]
    async fn disabled_parallel_with_zero_max_parallel_is_accepted() {
        // max_parallel is meaningless when parallelism is off, so it is not a
        // validation failure: applies run serially regardless.
        let mut engine = SyncEngine::new(
            test_canned_couch("prefix/", CannedCouch::default()),
            test_local_db(),
            test_root("/tmp/apply-serial-zmax"),
        )
        .with_apply_settings(false, 0, 100);
        let report = engine
            .sync()
            .await
            .expect("serial sync with max_parallel 0 must not fail");
        assert_eq!(report.uploaded.0, 0);
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
            compute_bytes_hash(b"local edit"),
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

    /// Build a `CannedCouch` whose remote doc for `remote_path` carries
    /// `content` at the given revision and hash.
    fn resolve_canned_couch(remote_path: &str, content: &[u8], rev: &str) -> CannedCouch {
        let mut meta = FileDoc::new(remote_path.to_string(), String::new(), content.len() as u64);
        meta.rev = Some(rev.to_string());
        meta.path = remote_path.to_string();
        meta.hash = compute_bytes_hash(content);
        CannedCouch {
            metadata: std::collections::HashMap::from([(remote_path.to_string(), meta)]),
            contents: std::collections::HashMap::from([(
                remote_path.to_string(),
                content.to_vec(),
            )]),
            ..CannedCouch::default()
        }
    }

    #[tokio::test]
    async fn resolve_conflict_keep_both_preserves_unique_local_only_copy_and_converges() {
        // Keep-both must preserve the remote version under a unique,
        // deliberately local-only (dot-prefixed) name without clobbering an
        // existing `.remote` backup, leave the local file intact, and converge
        // its sync state so the next cycle does not re-upload the kept local
        // version over the preserved remote one.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let state_db = db_dir.path().join("state.db");

        // Nested path so we prove the dot-prefix lands on the filename, not the
        // parent directory.
        let local_path = "docs/conflict.txt";
        let local_full = root.as_path().join(local_path);
        std::fs::create_dir_all(local_full.parent().unwrap()).unwrap();
        std::fs::write(&local_full, b"LOCAL-B").unwrap();

        let local = LocalDb::open(&state_db).unwrap();
        // Stored state reflects the last-synced remote version (divergent from
        // the on-disk LOCAL-B), as in a real conflict.
        seed_file_state(&local, local_path, &compute_bytes_hash(b"REMOTE-A"), 8);
        let remote_state = RemoteState {
            hash: compute_bytes_hash(b"REMOTE-A"),
            size: 8,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-bbb").unwrap(),
            deleted: false,
        };
        let local_state = local.get_file_state(local_path).unwrap().unwrap();
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local_state,
                remote_state,
            ))
            .unwrap();

        let remote_path = format!("test-prefix/{local_path}");
        let mut engine = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"REMOTE-A", "2-bbb"),
            ),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        );
        engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepBoth)
            .await
            .expect("keep-both should succeed");

        // Conflict resolved; local file untouched.
        assert!(
            engine.get_conflicts().await.unwrap().is_empty(),
            "keep-both must remove the conflict record"
        );
        assert_eq!(
            std::fs::read_to_string(&local_full).unwrap(),
            "LOCAL-B",
            "local version is kept"
        );

        // A unique, dot-prefixed preservation copy holds the remote content and
        // the pre-existing backup survives untouched.
        let preserved = root.as_path().join("docs/.conflict.txt.remote");
        assert_eq!(
            std::fs::read_to_string(&preserved).unwrap(),
            "REMOTE-A",
            "remote version preserved under a unique local-only name"
        );

        // The preservation copy is deliberately local-only: no sync state is
        // recorded for it, so it is never queued for any remote path.
        assert!(
            engine
                .get_file_state("docs/.conflict.txt.remote")
                .await
                .unwrap()
                .is_none(),
            "preservation copy must not be tracked as a synced download"
        );

        // The kept local version's state converges so the next sync does not
        // re-upload it over the preserved remote version.
        let synced = engine
            .get_file_state(local_path)
            .await
            .unwrap()
            .expect("local state recorded");
        assert_eq!(synced.hash, compute_bytes_hash(b"LOCAL-B"));

        // A follow-up cycle with an empty remote feed makes no change: the kept
        // local file is no longer seen as pending and the dot-prefixed
        // preservation copy is ignored entirely.
        let mut engine2 = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                CannedCouch {
                    changes: Vec::new(),
                    last_seq: "1".to_string(),
                    ..CannedCouch::default()
                },
            ),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        );
        let report = engine2.sync().await.unwrap();
        assert_eq!(report.uploaded.0, 0, "kept local must not re-upload");
        assert_eq!(report.downloaded.0, 0);
        assert_eq!(report.conflicts, 0);
    }

    #[tokio::test]
    async fn resolve_conflict_keep_both_writes_to_configured_conflict_dir() {
        // When `conflict_dir` is configured, keep-both must write the remote
        // preservation copy under `<root>/<conflict_dir>/<relative-dir>/` rather
        // than next to the original file, keeping the local file intact and the
        // copy local-only.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let state_db = db_dir.path().join("state.db");

        let local_path = "docs/conflict.txt";
        let local_full = root.as_path().join(local_path);
        std::fs::create_dir_all(local_full.parent().unwrap()).unwrap();
        std::fs::write(&local_full, b"LOCAL-B").unwrap();

        let local = LocalDb::open(&state_db).unwrap();
        seed_file_state(&local, local_path, &compute_bytes_hash(b"REMOTE-A"), 8);
        let remote_state = RemoteState {
            hash: compute_bytes_hash(b"REMOTE-A"),
            size: 8,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-bbb").unwrap(),
            deleted: false,
        };
        let local_state = local.get_file_state(local_path).unwrap().unwrap();
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local_state,
                remote_state,
            ))
            .unwrap();

        let remote_path = format!("test-prefix/{local_path}");
        let mut engine = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"REMOTE-A", "2-bbb"),
            ),
            LocalDb::open(&state_db).unwrap(),
            root.clone(),
        )
        .with_conflict_dir(Some(PathBuf::from(".conflicts")));
        engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepBoth)
            .await
            .expect("keep-both should succeed");

        // The copy lands under the configured conflict_dir, mirroring the
        // original's subdirectory.
        let preserved = root.as_path().join(".conflicts/docs/conflict.txt.remote");
        assert_eq!(
            std::fs::read_to_string(&preserved).unwrap(),
            "REMOTE-A",
            "remote version preserved under the configured conflict_dir"
        );

        // No sync state is recorded for the preservation copy, so it is never
        // queued for a remote path.
        assert!(
            engine
                .get_file_state(".conflicts/docs/conflict.txt.remote")
                .await
                .unwrap()
                .is_none(),
            "preservation copy under conflict_dir must not be tracked"
        );

        // The local version is kept.
        assert_eq!(
            std::fs::read_to_string(&local_full).unwrap(),
            "LOCAL-B",
            "local version is kept"
        );
        assert!(
            engine.get_conflicts().await.unwrap().is_empty(),
            "keep-both must remove the conflict record"
        );
    }

    #[tokio::test]
    async fn resolve_conflict_keep_both_uses_numeric_suffix_when_preservation_exists() {
        // A second keep-both on the same path (same unique base) must not
        // clobber the first preservation copy: it picks the next free suffix.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local_path = "doc.txt";
        std::fs::write(root.as_path().join(local_path), b"LOCAL-B").unwrap();
        // A pre-existing (non-dot) .remote backup must survive resolution — the
        // new dot-prefixed preservation name never collides with it.
        let legacy_backup = root.as_path().join("doc.txt.remote");
        std::fs::write(&legacy_backup, b"old backup").unwrap();

        let local = test_local_db();
        seed_file_state(&local, local_path, &compute_bytes_hash(b"REMOTE-A"), 8);
        let remote_state = RemoteState {
            hash: compute_bytes_hash(b"REMOTE-A"),
            size: 8,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-bbb").unwrap(),
            deleted: false,
        };
        let local_state = local.get_file_state(local_path).unwrap().unwrap();
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local_state,
                remote_state,
            ))
            .unwrap();

        // First resolution writes .doc.txt.remote.
        let remote_path = format!("test-prefix/{local_path}");
        let mut engine = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"REMOTE-A", "2-bbb"),
            ),
            local,
            root.clone(),
        );
        engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepBoth)
            .await
            .unwrap();

        // Re-store a conflict and resolve again with a different remote version.
        let local2 = test_local_db();
        seed_file_state(&local2, local_path, &compute_bytes_hash(b"REMOTE-A"), 8);
        local2
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local2.get_file_state(local_path).unwrap().unwrap(),
                RemoteState {
                    hash: compute_bytes_hash(b"REMOTE-C"),
                    size: 8,
                    modified_at: Utc::now(),
                    couch_rev: CouchRev::new("3-ccc").unwrap(),
                    deleted: false,
                },
            ))
            .unwrap();
        let mut engine2 = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"REMOTE-C", "3-ccc"),
            ),
            local2,
            root.clone(),
        );
        engine2
            .resolve_conflict(local_path, ResolutionStrategy::KeepBoth)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.as_path().join(".doc.txt.remote")).unwrap(),
            "REMOTE-A",
            "first preservation copy survives"
        );
        assert_eq!(
            std::fs::read_to_string(root.as_path().join(".doc.txt.remote.1")).unwrap(),
            "REMOTE-C",
            "second preservation copy uses the next free suffix"
        );
        assert_eq!(
            std::fs::read_to_string(&legacy_backup).unwrap(),
            "old backup",
            "pre-existing non-dot .remote backup must survive"
        );
    }

    #[tokio::test]
    async fn resolve_conflict_keep_both_preserves_binary_remote_content() {
        // Keep-both must preserve arbitrary (non-UTF-8) remote bytes verbatim in
        // the local-only preservation copy — the remote version is never
        // decoded or truncated.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local_path = "blob.bin";
        std::fs::write(root.as_path().join(local_path), b"LOCAL-B").unwrap();

        let local = test_local_db();
        seed_file_state(&local, local_path, &compute_bytes_hash(b"LOCAL-B"), 7);
        let remote_state = RemoteState {
            hash: compute_bytes_hash(b"\x00\x01\x02\xff\xfe"),
            size: 5,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-bbb").unwrap(),
            deleted: false,
        };
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local.get_file_state(local_path).unwrap().unwrap(),
                remote_state,
            ))
            .unwrap();

        let remote_path = format!("test-prefix/{local_path}");
        let mut engine = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"\x00\x01\x02\xff\xfe", "2-bbb"),
            ),
            local,
            root.clone(),
        );
        engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepBoth)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(root.as_path().join(".blob.bin.remote")).unwrap(),
            b"\x00\x01\x02\xff\xfe",
            "binary remote bytes preserved verbatim"
        );
        assert_eq!(
            std::fs::read(root.as_path().join(local_path)).unwrap(),
            b"LOCAL-B",
            "local binary version kept"
        );
    }

    #[tokio::test]
    async fn resolve_conflict_keep_local_refuses_when_remote_changed_since_preview() {
        // A destructive keep-local must not overwrite a remote version that
        // advanced after the conflict was previewed: it refreshes the conflict's
        // remote snapshot and bails so the user re-reviews.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local_path = "doc.txt";
        std::fs::write(root.as_path().join(local_path), b"LOCAL-B").unwrap();

        let local = test_local_db();
        seed_file_state(&local, local_path, &compute_bytes_hash(b"LOCAL-B"), 7);
        // Snapshot the user previewed: remote at rev 2-bbb (REMOTE-A).
        let remote_state = RemoteState {
            hash: compute_bytes_hash(b"REMOTE-A"),
            size: 8,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-bbb").unwrap(),
            deleted: false,
        };
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local.get_file_state(local_path).unwrap().unwrap(),
                remote_state,
            ))
            .unwrap();

        // Live remote has since advanced to 3-ccc (REMOTE-C) — unseen by the
        // user who previewed 2-bbb.
        let remote_path = format!("test-prefix/{local_path}");
        let mut engine = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"REMOTE-C", "3-ccc"),
            ),
            local,
            root.clone(),
        );
        let err = engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepLocal)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Remote changed since the conflict was previewed"),
            "unexpected error: {err}"
        );

        // The conflict is NOT resolved; its remote snapshot is refreshed so the
        // next preview shows the current remote version.
        let conflicts = engine.get_conflicts().await.unwrap();
        assert_eq!(conflicts.len(), 1, "conflict must remain for re-review");
        assert_eq!(
            conflicts[0].remote_state.couch_rev.as_str(),
            "3-ccc",
            "conflict snapshot refreshed to the current remote version"
        );
        assert_eq!(
            std::fs::read_to_string(root.as_path().join(local_path)).unwrap(),
            "LOCAL-B",
            "local file untouched after refused keep-local"
        );
    }

    #[tokio::test]
    async fn resolve_conflict_keep_remote_refuses_when_local_edited_since_preview() {
        // A destructive keep-remote must not overwrite a local edit made after
        // the conflict was previewed: it refreshes the conflict's local snapshot
        // and bails so the user re-reviews.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let local_path = "doc.txt";
        // Snapshot the user previewed: local content "original".
        std::fs::write(root.as_path().join(local_path), b"original").unwrap();

        let local = test_local_db();
        seed_file_state(&local, local_path, &compute_bytes_hash(b"original"), 8);
        let remote_state = RemoteState {
            hash: compute_bytes_hash(b"REMOTE-A"),
            size: 8,
            modified_at: Utc::now(),
            couch_rev: CouchRev::new("2-bbb").unwrap(),
            deleted: false,
        };
        local
            .store_conflict(&Conflict::new(
                local_path.to_string(),
                local.get_file_state(local_path).unwrap().unwrap(),
                remote_state,
            ))
            .unwrap();

        // The user edited the local file after previewing (before resolving).
        std::fs::write(root.as_path().join(local_path), b"edited later").unwrap();

        let remote_path = format!("test-prefix/{local_path}");
        let mut engine = SyncEngine::new(
            test_canned_couch(
                "test-prefix/",
                resolve_canned_couch(&remote_path, b"REMOTE-A", "2-bbb"),
            ),
            local,
            root.clone(),
        );
        let err = engine
            .resolve_conflict(local_path, ResolutionStrategy::KeepRemote)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Local file changed since the conflict was previewed"),
            "unexpected error: {err}"
        );

        // The conflict is NOT resolved and the newer local edit is preserved;
        // the local snapshot is refreshed for re-review.
        let conflicts = engine.get_conflicts().await.unwrap();
        assert_eq!(conflicts.len(), 1, "conflict must remain for re-review");
        assert_eq!(
            conflicts[0].local_state.hash,
            compute_bytes_hash(b"edited later"),
            "conflict local snapshot refreshed to the current local version"
        );
        assert_eq!(
            std::fs::read_to_string(root.as_path().join(local_path)).unwrap(),
            "edited later",
            "newer local edit preserved after refused keep-remote"
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

    fn is_effective_root() -> bool {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
    }

    // ── Recoverable remote rebuild baseline (#6370) ────────────────────────

    #[tokio::test]
    async fn rebuild_upload_failure_retains_baseline_and_refuses_sync_until_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("up.txt"), b"local content").unwrap();

        // The FIRST save of the planned upload fails transiently.
        let canned = CannedCouch {
            save_errors_once: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::from(["prefix/up.txt".to_string()]),
            )),
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );
        // Baseline: prior tracked state that a rebuild would normally clear.
        engine
            .local_db
            .save_file_state(&FileState::new(
                "up.txt".to_string(),
                "deadbeef".to_string(),
                3,
                Utc::now() - Duration::days(1),
            ))
            .await
            .unwrap();

        // The first publication attempt fails after the plan is journaled but
        // before the baseline is cleared.
        let err = engine
            .rebuild_remote_from_local()
            .await
            .expect_err("transient upload failure must abort the rebuild");
        assert!(
            err.to_string().contains("simulated transient save failure"),
            "unexpected error: {err}"
        );

        // The baseline is retained: prior tracked state survives a failed
        // rebuild instead of being cleared up front.
        assert!(
            engine.get_file_state("up.txt").await.unwrap().is_some(),
            "a failed rebuild must retain the previous baseline, not clear it"
        );

        // The declared plan is journaled with progress so a restart can resume.
        let journal = engine
            .local_db
            .get_remote_rebuild_journal()
            .await
            .unwrap()
            .expect("a failed rebuild must leave a journal");
        assert_eq!(journal.uploads, vec!["up.txt".to_string()]);
        assert_eq!(
            journal.uploads_done, 0,
            "first upload failed before any progress"
        );

        // A normal sync is refused rather than misclassifying surviving remote
        // data as ordinary new edits or conflicts.
        let sync_err = engine
            .sync()
            .await
            .expect_err("normal sync must be refused while a rebuild is pending");
        assert!(
            sync_err
                .to_string()
                .contains("remote rebuild is incomplete"),
            "unexpected sync error: {sync_err}"
        );

        // Re-running the rebuild resumes/reconciles: the transient failure is
        // consumed, publication completes, the journal is cleared, and state is
        // reset so a normal sync resumes normally.
        let retry = engine
            .rebuild_remote_from_local()
            .await
            .expect("retry must complete the rebuild");
        assert_eq!(retry.uploaded.0, 1);
        assert_eq!(
            engine.local_db.get_remote_rebuild_journal().await.unwrap(),
            None,
            "a completed rebuild must clear the journal"
        );
        assert!(
            engine.get_file_state("up.txt").await.unwrap().is_none(),
            "a completed rebuild must reset tracked state"
        );
        engine
            .sync()
            .await
            .expect("normal sync resumes after rebuild");
    }

    #[tokio::test]
    async fn rebuild_delete_failure_retains_baseline_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(root.as_path().join("keep.txt"), b"keep").unwrap();

        // A remote orphan would be deleted by the rebuild; its delete fails once.
        let canned = CannedCouch {
            all_files: vec![remote_doc_at("prefix/old.txt", "1-abc", Vec::new())],
            delete_errors_once: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::from(["prefix/old.txt".to_string()]),
            )),
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );
        // Baseline: prior tracked state that a rebuild would normally clear.
        engine
            .local_db
            .save_file_state(&FileState::new(
                "keep.txt".to_string(),
                "cafe".to_string(),
                4,
                Utc::now() - Duration::days(1),
            ))
            .await
            .unwrap();

        let err = engine
            .rebuild_remote_from_local()
            .await
            .expect_err("transient delete failure must abort the rebuild");
        assert!(
            err.to_string()
                .contains("simulated transient delete failure"),
            "unexpected error: {err}"
        );

        // The upload completed before the failed delete; the baseline is kept.
        let journal = engine
            .local_db
            .get_remote_rebuild_journal()
            .await
            .unwrap()
            .expect("delete failure must leave a journal");
        assert_eq!(
            journal.uploads_done, 1,
            "upload completed before the failed delete"
        );
        assert_eq!(journal.deletes_done, 0, "delete failed before progress");
        assert_eq!(journal.deletes, vec!["prefix/old.txt".to_string()]);
        assert!(
            engine.get_file_state("keep.txt").await.unwrap().is_some(),
            "a failed rebuild must retain the previous baseline"
        );

        // Retry completes: the delete succeeds now, the journal is cleared, and
        // state is reset so a normal sync resumes normally.
        let retry = engine
            .rebuild_remote_from_local()
            .await
            .expect("retry must complete the rebuild");
        assert_eq!(retry.uploaded.0, 1);
        assert_eq!(retry.deleted_remote, 1);
        assert_eq!(
            engine.local_db.get_remote_rebuild_journal().await.unwrap(),
            None
        );
        assert!(
            engine.get_file_state("keep.txt").await.unwrap().is_none(),
            "a completed rebuild must reset tracked state"
        );
    }

    #[tokio::test]
    async fn rebuild_incomplete_scan_aborts_without_journal_or_remote_deletion() {
        if is_effective_root() {
            return; // root bypasses permission checks; the test is void.
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), b"keep").unwrap();
        let locked_dir = dir.path().join("locked");
        std::fs::create_dir_all(&locked_dir).unwrap();
        std::fs::write(locked_dir.join("hidden.txt"), b"secret").unwrap();
        let mut perms = std::fs::metadata(&locked_dir).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&locked_dir, perms).unwrap();

        let root = SyncDirPath::new(dir.path()).unwrap();

        // A remote orphan that WOULD be deleted if the rebuild ran.
        let canned = CannedCouch {
            all_files: vec![remote_doc_at("prefix/old.txt", "1-abc", Vec::new())],
            ..CannedCouch::default()
        };
        let mut engine = SyncEngine::with_ignore(
            test_canned_couch("prefix/", canned),
            test_local_db(),
            root.clone(),
            IgnoreMatcher::empty(),
        );
        // Baseline: prior tracked state that a rebuild would normally clear.
        engine
            .local_db
            .save_file_state(&FileState::new(
                "visible.txt".to_string(),
                "deadbeef".to_string(),
                4,
                Utc::now() - Duration::days(1),
            ))
            .await
            .unwrap();

        let err = engine
            .rebuild_remote_from_local()
            .await
            .expect_err("incomplete scan must abort the rebuild");
        assert!(
            err.to_string().contains("scan is incomplete"),
            "unexpected error: {err}"
        );
        assert_eq!(
            engine.local_db.get_remote_rebuild_journal().await.unwrap(),
            None,
            "an aborted rebuild must not leave a journal"
        );
        assert!(
            engine
                .get_file_state("visible.txt")
                .await
                .unwrap()
                .is_some(),
            "an aborted rebuild must retain the baseline"
        );
        assert_eq!(
            engine.couchdb.test_write_calls(),
            0,
            "no remote mutation may happen on an incomplete scan"
        );
    }
}
