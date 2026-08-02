use crate::models::{Change, ChangeSource, ChangeType, Checkpoint, Conflict, CouchRev, FileState};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use tracing::info;

/// Local SQLite database for state tracking
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

    /// Create an in-memory database (for testing)
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
            r#"
            -- File state table
            CREATE TABLE IF NOT EXISTS file_states (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                modified_at TEXT NOT NULL,
                couch_rev TEXT,
                last_sync_at TEXT NOT NULL
            );

            -- Change queue for pending operations
            CREATE TABLE IF NOT EXISTS change_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL,
                change_type TEXT NOT NULL,
                source TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                hash TEXT,
                size INTEGER,
                processed BOOLEAN DEFAULT FALSE
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
                notified BOOLEAN DEFAULT FALSE
            );

            -- Sync checkpoint
            CREATE TABLE IF NOT EXISTS sync_checkpoint (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                last_seq TEXT,
                last_sync_at TEXT NOT NULL
            );

            -- Indexes
            CREATE INDEX IF NOT EXISTS idx_changes_path ON change_queue(path);
            CREATE INDEX IF NOT EXISTS idx_changes_processed ON change_queue(processed);
            CREATE INDEX IF NOT EXISTS idx_conflicts_notified ON conflicts(notified);
            "#,
        )?;
        Ok(())
    }

    // === File State Operations ===

    /// Get file state by path
    pub fn get_file_state(&self, path: &str) -> Result<Option<FileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, hash, size, modified_at, couch_rev, last_sync_at 
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
                })
            })
            .optional()?;

        Ok(state)
    }

    /// Save or update file state
    pub fn save_file_state(&self, state: &FileState) -> Result<()> {
        #[cfg(test)]
        self.save_file_state_calls
            .set(self.save_file_state_calls.get() + 1);

        // Check if this is a new file
        let is_new = self.get_file_state(&state.path)?.is_none();

        self.conn.execute(
            "INSERT OR REPLACE INTO file_states
             (path, hash, size, modified_at, couch_rev, last_sync_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                &state.path,
                &state.hash,
                u64_to_i64(state.size)?,
                state.modified_at.to_rfc3339(),
                &state.couch_rev,
                state.last_sync_at.to_rfc3339(),
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
    pub fn save_file_state_calls(&self) -> u64 {
        self.save_file_state_calls.get()
    }

    /// Delete file state
    pub fn delete_file_state(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM file_states WHERE path = ?", params![path])?;
        Ok(())
    }

    /// Get all file states
    pub fn get_all_file_states(&self) -> Result<Vec<FileState>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, hash, size, modified_at, couch_rev, last_sync_at 
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
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(states)
    }

    /// Clear all tracked file state
    pub fn clear_file_states(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM file_states", [])?;
        Ok(count)
    }

    // === Change Queue Operations ===

    /// Add change to queue
    pub fn queue_change(&self, change: &Change) -> Result<()> {
        self.conn.execute(
            "INSERT INTO change_queue (path, change_type, source, timestamp, hash, size)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                &change.path(),
                format!("{:?}", change.change_type()),
                format!("{:?}", change.source()),
                Utc::now().to_rfc3339(),
                change.hash().as_ref(),
                opt_u64_to_i64(change.size())?,
            ],
        )?;
        Ok(())
    }

    /// Get unprocessed changes
    pub fn get_pending_changes(&self) -> Result<Vec<Change>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, change_type, source, timestamp, hash, size
             FROM change_queue WHERE processed = FALSE ORDER BY timestamp",
        )?;

        let changes = stmt
            .query_map([], |row| {
                let change_type_str: String = row.get(1)?;
                let source_str: String = row.get(2)?;

                let size: Option<i64> = row.get(5)?;
                let path: String = row.get(0)?;
                let change_type = parse_change_type(&change_type_str);
                let source = parse_change_source(&source_str);
                let hash: Option<String> = row.get(4)?;
                let size: Option<u64> = size.map(i64_to_u64).transpose()?;

                Ok(match (change_type, source) {
                    (ChangeType::Created, ChangeSource::Local) => {
                        let hash = hash.unwrap_or_default();
                        let size = size.unwrap_or(0);
                        Change::local_created(path, hash, size)
                    }
                    (ChangeType::Modified, ChangeSource::Local) => {
                        let hash = hash.unwrap_or_default();
                        let size = size.unwrap_or(0);
                        Change::local_modified(path, hash, size)
                    }
                    (ChangeType::Deleted, ChangeSource::Local) => Change::local_deleted(path),
                    (ChangeType::Created, ChangeSource::Remote) => {
                        let hash = hash.unwrap_or_default();
                        let size = size.unwrap_or(0);
                        Change::remote_created(path, hash, size, Utc::now(), String::new())
                    }
                    (ChangeType::Modified, ChangeSource::Remote) => {
                        let hash = hash.unwrap_or_default();
                        let size = size.unwrap_or(0);
                        Change::remote_modified(path, hash, size, Utc::now(), String::new())
                    }
                    (ChangeType::Deleted, ChangeSource::Remote) => {
                        Change::remote_deleted(path, None)
                    }
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(changes)
    }

    /// Mark changes as processed
    pub fn mark_changes_processed(&self, paths: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        for path in paths {
            self.conn.execute(
                "UPDATE change_queue SET processed = TRUE WHERE path = ?",
                params![path],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Clear processed changes
    pub fn clear_processed_changes(&self) -> Result<usize> {
        let count = self
            .conn
            .execute("DELETE FROM change_queue WHERE processed = TRUE", [])?;
        Ok(count)
    }

    // === Conflict Operations ===

    /// Store conflict
    pub fn store_conflict(&self, conflict: &Conflict) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO conflicts 
             (path, local_hash, local_size, local_modified_at,
              remote_hash, remote_size, remote_modified_at, remote_couch_rev,
              detected_at, notified)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                &conflict.path,
                &conflict.local_state.hash,
                conflict.local_state.size as i64,
                conflict.local_state.modified_at.to_rfc3339(),
                &conflict.remote_state.hash,
                conflict.remote_state.size as i64,
                conflict.remote_state.modified_at.to_rfc3339(),
                &conflict.remote_state.couch_rev,
                conflict.detected_at.to_rfc3339(),
                conflict.is_notified(),
            ],
        )?;
        Ok(())
    }

    /// Get all conflicts
    pub fn get_conflicts(&self) -> Result<Vec<Conflict>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, local_hash, local_size, local_modified_at,
                    remote_hash, remote_size, remote_modified_at, remote_couch_rev,
                    detected_at, notified
             FROM conflicts ORDER BY detected_at DESC",
        )?;

        let conflicts = stmt
            .query_map([], |row| {
                use crate::models::RemoteState;

                let path: String = row.get(0)?;
                let local_state = FileState {
                    path: path.clone(),
                    hash: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    modified_at: row.get(3)?,
                    couch_rev: None,
                    last_sync_at: Utc::now(),
                };

                let remote_state = RemoteState {
                    hash: row.get(4)?,
                    size: row.get::<_, i64>(5)? as u64,
                    modified_at: row.get(6)?,
                    couch_rev: CouchRev::new(row.get::<_, String>(7)?.as_str()).unwrap_or_default(),
                    deleted: false,
                };

                let mut conflict = Conflict::new(path, local_state, remote_state);
                conflict.detected_at = row.get(8)?;
                let notified: bool = row.get(9)?;
                if notified {
                    conflict.notification_mode = crate::models::NotificationMode::Notified;
                }

                Ok(conflict)
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(conflicts)
    }

    /// Get conflict by path
    pub fn get_conflict(&self, path: &str) -> Result<Option<Conflict>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, local_hash, local_size, local_modified_at,
                    remote_hash, remote_size, remote_modified_at, remote_couch_rev,
                    detected_at, notified
             FROM conflicts WHERE path = ?",
        )?;

        let conflict = stmt
            .query_row(params![path], |row| {
                use crate::models::RemoteState;

                let path: String = row.get(0)?;
                let local_state = FileState {
                    path: path.clone(),
                    hash: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    modified_at: row.get(3)?,
                    couch_rev: None,
                    last_sync_at: Utc::now(),
                };

                let remote_state = RemoteState {
                    hash: row.get(4)?,
                    size: row.get::<_, i64>(5)? as u64,
                    modified_at: row.get(6)?,
                    couch_rev: CouchRev::new(row.get::<_, String>(7)?.as_str()).unwrap_or_default(),
                    deleted: false,
                };

                let mut conflict = Conflict::new(path, local_state, remote_state);
                conflict.detected_at = row.get(8)?;
                let notified: bool = row.get(9)?;
                if notified {
                    conflict.notification_mode = crate::models::NotificationMode::Notified;
                }

                Ok(conflict)
            })
            .optional()?;

        Ok(conflict)
    }

    /// Mark conflict as notified
    pub fn mark_conflict_notified(&self, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE conflicts SET notified = TRUE WHERE path = ?",
            params![path],
        )?;
        Ok(())
    }

    /// Delete conflict
    pub fn delete_conflict(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM conflicts WHERE path = ?", params![path])?;
        Ok(())
    }

    /// Clear all conflicts
    pub fn clear_conflicts(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM conflicts", [])?;
        Ok(count)
    }

    // === Sync Checkpoint Operations ===

    /// Get last sync checkpoint
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
    pub fn save_checkpoint(&self, seq: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_checkpoint (id, last_seq, last_sync_at)
             VALUES (1, ?, ?)",
            params![seq, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Clear the sync checkpoint
    pub fn clear_checkpoint(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM sync_checkpoint", [])?;
        Ok(count)
    }

    /// Remove tracked state that should not survive an authoritative rebuild.
    pub fn reset_sync_state(&self) -> Result<()> {
        self.clear_file_states()?;
        self.conn.execute("DELETE FROM change_queue", [])?;
        self.clear_conflicts()?;
        self.clear_checkpoint()?;
        Ok(())
    }
}

fn parse_change_type(s: &str) -> ChangeType {
    match s {
        "Created" => ChangeType::Created,
        "Modified" => ChangeType::Modified,
        "Deleted" => ChangeType::Deleted,
        _ => ChangeType::Modified,
    }
}

fn parse_change_source(s: &str) -> crate::models::ChangeSource {
    match s {
        "Local" => crate::models::ChangeSource::Local,
        "Remote" => crate::models::ChangeSource::Remote,
        _ => crate::models::ChangeSource::Local,
    }
}

fn u64_to_i64(value: u64) -> Result<i64> {
    Ok(i64::try_from(value)?)
}

fn opt_u64_to_i64(value: Option<u64>) -> Result<Option<i64>> {
    value.map(u64_to_i64).transpose()
}

fn i64_to_u64(value: i64) -> rusqlite::Result<u64> {
    if value < 0 {
        let err = std::io::Error::new(std::io::ErrorKind::InvalidData, "negative size");
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Integer,
            Box::new(err),
        ));
    }
    Ok(value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::models::{Change, ChangeSource, ChangeType, Conflict};
    use chrono::Utc;

    // ── helpers ──────────────────────────────────────────────────────────

    fn test_db() -> LocalDb {
        LocalDb::open_in_memory().expect("failed to create in-memory database")
    }

    fn make_file_state(path: &str) -> FileState {
        FileState::new(path.to_string(), "abc123".to_string(), 1024, Utc::now())
    }

    fn make_change(path: &str) -> Change {
        Change::local_created(path.to_string(), "def456".to_string(), 2048)
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
        assert!(states.is_empty());
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
        assert!(states.is_empty());
    }

    // ── change_queue operations ──────────────────────────────────────────

    #[test]
    fn test_queue_and_get_pending_changes() {
        let db = test_db();
        let change = make_change("/test/file.txt");
        db.queue_change(&change).expect("queue_change");

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path(), "/test/file.txt");
        assert_eq!(pending[0].change_type(), ChangeType::Created);
        assert_eq!(pending[0].source(), ChangeSource::Local);
    }

    #[test]
    fn test_get_pending_changes_empty() {
        let db = test_db();
        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_get_pending_changes_ordered_by_timestamp() {
        let db = test_db();

        let c1 = make_change("/first");
        let c2 = make_change("/second");
        let c3 = make_change("/third");

        db.queue_change(&c3).expect("queue c3");
        db.queue_change(&c1).expect("queue c1");
        db.queue_change(&c2).expect("queue c2");

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].path(), "/third");
        assert_eq!(pending[1].path(), "/first");
        assert_eq!(pending[2].path(), "/second");
    }

    #[test]
    fn test_mark_changes_processed() {
        let db = test_db();
        db.queue_change(&make_change("/a.txt")).expect("queue a");
        db.queue_change(&make_change("/b.txt")).expect("queue b");

        db.mark_changes_processed(&["/a.txt".to_string()])
            .expect("mark_changes_processed");

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path(), "/b.txt");
    }

    #[test]
    fn test_mark_changes_processed_all() {
        let db = test_db();
        db.queue_change(&make_change("/a.txt")).expect("queue a");
        db.queue_change(&make_change("/b.txt")).expect("queue b");

        db.mark_changes_processed(&["/a.txt".to_string(), "/b.txt".to_string()])
            .expect("mark_changes_processed");

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_clear_processed_changes() {
        let db = test_db();
        db.queue_change(&make_change("/a.txt")).expect("queue a");
        db.queue_change(&make_change("/b.txt")).expect("queue b");

        db.mark_changes_processed(&["/a.txt".to_string()])
            .expect("mark");
        let cleared = db
            .clear_processed_changes()
            .expect("clear_processed_changes");
        assert_eq!(cleared, 1);

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path(), "/b.txt");
    }

    #[test]
    fn test_clear_processed_changes_noop_when_none_processed() {
        let db = test_db();
        db.queue_change(&make_change("/a.txt")).expect("queue a");

        let cleared = db
            .clear_processed_changes()
            .expect("clear_processed_changes");
        assert_eq!(cleared, 0);

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert_eq!(pending.len(), 1);
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
        db.queue_change(&make_change("/a.txt"))
            .expect("queue change");
        db.store_conflict(&make_conflict("/a.txt"))
            .expect("store conflict");
        db.save_checkpoint("seq-1").expect("save checkpoint");

        // Reset
        db.reset_sync_state().expect("reset_sync_state");

        // Verify everything is gone
        assert!(db.get_all_file_states().expect("file states").is_empty());
        assert!(db
            .get_pending_changes()
            .expect("pending changes")
            .is_empty());
        assert!(db.get_conflicts().expect("conflicts").is_empty());
        assert!(db.get_checkpoint().expect("checkpoint").is_none());
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
    fn test_queue_change_with_all_change_types() {
        let db = test_db();
        let path = "/test/file.txt";
        let hash = Some("hash".to_string());
        let now = Utc::now();

        let created =
            Change::local_created(path.to_string(), hash.clone().unwrap_or_default(), 100u64);
        let modified = Change::remote_modified(
            path.to_string(),
            hash.clone().unwrap_or_default(),
            100u64,
            now,
            "1-rev".to_string(),
        );
        let deleted = Change::local_deleted(path.to_string());

        db.queue_change(&created).expect("queue created");
        db.queue_change(&modified).expect("queue modified");
        db.queue_change(&deleted).expect("queue deleted");

        let pending = db.get_pending_changes().expect("get_pending_changes");
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].change_type(), ChangeType::Created);
        assert_eq!(pending[1].change_type(), ChangeType::Modified);
        assert_eq!(pending[2].change_type(), ChangeType::Deleted);
        assert_eq!(pending[0].source(), ChangeSource::Local);
        assert_eq!(pending[1].source(), ChangeSource::Remote);
        assert_eq!(pending[2].source(), ChangeSource::Local);
    }
}
