use crate::models::{Change, ChangeType, Conflict, FileState};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use tracing::info;

/// Local SQLite database for state tracking
pub struct LocalDb {
    conn: Connection,
}

impl LocalDb {
    /// Open or create the local database
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init_schema()?;
        info!("Local database initialized");
        Ok(db)
    }

    /// Create an in-memory database (for testing)
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
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
                Ok(FileState {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                    size: row.get(2)?,
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
        self.conn.execute(
            "INSERT OR REPLACE INTO file_states 
             (path, hash, size, modified_at, couch_rev, last_sync_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                &state.path,
                &state.hash,
                state.size,
                state.modified_at.to_rfc3339(),
                &state.couch_rev,
                state.last_sync_at.to_rfc3339(),
            ],
        )?;
        Ok(())
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
                Ok(FileState {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                    size: row.get(2)?,
                    modified_at: row.get(3)?,
                    couch_rev: row.get(4)?,
                    last_sync_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(states)
    }

    // === Change Queue Operations ===

    /// Add change to queue
    pub fn queue_change(&self, change: &Change) -> Result<()> {
        self.conn.execute(
            "INSERT INTO change_queue (path, change_type, source, timestamp, hash, size)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                &change.path,
                format!("{:?}", change.change_type),
                format!("{:?}", change.source),
                change.timestamp.to_rfc3339(),
                change.hash.as_ref(),
                change.size,
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
                
                Ok(Change {
                    path: row.get(0)?,
                    change_type: parse_change_type(&change_type_str),
                    source: parse_change_source(&source_str),
                    timestamp: row.get(3)?,
                    hash: row.get(4)?,
                    size: row.get(5)?,
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
        let count = self.conn.execute(
            "DELETE FROM change_queue WHERE processed = TRUE",
            [],
        )?;
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
                conflict.notified,
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
                    path: path.clone(),
                    hash: row.get(4)?,
                    size: row.get::<_, i64>(5)? as u64,
                    modified_at: row.get(6)?,
                    couch_rev: row.get(7)?,
                    deleted: false,
                };
                
                let mut conflict = Conflict::new(path, local_state, remote_state);
                conflict.detected_at = row.get(8)?;
                conflict.notified = row.get(9)?;
                
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
                    path: path.clone(),
                    hash: row.get(4)?,
                    size: row.get::<_, i64>(5)? as u64,
                    modified_at: row.get(6)?,
                    couch_rev: row.get(7)?,
                    deleted: false,
                };
                
                let mut conflict = Conflict::new(path, local_state, remote_state);
                conflict.detected_at = row.get(8)?;
                conflict.notified = row.get(9)?;
                
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
    pub fn get_checkpoint(&self) -> Result<Option<(String, DateTime<Utc>)>> {
        let result = self
            .conn
            .query_row(
                "SELECT last_seq, last_sync_at FROM sync_checkpoint WHERE id = 1",
                [],
                |row| {
                    let seq: String = row.get(0)?;
                    let timestamp: DateTime<Utc> = row.get(1)?;
                    Ok((seq, timestamp))
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
