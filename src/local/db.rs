use crate::models::{Checkpoint, Conflict, CouchRev, FileState, SyncIdentity};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::info;

/// Local `SQLite` database for state tracking
pub struct LocalDb {
    conn: Connection,
    /// Number of `save_file_state` writes issued (test-only; counts every
    /// INSERT OR REPLACE, including content-identical rewrites). Lets engine
    /// tests assert that no-op syncs skip redundant state rewrites.
    #[cfg(test)]
    save_file_state_calls: std::cell::Cell<u64>,
}

impl LocalDb {
    /// Open or create the local database
    ///
    /// # Errors
    ///
    /// Returns an error if the database file cannot be opened or the schema
    /// cannot be initialized.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn,
            #[cfg(test)]
            save_file_state_calls: std::cell::Cell::new(0),
        };
        db.init_schema()?;
        info!("Local database initialized");
        Ok(db)
    }

    /// Open an existing local database strictly read-only, without creating or
    /// migrating schema.
    ///
    /// Dry-run sync uses this so it can walk the full pipeline against an
    /// existing baseline without creating tables, running migrations, or
    /// writing anything. It fails if `path` does not exist — an absent
    /// baseline is handled by the caller with an ephemeral in-memory database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database file cannot be opened read-only.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn,
            #[cfg(test)]
            save_file_state_calls: std::cell::Cell::new(0),
        })
    }

    /// Create an in-memory database (for testing)
    ///
    /// # Errors
    ///
    /// Returns an error if the in-memory database cannot be created or the
    /// schema cannot be initialized.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn,
            #[cfg(test)]
            save_file_state_calls: std::cell::Cell::new(0),
        };
        db.init_schema()?;
        Ok(db)
    }

    /// Initialize database schema
    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r"
            -- File state table
            CREATE TABLE IF NOT EXISTS file_states (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                modified_at TEXT NOT NULL,
                couch_rev TEXT,
                last_sync_at TEXT NOT NULL,
                change_fingerprint TEXT
            );

            -- Conflicts table
            CREATE TABLE IF NOT EXISTS conflicts (
                path TEXT PRIMARY KEY,
                local_hash TEXT NOT NULL,
                local_size INTEGER NOT NULL,
                local_modified_at TEXT NOT NULL,
                remote_hash TEXT NOT NULL,
                remote_size INTEGER NOT NULL,
                remote_modified_at TEXT NOT NULL,
                remote_couch_rev TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                notified BOOLEAN DEFAULT FALSE,
                -- Faithful conflict baseline. Added by schema migration for
                -- pre-existing databases; new databases create them directly.
                remote_deleted BOOLEAN NOT NULL DEFAULT FALSE,
                local_couch_rev TEXT,
                local_last_sync_at TEXT
            );

            -- Per-channel conflict delivery state. Keyed by (channel, path,
            -- generation) so each opted-in channel tracks its own deliveries,
            -- a recurrent conflict at the same path (new generation) is
            -- delivered again, and one channel's success never suppresses
            -- another channel. The DB is per-root, so this is per-root too.
            CREATE TABLE IF NOT EXISTS conflict_delivery (
                channel TEXT NOT NULL,
                path TEXT NOT NULL,
                generation TEXT NOT NULL,
                delivered_at TEXT NOT NULL,
                PRIMARY KEY (channel, path, generation)
            );

            -- Sync checkpoint
            CREATE TABLE IF NOT EXISTS sync_checkpoint (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                last_seq TEXT,
                last_sync_at TEXT NOT NULL
            );

            -- Versioned data-migration + sync bookkeeping metadata. Keys gate
            -- one-time conservative migrations (e.g. historical remote-prefix
            -- pollution repair) so they run exactly once per remote scope.
            CREATE TABLE IF NOT EXISTS sync_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- Indexes
            CREATE INDEX IF NOT EXISTS idx_conflicts_notified ON conflicts(notified);
            ",
        )?;
        self.migrate_schema()?;
        Ok(())
    }

    /// Add columns to pre-existing databases created before the faithful
    /// conflict-baseline columns existed. Backwards-compatible: existing
    /// conflicts and checkpoints are left intact, and only the missing
    /// baseline columns are added. Fresh databases already contain them, so
    /// this is a no-op.
    fn migrate_schema(&self) -> Result<()> {
        let cols = self.column_names("conflicts")?;
        if !cols.iter().any(|c| c == "remote_deleted") {
            self.conn.execute_batch(
                "ALTER TABLE conflicts ADD COLUMN remote_deleted BOOLEAN NOT NULL DEFAULT FALSE",
            )?;
        }
        if !cols.iter().any(|c| c == "local_couch_rev") {
            self.conn
                .execute_batch("ALTER TABLE conflicts ADD COLUMN local_couch_rev TEXT")?;
        }
        if !cols.iter().any(|c| c == "local_last_sync_at") {
            self.conn
                .execute_batch("ALTER TABLE conflicts ADD COLUMN local_last_sync_at TEXT")?;
        }

        // Backfill the file-state change fingerprint for databases created
        // before it existed. Backwards-compatible: existing rows are left
        // intact and simply carry a NULL fingerprint, which the scanner treats
        // as "no reliable change evidence" and conservatively re-hashes.
        let file_cols = self.column_names("file_states")?;
        if !file_cols.iter().any(|c| c == "change_fingerprint") {
            self.conn
                .execute_batch("ALTER TABLE file_states ADD COLUMN change_fingerprint TEXT")?;
        }
        Ok(())
    }

    /// Column names of a table, from `PRAGMA table_info`.
    fn column_names(&self, table: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names)
    }

    // === File State Operations ===

    /// Serialize a change fingerprint for storage in the `change_fingerprint`
    /// TEXT column.
    fn fingerprint_to_string(fp: &crate::models::ChangeFingerprint) -> String {
        format!("{}:{}", fp.secs, fp.nanos)
    }

    /// Parse a change fingerprint from its stored TEXT form, returning `None`
    /// for a `NULL`/absent value or an unparseable string.
    fn string_to_fingerprint(s: &str) -> Option<crate::models::ChangeFingerprint> {
        let (secs, nanos) = s.split_once(':')?;
        Some(crate::models::ChangeFingerprint {
            secs: secs.parse().ok()?,
            nanos: nanos.parse().ok()?,
        })
    }

    /// Get file state by path
    ///
    /// # Errors
    ///
    /// Returns an error if the query against the local database fails.
    pub fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, hash, size, modified_at, couch_rev, last_sync_at, change_fingerprint 
             FROM file_states WHERE path = ?",
        )?;

        let state = stmt
            .query_row(params![path], |row| {
                let size: i64 = row.get(2)?;
                Ok(FileState {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                    size: i64_to_u64(size)?,
                    modified_at: row.get(3)?,
                    couch_rev: row.get(4)?,
                    last_sync_at: row.get(5)?,
                    change_fingerprint: row
                        .get::<_, Option<String>>(6)?
                        .as_deref()
                        .and_then(Self::string_to_fingerprint),
                })
            })
            .optional()?;

        Ok(state)
    }

    /// Get file states for a set of paths in a single batch query.
    ///
    /// Returns a map keyed by path containing only paths that have a stored
    /// state; paths without a stored state are simply absent from the result,
    /// matching the per-path [`Self::get_file_state`] behavior. The whole
    /// batch is loaded with one `WHERE path IN (...)` prepared statement
    /// (chunked only when the path set would exceed `SQLite`'s bind-parameter
    /// limit) instead of one prepared-statement query per path.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch query against the local database fails.
    pub fn get_file_states(&self, paths: &[&str]) -> Result<HashMap<String, FileState>> {
        // SQLite limits the number of bind parameters in a single statement.
        // Chunk large path sets so we never exceed the limit while still
        // issuing far fewer queries than one-per-path.
        const MAX_PARAMS_PER_QUERY: usize = 900;
        let mut states = HashMap::new();
        if paths.is_empty() {
            return Ok(states);
        }

        for chunk in paths.chunks(MAX_PARAMS_PER_QUERY) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT path, hash, size, modified_at, couch_rev, last_sync_at, change_fingerprint \
                 FROM file_states WHERE path IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                let size: i64 = row.get(2)?;
                Ok(FileState {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                    size: i64_to_u64(size)?,
                    modified_at: row.get(3)?,
                    couch_rev: row.get(4)?,
                    last_sync_at: row.get(5)?,
                    change_fingerprint: row
                        .get::<_, Option<String>>(6)?
                        .as_deref()
                        .and_then(Self::string_to_fingerprint),
                })
            })?;
            for state in rows {
                let state = state?;
                states.insert(state.path.clone(), state);
            }
        }

        Ok(states)
    }

    /// Save or update file state
    ///
    /// # Errors
    ///
    /// Returns an error if the file state cannot be written to the local
    /// database.
    pub fn save_file_state(&self, state: &FileState) -> Result<()> {
        #[cfg(test)]
        self.save_file_state_calls
            .set(self.save_file_state_calls.get() + 1);

        // Check if this is a new file
        let is_new = self.get_file_state(&state.path)?.is_none();

        self.conn.execute(
            "INSERT OR REPLACE INTO file_states
             (path, hash, size, modified_at, couch_rev, last_sync_at, change_fingerprint)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                &state.path,
                &state.hash,
                u64_to_i64(state.size)?,
                &state.modified_at,
                &state.couch_rev,
                &state.last_sync_at,
                state
                    .change_fingerprint
                    .as_ref()
                    .map(Self::fingerprint_to_string),
            ],
        )?;

        if is_new {
            info!(
                "Added new file to local database: {} (size: {} bytes)",
                state.path, state.size
            );
        }

        Ok(())
    }

    /// Number of `save_file_state` writes issued since this database was
    /// created (test-only).
    #[cfg(test)]
    #[allow(clippy::missing_const_for_fn)]
    pub fn save_file_state_calls(&self) -> u64 {
        self.save_file_state_calls.get()
    }

    /// Delete file state
    /// Delete a file state by path.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete statement cannot be executed.
    pub fn delete_file_state(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM file_states WHERE path = ?", params![path])?;
        Ok(())
    }

    /// Get all file states
    /// Get all file states.
    ///
    /// # Errors
    ///
    /// Returns an error if the query against the local database fails.
    pub fn get_all_file_states(&self) -> Result<Vec<FileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, hash, size, modified_at, couch_rev, last_sync_at, change_fingerprint 
             FROM file_states",
        )?;

        let states = stmt
            .query_map([], |row| {
                let size: i64 = row.get(2)?;
                Ok(FileState {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                    size: i64_to_u64(size)?,
                    modified_at: row.get(3)?,
                    couch_rev: row.get(4)?,
                    last_sync_at: row.get(5)?,
                    change_fingerprint: row
                        .get::<_, Option<String>>(6)?
                        .as_deref()
                        .and_then(Self::string_to_fingerprint),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(states)
    }

    /// Clear all tracked file state
    /// Remove all file states.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete statement cannot be executed.
    pub fn clear_file_states(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM file_states", [])?;
        Ok(count)
    }

    // === Conflict Operations ===

    /// Store conflict
    ///
    /// # Errors
    ///
    /// Returns an error if the conflict cannot be inserted into the local
    /// database.
    pub fn store_conflict(&self, conflict: &Conflict) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO conflicts 
             (path, local_hash, local_size, local_modified_at,
              remote_hash, remote_size, remote_modified_at, remote_couch_rev,
              detected_at, notified, remote_deleted, local_couch_rev,
              local_last_sync_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &conflict.path,
                &conflict.local_state.hash,
                i64::try_from(conflict.local_state.size).unwrap_or(i64::MAX),
                &conflict.local_state.modified_at,
                &conflict.remote_state.hash,
                i64::try_from(conflict.remote_state.size).unwrap_or(i64::MAX),
                &conflict.remote_state.modified_at,
                &conflict.remote_state.couch_rev,
                &conflict.detected_at,
                conflict.is_notified(),
                conflict.remote_state.deleted,
                conflict.local_state.couch_rev.as_deref(),
                &conflict.local_state.last_sync_at,
            ],
        )?;
        Ok(())
    }

    /// Get all conflicts
    /// Get all stored conflicts.
    ///
    /// # Errors
    ///
    /// Returns an error if the query against the local database fails.
    pub fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, local_hash, local_size, local_modified_at,
                    remote_hash, remote_size, remote_modified_at, remote_couch_rev,
                    detected_at, notified, remote_deleted, local_couch_rev,
                    local_last_sync_at
             FROM conflicts ORDER BY detected_at DESC",
        )?;

        let conflicts = stmt
            .query_map([], row_to_conflict)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(conflicts)
    }

    /// Get conflict by path
    /// Get a stored conflict by path.
    ///
    /// # Errors
    ///
    /// Returns an error if the query against the local database fails.
    pub fn get_conflict(&self, path: &str) -> Result<Option<Conflict>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, local_hash, local_size, local_modified_at,
                    remote_hash, remote_size, remote_modified_at, remote_couch_rev,
                    detected_at, notified, remote_deleted, local_couch_rev,
                    local_last_sync_at
             FROM conflicts WHERE path = ?",
        )?;

        let conflict = stmt.query_row(params![path], row_to_conflict).optional()?;

        Ok(conflict)
    }

    /// Mark conflict as notified
    /// Mark a conflict as notified.
    ///
    /// # Errors
    ///
    /// Returns an error if the update statement cannot be executed.
    pub fn mark_conflict_notified(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE conflicts SET notified = TRUE WHERE path = ?",
            params![path],
        )?;
        Ok(())
    }

    /// Record that a conflict was delivered on `channel` for a specific
    /// `generation`. Delivery state is keyed by (channel, path, generation) so
    /// each channel tracks its own deliveries, a recurrent conflict (new
    /// generation) is delivered again, and one channel's success never
    /// suppresses another channel. The DB is per-root, so this is per-root.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert cannot be executed.
    pub fn mark_conflict_delivered(
        &self,
        channel: &str,
        path: &str,
        generation: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO conflict_delivery (channel, path, generation, delivered_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![channel, path, generation, Utc::now()],
        )?;
        Ok(())
    }

    /// Return the set of `(path, generation)` pairs already delivered on
    /// `channel`, so a caller can decide which conflicts are brand-new.
    ///
    /// # Errors
    ///
    /// Returns an error if the query against the local database fails.
    pub fn delivered_conflicts(&self, channel: &str) -> Result<HashSet<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, generation FROM conflict_delivery WHERE channel = ?1")?;
        let rows = stmt.query_map(params![channel], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut set = HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        Ok(set)
    }

    /// Delete conflict
    /// Delete a stored conflict by path.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete statement cannot be executed.
    pub fn delete_conflict(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM conflicts WHERE path = ?", params![path])?;
        Ok(())
    }

    /// Clear all conflicts
    /// Remove all stored conflicts.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete statement cannot be executed.
    pub fn clear_conflicts(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM conflicts", [])?;
        Ok(count)
    }

    // === Sync Checkpoint Operations ===

    /// Get last sync checkpoint
    /// Get the stored replication checkpoint, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query against the local database fails.
    pub fn get_checkpoint(&self) -> Result<Option<Checkpoint>> {
        let result = self
            .conn
            .query_row(
                "SELECT last_seq, last_sync_at FROM sync_checkpoint WHERE id = 1",
                [],
                |row| {
                    let seq: String = row.get(0)?;
                    let timestamp: DateTime<Utc> = row.get(1)?;
                    Ok(Checkpoint::new(seq, timestamp))
                },
            )
            .optional()?;
        Ok(result)
    }

    /// Save sync checkpoint
    /// Store the replication checkpoint sequence.
    ///
    /// # Errors
    ///
    /// Returns an error if the checkpoint cannot be written.
    pub fn save_checkpoint(&self, seq: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_checkpoint (id, last_seq, last_sync_at)
             VALUES (1, ?, ?)",
            params![seq, Utc::now()],
        )?;
        Ok(())
    }

    /// Clear the sync checkpoint
    /// Remove the stored replication checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete statement cannot be executed.
    pub fn clear_checkpoint(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM sync_checkpoint", [])?;
        Ok(count)
    }

    /// Remove tracked state that should not survive an authoritative rebuild.
    /// Reset all sync state (file states, conflicts, and checkpoint).
    ///
    /// The three deletes run inside a single `SQLite` transaction so the reset is
    /// atomic: an interruption mid-reset can never leave file states, conflicts
    /// and the checkpoint in a partially-cleared, inconsistent state.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the reset operations fail.
    pub fn reset_sync_state(&self) -> Result<()> {
        // `unchecked_transaction` is safe here because callers (the engine's
        // `AsyncLocalDb`) always hold the connection mutex, so no other thread
        // can observe a half-committed reset.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM file_states", [])?;
        tx.execute("DELETE FROM conflicts", [])?;
        tx.execute("DELETE FROM sync_checkpoint", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically replace the tracked sync baseline with a freshly rebuilt one.
    ///
    /// Used by the authoritative local rebuild at the verified transition: it
    /// clears all prior sync state (file states, conflicts, checkpoint) and
    /// installs the new `states` in a single `SQLite` transaction, so the reset
    /// and the new baseline commit or roll back together. Obsolete file states
    /// are dropped only after their replacement content has been verified and
    /// staged.
    ///
    /// # Errors
    ///
    /// Returns an error if the transaction fails.
    pub fn commit_rebuild_state(&self, states: &[FileState]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM file_states", [])?;
        tx.execute("DELETE FROM conflicts", [])?;
        tx.execute("DELETE FROM sync_checkpoint", [])?;
        for state in states {
            tx.execute(
                "INSERT OR REPLACE INTO file_states
                 (path, hash, size, modified_at, couch_rev, last_sync_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    &state.path,
                    &state.hash,
                    u64_to_i64(state.size)?,
                    &state.modified_at,
                    &state.couch_rev,
                    &state.last_sync_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read a key/value metadata entry used to gate one-time versioned data
    /// migrations (see `sync_meta`). Returns `Ok(None)` when the key is absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata query fails.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT value FROM sync_meta WHERE key = ?1")?;
        let value = stmt.query_row([key], |row| row.get(0)).optional()?;
        Ok(value)
    }

    /// Write a key/value metadata entry used to record that a one-time
    /// versioned data migration has run.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata write fails.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Read the stored [`SyncIdentity`] this local state is bound to, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata query or deserialization fails.
    pub fn get_sync_identity(&self) -> Result<Option<SyncIdentity>> {
        let Some(raw) = self.get_meta(SYNC_IDENTITY_KEY)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(&raw)?))
    }

    /// Persist the [`SyncIdentity`] this local state is bound to.
    ///
    /// # Errors
    ///
    /// Returns an error if the identity cannot be serialized or written.
    pub fn set_sync_identity(&self, identity: &SyncIdentity) -> Result<()> {
        let raw = serde_json::to_string(identity)?;
        self.set_meta(SYNC_IDENTITY_KEY, &raw)
    }

    /// Validate that stored state may be reused for the given sync mapping.
    ///
    /// A database with no recorded identity (fresh, or pre-identity legacy
    /// state) is conservatively bound to `identity` and its existing state is
    /// kept; the binding is persisted so a later mapping change is detected.
    ///
    /// # Errors
    ///
    /// Returns an error if the identity cannot be read or the conservative
    /// bind cannot be persisted.
    pub fn validate_sync_identity(&self, identity: &SyncIdentity) -> Result<SyncIdentityCheck> {
        match self.get_sync_identity()? {
            None => {
                self.set_sync_identity(identity)?;
                Ok(SyncIdentityCheck::Bound)
            }
            Some(stored) if stored == *identity => Ok(SyncIdentityCheck::Matched),
            Some(stored) => Ok(SyncIdentityCheck::Mismatched {
                stored,
                current: identity.clone(),
            }),
        }
    }

    /// Read the persisted record of an in-progress authoritative remote
    /// rebuild, if one is pending. Returns `Ok(None)` when no rebuild is
    /// incomplete.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata query or deserialization fails.
    pub fn get_remote_rebuild_journal(&self) -> Result<Option<RemoteRebuildJournal>> {
        let Some(raw) = self.get_meta(REMOTE_REBUILD_JOURNAL_KEY)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(&raw)?))
    }

    /// Persist the declared plan/progress of an in-progress authoritative
    /// remote rebuild. Written before the first upload and updated as
    /// publication progresses, so an interrupted run leaves an accurate
    /// journal that a later restart can resume or reconcile.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal cannot be serialized or written.
    pub fn set_remote_rebuild_journal(&self, journal: &RemoteRebuildJournal) -> Result<()> {
        let raw = serde_json::to_string(journal)?;
        self.set_meta(REMOTE_REBUILD_JOURNAL_KEY, &raw)
    }

    /// Clear the persisted remote-rebuild journal. Called only once remote
    /// publication has completed and tracked state has been reset, so a
    /// normal sync resumes normally.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal row cannot be deleted.
    pub fn clear_remote_rebuild_journal(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM sync_meta WHERE key = ?1",
            params![REMOTE_REBUILD_JOURNAL_KEY],
        )?;
        Ok(())
    }

    /// Validate that stored state may be reused for the given sync mapping
    /// without writing anything.
    ///
    /// Dry-run sync uses this against a read-only or ephemeral baseline: a
    /// database with no recorded identity (fresh or legacy) is accepted and its
    /// state kept, but the conservative bind is NOT persisted, honoring the
    /// no-write dry-run contract.
    ///
    /// # Errors
    ///
    /// Returns an error if the identity cannot be read.
    pub fn validate_sync_identity_read_only(
        &self,
        identity: &SyncIdentity,
    ) -> Result<SyncIdentityCheck> {
        match self.get_sync_identity()? {
            None => Ok(SyncIdentityCheck::Bound),
            Some(stored) if stored == *identity => Ok(SyncIdentityCheck::Matched),
            Some(stored) => Ok(SyncIdentityCheck::Mismatched {
                stored,
                current: identity.clone(),
            }),
        }
    }
}

/// Key in `sync_meta` storing the serialized [`SyncIdentity`] that local state
/// is bound to.
const SYNC_IDENTITY_KEY: &str = "sync_identity";

/// Key in `sync_meta` storing the serialized [`RemoteRebuildJournal`] of an
/// in-progress authoritative remote rebuild.
const REMOTE_REBUILD_JOURNAL_KEY: &str = "remote_rebuild_journal";

/// Persisted record of an in-progress (or interrupted) authoritative remote
/// rebuild.
///
/// The journal is written with the full declared plan *before* the first
/// upload and survives until publication completes. This keeps the previous
/// baseline intact instead of clearing it up front: if an upload or delete
/// fails partway, the journal and the pre-rebuild tracked state remain, so a
/// restart can resume or reconcile the authoritative plan and a normal sync is
/// refused rather than misclassifying surviving remote data as ordinary new
/// edits or conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRebuildJournal {
    /// Planned local uploads in publication order.
    pub uploads: Vec<String>,
    /// Planned remote deletes in publication order.
    pub deletes: Vec<String>,
    /// Uploads already published.
    pub uploads_done: usize,
    /// Remote deletes already published.
    pub deletes_done: usize,
}

impl RemoteRebuildJournal {
    /// Return a copy of the journal with publication progress advanced to the
    /// given completed counts, for persisting an accurate checkpoint mid-run.
    #[must_use]
    pub const fn with_progress(mut self, uploads_done: usize, deletes_done: usize) -> Self {
        self.uploads_done = uploads_done;
        self.deletes_done = deletes_done;
        self
    }
}

/// Outcome of validating the current sync mapping against the stored identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncIdentityCheck {
    /// Stored state is bound to the exact same mapping — safe to reuse.
    Matched,
    /// No identity was recorded (fresh or legacy database). The current
    /// mapping is persisted so future changes are detected, and existing state
    /// is conservatively kept.
    Bound,
    /// The mapping changed since the state was recorded; the stored state must
    /// not be reused without an explicit reset/rebuild.
    Mismatched {
        /// The mapping the stored state was produced under.
        stored: SyncIdentity,
        /// The mapping the engine is about to sync.
        current: SyncIdentity,
    },
}

fn u64_to_i64(value: u64) -> Result<i64> {
    Ok(i64::try_from(value)?)
}

/// Map a `conflicts` row to a [`Conflict`], shared by both conflict lookup
/// APIs so they always agree. Reads back the persisted baseline faithfully:
/// the remote deletion flag, the local couch revision, and the local
/// `last_sync_at`. Pre-migration rows (added before these columns existed)
/// have NULL local baseline; their `last_sync_at` falls back to the stored
/// `detected_at` rather than fabricating a current timestamp.
fn row_to_conflict(row: &rusqlite::Row<'_>) -> rusqlite::Result<Conflict> {
    use crate::models::{NotificationMode, RemoteState};

    let path: String = row.get(0)?;
    let detected_at: DateTime<Utc> = row.get(8)?;
    let local_state = FileState {
        path: path.clone(),
        hash: row.get(1)?,
        size: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        modified_at: row.get(3)?,
        couch_rev: row
            .get::<_, Option<String>>(11)?
            .as_deref()
            .and_then(CouchRev::new),
        last_sync_at: row
            .get::<_, Option<DateTime<Utc>>>(12)?
            .unwrap_or(detected_at),
        // A conflict record is a historical snapshot of a resolved conflict,
        // not a live scan fingerprint; it carries no change evidence.
        change_fingerprint: None,
    };

    let remote_state = RemoteState {
        hash: row.get(4)?,
        size: u64::try_from(row.get::<_, i64>(5)?).unwrap_or(0),
        modified_at: row.get(6)?,
        couch_rev: CouchRev::new(row.get::<_, String>(7)?.as_str()).unwrap_or_default(),
        deleted: row.get(10)?,
    };

    let mut conflict = Conflict::new(path, local_state, remote_state);
    conflict.detected_at = detected_at;
    let notified: bool = row.get(9)?;
    if notified {
        conflict.notification_mode = NotificationMode::Notified;
    }

    Ok(conflict)
}

fn i64_to_u64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| {
        let err = std::io::Error::new(std::io::ErrorKind::InvalidData, "negative size");
        rusqlite::Error::FromSqlConversionFailure(0, Type::Integer, Box::new(err))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::Conflict;
    use chrono::Utc;

    // ── helpers ──────────────────────────────────────────────────────────

    fn test_db() -> LocalDb {
        LocalDb::open_in_memory().expect("failed to create in-memory database")
    }

    fn make_file_state(path: &str) -> FileState {
        FileState::new(path.to_string(), "abc123".to_string(), 1024, Utc::now())
    }

    fn make_conflict(path: &str) -> Conflict {
        let local = make_file_state(path);
        let remote = crate::models::RemoteState {
            hash: "remote_hash".to_string(),
            size: 4096,
            modified_at: Utc::now(),
            couch_rev: crate::models::CouchRev::new("1-abc").unwrap(),
            deleted: false,
        };
        Conflict::new(path.to_string(), local, remote)
    }

    fn make_conflict_with_baseline(
        path: &str,
        local_couch_rev: Option<&str>,
        local_last_sync_at: DateTime<Utc>,
        remote_deleted: bool,
    ) -> Conflict {
        let mut local = make_file_state(path);
        local.couch_rev = local_couch_rev.and_then(crate::models::CouchRev::new);
        local.last_sync_at = local_last_sync_at;
        let remote = crate::models::RemoteState {
            hash: "remote_hash".to_string(),
            size: 4096,
            modified_at: Utc::now(),
            couch_rev: crate::models::CouchRev::new("1-abc").unwrap(),
            deleted: remote_deleted,
        };
        Conflict::new(path.to_string(), local, remote)
    }

    // ── file_state operations ────────────────────────────────────────────

    #[test]
    fn test_save_and_get_file_state() {
        let db = test_db();
        let state = make_file_state("/test/file.txt");

        db.save_file_state(&state).expect("save_file_state failed");
        let loaded = db
            .get_file_state("/test/file.txt")
            .expect("get_file_state failed");

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.path, "/test/file.txt");
        assert_eq!(loaded.hash, "abc123");
        assert_eq!(loaded.size, 1024);
    }

    #[test]
    fn test_get_file_state_missing() {
        let db = test_db();
        let loaded = db
            .get_file_state("/nonexistent")
            .expect("get_file_state failed");
        assert!(loaded.is_none());
    }

    #[test]
    fn test_file_state_datetime_round_trip() {
        let db = test_db();
        // Fixed timestamps with nanosecond precision to catch any precision
        // loss in the direct DateTime<Utc> SQLite binding (rusqlite's chrono
        // ToSql formats %f fractional seconds and FromSql parses them back).
        let modified_at =
            chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("valid ts");
        let last_sync_at =
            chrono::DateTime::from_timestamp(1_700_000_100, 987_654_321).expect("valid ts");

        let mut state = FileState::new(
            "/roundtrip.txt".to_string(),
            "roundtrip-hash".to_string(),
            4096,
            modified_at,
        );
        state.last_sync_at = last_sync_at;
        db.save_file_state(&state).expect("save_file_state failed");

        let loaded = db
            .get_file_state("/roundtrip.txt")
            .expect("get_file_state failed")
            .expect("state present");
        assert_eq!(loaded.modified_at, modified_at);
        assert_eq!(loaded.last_sync_at, last_sync_at);
    }

    #[test]
    fn test_file_state_change_fingerprint_round_trip() {
        let db = test_db();
        let mut state = make_file_state("/fp.txt");
        // A change fingerprint must survive the SQLite TEXT round-trip exactly,
        // including nanosecond resolution and NULL (no-evidence) values.
        state.change_fingerprint = Some(crate::models::ChangeFingerprint {
            secs: 1_700_000_000,
            nanos: 123_456_789,
        });
        db.save_file_state(&state)
            .expect("save with fingerprint failed");

        let loaded = db
            .get_file_state("/fp.txt")
            .expect("get_file_state failed")
            .expect("state present");
        assert_eq!(loaded.change_fingerprint, state.change_fingerprint);

        // NULL (None) fingerprints round-trip back to None, not an error.
        state.change_fingerprint = None;
        db.save_file_state(&state)
            .expect("save without fingerprint failed");
        let loaded = db
            .get_file_state("/fp.txt")
            .expect("get_file_state failed")
            .expect("state present");
        assert_eq!(loaded.change_fingerprint, None);
    }

    #[test]
    fn test_get_file_states_batch_matches_per_path() {
        let db = test_db();
        // A mixed set of paths: some present, some missing, plus duplicates
        // to exercise dedup at the caller.
        for (i, p) in ["/a.txt", "/b.txt", "/c.txt"].iter().enumerate() {
            let mut state = make_file_state(p);
            state.hash = format!("hash-{i}");
            db.save_file_state(&state).expect("save_file_state failed");
        }
        let paths = ["/a.txt", "/b.txt", "/missing.txt", "/c.txt", "/a.txt"];
        let batch = db.get_file_states(&paths).expect("get_file_states failed");

        // Batch result must contain exactly the stored paths from the set.
        assert_eq!(batch.len(), 3);
        for p in ["/a.txt", "/b.txt", "/c.txt"] {
            let per_path = db.get_file_state(p).expect("get_file_state failed");
            assert_eq!(batch.get(p), per_path.as_ref(), "batch mismatch for {p}");
        }
        assert!(!batch.contains_key("/missing.txt"));
    }

    #[test]
    fn test_save_file_state_update_existing() {
        let db = test_db();
        let mut state = make_file_state("/test/file.txt");
        db.save_file_state(&state).expect("first save");

        // Update with new hash and size
        state.hash = "updated_hash".to_string();
        state.size = 2048;
        db.save_file_state(&state).expect("update save");

        let loaded = db
            .get_file_state("/test/file.txt")
            .expect("get_file_state")
            .unwrap();
        assert_eq!(loaded.hash, "updated_hash");
        assert_eq!(loaded.size, 2048);
    }

    #[test]
    fn test_delete_file_state() {
        let db = test_db();
        let state = make_file_state("/test/file.txt");
        db.save_file_state(&state).expect("save");

        db.delete_file_state("/test/file.txt").expect("delete");

        let loaded = db.get_file_state("/test/file.txt").expect("get_file_state");
        assert!(loaded.is_none());
    }

    #[test]
    fn test_get_all_file_states_empty() {
        let db = test_db();
        let states = db.get_all_file_states().expect("get_all_file_states");
        assert_eq!(states, []);
    }

    #[test]
    fn test_get_all_file_states_multiple() {
        let db = test_db();
        db.save_file_state(&make_file_state("/a.txt"))
            .expect("save a");
        db.save_file_state(&make_file_state("/b.txt"))
            .expect("save b");
        db.save_file_state(&make_file_state("/c.txt"))
            .expect("save c");

        let states = db.get_all_file_states().expect("get_all_file_states");
        assert_eq!(states.len(), 3);

        let paths: Vec<&str> = states.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"/a.txt"));
        assert!(paths.contains(&"/b.txt"));
        assert!(paths.contains(&"/c.txt"));
    }

    #[test]
    fn test_clear_file_states() {
        let db = test_db();
        db.save_file_state(&make_file_state("/a.txt"))
            .expect("save a");
        db.save_file_state(&make_file_state("/b.txt"))
            .expect("save b");

        let cleared = db.clear_file_states().expect("clear_file_states");
        assert_eq!(cleared, 2);

        let states = db.get_all_file_states().expect("get_all_file_states");
        assert_eq!(states, []);
    }

    // ── conflict operations ──────────────────────────────────────────────

    #[test]
    fn test_store_and_get_conflicts() {
        let db = test_db();
        let conflict = make_conflict("/test/conflict.txt");
        db.store_conflict(&conflict).expect("store_conflict");

        let conflicts = db.get_conflicts().expect("get_conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "/test/conflict.txt");
        assert_eq!(conflicts[0].local_state.hash, "abc123");
        assert_eq!(conflicts[0].remote_state.hash, "remote_hash");
        assert!(!conflicts[0].is_notified());
    }

    #[test]
    fn test_get_conflicts_empty() {
        let db = test_db();
        let conflicts = db.get_conflicts().expect("get_conflicts");
        assert!(conflicts.is_empty());
    }

    #[test]
    fn test_get_conflict_by_path() {
        let db = test_db();
        db.store_conflict(&make_conflict("/a.txt"))
            .expect("store a");
        db.store_conflict(&make_conflict("/b.txt"))
            .expect("store b");

        let found = db.get_conflict("/a.txt").expect("get_conflict");
        assert!(found.is_some());
        assert_eq!(found.unwrap().path, "/a.txt");

        let not_found = db.get_conflict("/nonexistent").expect("get_conflict");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_store_conflict_replace_existing() {
        let db = test_db();
        let mut conflict = make_conflict("/test/conflict.txt");
        db.store_conflict(&conflict).expect("first store");

        conflict.mark_notified();
        conflict.remote_state.hash = "updated_remote".to_string();
        db.store_conflict(&conflict).expect("second store");

        let loaded = db
            .get_conflict("/test/conflict.txt")
            .expect("get_conflict")
            .unwrap();
        assert!(loaded.is_notified());
        assert_eq!(loaded.remote_state.hash, "updated_remote");
    }

    #[test]
    fn test_mark_conflict_notified() {
        let db = test_db();
        db.store_conflict(&make_conflict("/test/conflict.txt"))
            .expect("store");

        db.mark_conflict_notified("/test/conflict.txt")
            .expect("mark_notified");

        let loaded = db
            .get_conflict("/test/conflict.txt")
            .expect("get_conflict")
            .unwrap();
        assert!(loaded.is_notified());
    }

    #[test]
    fn test_delete_conflict() {
        let db = test_db();
        db.store_conflict(&make_conflict("/test/conflict.txt"))
            .expect("store");

        db.delete_conflict("/test/conflict.txt").expect("delete");

        let loaded = db.get_conflict("/test/conflict.txt").expect("get_conflict");
        assert!(loaded.is_none());
    }

    #[test]
    fn test_conflict_baseline_round_trip_edit_edit() {
        let db = test_db();
        // A non-null local couch_rev and a non-now last_sync_at must survive
        // write/read faithfully through both lookup APIs, so neither is
        // synthesized on the way out.
        let last_sync =
            chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("valid ts");
        let conflict = make_conflict_with_baseline("/e/e.txt", Some("1-abc"), last_sync, false);
        db.store_conflict(&conflict).expect("store_conflict");

        let from_list = db.get_conflicts().expect("get_conflicts");
        assert_eq!(from_list.len(), 1);
        let from_by_path = db
            .get_conflict("/e/e.txt")
            .expect("get_conflict")
            .expect("present");

        for loaded in [&from_list[0], &from_by_path] {
            assert!(!loaded.remote_state.deleted);
            assert_eq!(loaded.local_state.couch_rev.as_deref(), Some("1-abc"));
            assert_eq!(loaded.local_state.last_sync_at, last_sync);
            assert_eq!(loaded.local_state.hash, "abc123");
            assert_eq!(loaded.remote_state.hash, "remote_hash");
            assert_eq!(loaded.remote_state.size, 4096);
        }
        assert_eq!(from_list[0].local_state, from_by_path.local_state);
    }

    #[test]
    fn test_conflict_baseline_round_trip_edit_delete() {
        let db = test_db();
        // The remote deletion flag (edit/delete conflict) must round-trip as
        // true instead of being hard-coded back to false on read.
        let last_sync =
            chrono::DateTime::from_timestamp(1_700_000_100, 987_654_321).expect("valid ts");
        let conflict = make_conflict_with_baseline("/e/d.txt", Some("2-def"), last_sync, true);
        db.store_conflict(&conflict).expect("store_conflict");

        let from_list = db.get_conflicts().expect("get_conflicts");
        assert_eq!(from_list.len(), 1);
        let from_by_path = db
            .get_conflict("/e/d.txt")
            .expect("get_conflict")
            .expect("present");

        for loaded in [&from_list[0], &from_by_path] {
            assert!(loaded.remote_state.deleted);
            assert_eq!(loaded.local_state.couch_rev.as_deref(), Some("2-def"));
            assert_eq!(loaded.local_state.last_sync_at, last_sync);
        }
        assert_eq!(from_list[0].local_state, from_by_path.local_state);
    }

    #[test]
    fn test_conflict_baseline_survives_reopen() {
        // The baseline must survive a full close/reopen of the database file.
        let db_path =
            std::env::temp_dir().join(format!("conflict_reopen_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        {
            let db = LocalDb::open(&db_path).expect("open");
            let last_sync =
                chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("valid ts");
            db.store_conflict(&make_conflict_with_baseline(
                "/reopen.txt",
                Some("3-ghi"),
                last_sync,
                true,
            ))
            .expect("store_conflict");
        }

        let db = LocalDb::open(&db_path).expect("reopen");
        let loaded = db
            .get_conflict("/reopen.txt")
            .expect("get_conflict")
            .expect("present");
        assert!(loaded.remote_state.deleted);
        assert_eq!(loaded.local_state.couch_rev.as_deref(), Some("3-ghi"));
        assert_eq!(
            loaded.local_state.last_sync_at,
            chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("valid ts")
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_old_schema_migration_preserves_conflicts_and_checkpoint() {
        // A database created before the baseline columns existed must migrate
        // in place: conflicts and checkpoints survive, and only the missing
        // baseline columns are added.
        let db_path =
            std::env::temp_dir().join(format!("conflict_migration_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        // Build a pre-migration database by hand (old conflicts schema).
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open raw conn");
            conn.execute_batch(
                r"
                CREATE TABLE file_states (
                    path TEXT PRIMARY KEY, hash TEXT NOT NULL, size INTEGER NOT NULL,
                    modified_at TEXT NOT NULL, couch_rev TEXT, last_sync_at TEXT NOT NULL
                );
                CREATE TABLE conflicts (
                    path TEXT PRIMARY KEY,
                    local_hash TEXT NOT NULL,
                    local_size INTEGER NOT NULL,
                    local_modified_at TEXT NOT NULL,
                    remote_hash TEXT NOT NULL,
                    remote_size INTEGER NOT NULL,
                    remote_modified_at TEXT NOT NULL,
                    remote_couch_rev TEXT NOT NULL,
                    detected_at TEXT NOT NULL,
                    notified BOOLEAN DEFAULT FALSE
                );
                CREATE TABLE sync_checkpoint (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    last_seq TEXT,
                    last_sync_at TEXT NOT NULL
                );
                ",
            )
            .expect("create old schema");
            conn.execute(
                "INSERT INTO conflicts
                 (path, local_hash, local_size, local_modified_at,
                  remote_hash, remote_size, remote_modified_at, remote_couch_rev,
                  detected_at, notified)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    "/old.txt",
                    "oldlocal",
                    10_i64,
                    "2024-01-01T00:00:00Z",
                    "oldremote",
                    20_i64,
                    "2024-01-02T00:00:00Z",
                    "1-abc",
                    "2024-01-03T00:00:00Z",
                    false,
                ],
            )
            .expect("insert old conflict");
            conn.execute(
                "INSERT INTO sync_checkpoint (id, last_seq, last_sync_at)
                 VALUES (1, 'seq-1', '2024-01-01T00:00:00Z')",
                [],
            )
            .expect("insert checkpoint");
        }

        let db = LocalDb::open(&db_path).expect("open migrated db");

        let conflicts = db.get_conflicts().expect("get_conflicts");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "/old.txt");
        assert_eq!(conflicts[0].local_state.hash, "oldlocal");
        assert_eq!(conflicts[0].remote_state.hash, "oldremote");
        assert_eq!(conflicts[0].remote_state.couch_rev.as_str(), "1-abc");
        // Pre-migration rows carry no stored deletion flag (not deleted) and
        // their last_sync_at falls back to the stored detected_at — never a
        // fabricated current timestamp.
        assert!(!conflicts[0].remote_state.deleted);
        assert_eq!(
            conflicts[0].local_state.last_sync_at,
            conflicts[0].detected_at
        );

        let cp = db.get_checkpoint().expect("get_checkpoint");
        assert_eq!(cp.expect("checkpoint present").last_seq, "seq-1");

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_clear_conflicts() {
        let db = test_db();
        db.store_conflict(&make_conflict("/a.txt"))
            .expect("store a");
        db.store_conflict(&make_conflict("/b.txt"))
            .expect("store b");

        let cleared = db.clear_conflicts().expect("clear_conflicts");
        assert_eq!(cleared, 2);

        let conflicts = db.get_conflicts().expect("get_conflicts");
        assert!(conflicts.is_empty());
    }

    // ── checkpoint operations ────────────────────────────────────────────

    #[test]
    fn test_get_checkpoint_none() {
        let db = test_db();
        let cp = db.get_checkpoint().expect("get_checkpoint");
        assert!(cp.is_none());
    }

    #[test]
    fn test_save_and_get_checkpoint() {
        let db = test_db();
        db.save_checkpoint("1000-abc").expect("save_checkpoint");

        let cp = db
            .get_checkpoint()
            .expect("get_checkpoint")
            .expect("checkpoint should exist");
        assert_eq!(cp.last_seq, "1000-abc");
    }

    #[test]
    fn test_save_checkpoint_replace() {
        let db = test_db();
        db.save_checkpoint("old-seq").expect("first save");
        db.save_checkpoint("new-seq").expect("second save");

        let cp = db
            .get_checkpoint()
            .expect("get_checkpoint")
            .expect("checkpoint should exist");
        assert_eq!(cp.last_seq, "new-seq");
    }

    #[test]
    fn test_clear_checkpoint() {
        let db = test_db();
        db.save_checkpoint("some-seq").expect("save");

        let cleared = db.clear_checkpoint().expect("clear_checkpoint");
        assert_eq!(cleared, 1);

        let cp = db.get_checkpoint().expect("get_checkpoint");
        assert!(cp.is_none());
    }

    #[test]
    fn test_clear_checkpoint_noop_when_empty() {
        let db = test_db();
        let cleared = db.clear_checkpoint().expect("clear_checkpoint");
        assert_eq!(cleared, 0);
    }

    #[test]
    fn test_reset_sync_state() {
        let db = test_db();

        // Insert data across all tables
        db.save_file_state(&make_file_state("/a.txt"))
            .expect("save file state");
        db.store_conflict(&make_conflict("/a.txt"))
            .expect("store conflict");
        db.save_checkpoint("seq-1").expect("save checkpoint");

        // Reset
        db.reset_sync_state().expect("reset_sync_state");

        // Verify everything is gone
        assert_eq!(db.get_all_file_states().expect("file states"), []);
        assert!(db.get_conflicts().expect("conflicts").is_empty());
        assert!(db.get_checkpoint().expect("checkpoint").is_none());
    }

    #[test]
    fn test_commit_rebuild_state_replaces_baseline_atomically() {
        let db = test_db();

        // Prior baseline across all three tables.
        db.save_file_state(&make_file_state("/old.txt"))
            .expect("save old state");
        db.store_conflict(&make_conflict("/old.txt"))
            .expect("store conflict");
        db.save_checkpoint("old-seq").expect("checkpoint");

        // Commit a rebuilt baseline: stale states dropped, new ones installed.
        db.commit_rebuild_state(&[make_file_state("/new.txt")])
            .expect("commit rebuild state");

        // Old state gone, new state present, conflicts + checkpoint cleared.
        assert!(db.get_file_state("/old.txt").expect("old gone").is_none());
        assert!(
            db.get_file_state("/new.txt")
                .expect("new present")
                .is_some()
        );
        assert!(db.get_conflicts().expect("conflicts cleared").is_empty());
        assert!(db.get_checkpoint().expect("checkpoint cleared").is_none());
    }

    // ── edge cases ───────────────────────────────────────────────────────

    #[test]
    fn test_delete_file_state_nonexistent() {
        let db = test_db();
        let result = db.delete_file_state("/nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn test_delete_conflict_nonexistent() {
        let db = test_db();
        let result = db.delete_conflict("/nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn test_mark_conflict_notified_nonexistent() {
        let db = test_db();
        let result = db.mark_conflict_notified("/nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn test_conflict_delivery_state_is_per_channel_and_generation() {
        let db = test_db();

        // Nothing delivered yet.
        assert!(db.delivered_conflicts("telegram").unwrap().is_empty());
        assert!(db.delivered_conflicts("matrix").unwrap().is_empty());

        // Deliver on one channel; the other channel must remain independent.
        db.mark_conflict_delivered("telegram", "/a.txt", "2")
            .expect("mark telegram delivered");
        assert_eq!(
            db.delivered_conflicts("telegram").unwrap(),
            HashSet::from([("/a.txt".to_string(), "2".to_string())])
        );
        assert!(
            db.delivered_conflicts("matrix").unwrap().is_empty(),
            "one channel's delivery must not suppress another"
        );

        // A new generation at the same path is a distinct delivery.
        db.mark_conflict_delivered("telegram", "/a.txt", "3")
            .expect("mark new generation");
        assert_eq!(
            db.delivered_conflicts("telegram").unwrap(),
            HashSet::from([
                ("/a.txt".to_string(), "2".to_string()),
                ("/a.txt".to_string(), "3".to_string()),
            ])
        );

        // Re-delivering the same (channel, path, generation) is idempotent.
        db.mark_conflict_delivered("telegram", "/a.txt", "2")
            .expect("re-deliver same generation");
        assert_eq!(
            db.delivered_conflicts("telegram").unwrap().len(),
            2,
            "re-delivery must not create a duplicate row"
        );
    }

    // ── sync identity ────────────────────────────────────────────────────

    fn identity(root: &str, db_name: &str, uuid: &str, prefix: &str) -> SyncIdentity {
        SyncIdentity::new(root, db_name, uuid, prefix)
    }

    #[test]
    fn legacy_state_is_conservatively_bound_and_kept() {
        let db = test_db();
        // Pre-identity database carries real file state.
        db.save_file_state(&make_file_state("a.txt")).unwrap();

        // First validation with no stored identity binds and keeps the state.
        let current = identity("/root", "db", "uuid-1", "old/");
        assert_eq!(
            db.validate_sync_identity(&current).unwrap(),
            SyncIdentityCheck::Bound
        );
        assert_eq!(db.get_sync_identity().unwrap(), Some(current));
        assert_eq!(db.get_all_file_states().unwrap().len(), 1);
    }

    #[test]
    fn unchanged_mapping_is_matched() {
        let db = test_db();
        let current = identity("/root", "db", "uuid-1", "notes/");
        assert_eq!(
            db.validate_sync_identity(&current).unwrap(),
            SyncIdentityCheck::Bound
        );
        // Same mapping again is a match.
        assert_eq!(
            db.validate_sync_identity(&current).unwrap(),
            SyncIdentityCheck::Matched
        );
    }

    #[test]
    fn changed_prefix_is_a_mismatch() {
        let db = test_db();
        db.validate_sync_identity(&identity("/root", "db", "uuid-1", "old/"))
            .unwrap();

        let new_prefix = identity("/root", "db", "uuid-1", "new/");
        match db.validate_sync_identity(&new_prefix).unwrap() {
            SyncIdentityCheck::Mismatched { stored, current } => {
                assert_eq!(stored.prefix, "old/");
                assert_eq!(current.prefix, "new/");
            }
            other => panic!("expected Mismatched, got {other:?}"),
        }
    }

    #[test]
    fn changed_database_is_a_mismatch() {
        let db = test_db();
        db.validate_sync_identity(&identity("/root", "db-a", "uuid-1", "notes/"))
            .unwrap();
        assert!(matches!(
            db.validate_sync_identity(&identity("/root", "db-b", "uuid-1", "notes/"))
                .unwrap(),
            SyncIdentityCheck::Mismatched { .. }
        ));
    }

    #[test]
    fn recreated_remote_database_is_a_mismatch() {
        let db = test_db();
        // Same db NAME, but a fresh UUID signals the database was recreated.
        db.validate_sync_identity(&identity("/root", "db", "uuid-1", "notes/"))
            .unwrap();
        assert!(matches!(
            db.validate_sync_identity(&identity("/root", "db", "uuid-2", "notes/"))
                .unwrap(),
            SyncIdentityCheck::Mismatched { .. }
        ));
    }

    #[test]
    fn canonical_root_alias_binds_to_the_same_identity() {
        let db = test_db();
        // Two spellings that resolve to the same canonical root produce the
        // same identity string, so the second is a match, not a mismatch.
        let canonical = identity("/srv/root", "db", "uuid-1", "notes/");
        db.validate_sync_identity(&canonical).unwrap();
        assert_eq!(
            db.validate_sync_identity(&canonical).unwrap(),
            SyncIdentityCheck::Matched
        );
    }

    // ── read-only open (dry-run baseline) ────────────────────────────────

    #[test]
    fn open_read_only_opens_existing_database_without_creating_schema() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("state.db");
        // A real writable DB with full schema and a recorded state.
        {
            let writable = LocalDb::open(&db_path).expect("open writable db");
            writable
                .save_file_state(&make_file_state("/a.txt"))
                .expect("save state");
        }

        // Read-only open must succeed against the existing file and still
        // expose the recorded state.
        let ro = LocalDb::open_read_only(&db_path).expect("open read-only db");
        assert!(
            ro.get_file_state("/a.txt").expect("read state").is_some(),
            "read-only baseline must still expose existing state"
        );
    }

    #[test]
    fn open_read_only_does_not_create_schema_on_pristine_file() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let db_path = tmp.path().join("state.db");
        // A pristine, table-less SQLite file (no schema init ever ran).
        {
            let conn = Connection::open(&db_path).expect("create empty sqlite file");
            drop(conn);
        }

        // Read-only open of the pristine file must succeed without creating
        // any schema (no table creation, no migration).
        let ro = LocalDb::open_read_only(&db_path).expect("open read-only db");
        let table_count: i64 = ro
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get(0),
            )
            .expect("count tables");
        assert_eq!(
            table_count, 0,
            "read-only open must not create/migrate schema"
        );
    }

    #[test]
    fn open_read_only_fails_on_absent_file() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let missing = tmp.path().join("does-not-exist.db");
        assert!(
            LocalDb::open_read_only(&missing).is_err(),
            "read-only open of an absent baseline must fail (caller uses in-memory)"
        );
    }
}
