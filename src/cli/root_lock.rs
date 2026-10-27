//! OS-backed exclusive ownership lock over one sync root's local state.
//!
//! A single root can be reached by several independent processes: a daemon, a
//! manual `sync`, a `rebuild-*`, or an interactive `resolve`. Without a
//! root-level guard they can open the same `SQLite` state file and mutate the
//! local tree and the remote concurrently. This module provides a small,
//! cross-platform file lock that serializes *mutating* processes per canonical
//! root while leaving unrelated roots — and read-only commands (`status`,
//! `conflicts`, `sync --dry-run`) — independent.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;

/// How long a mutating command waits for another process to release the root
/// lock before failing with a clear error (the "bounded" behavior).
pub const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting for the root lock.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// An exclusive OS file lock guarding one sync root's state and filesystem.
///
/// The lock file lives inside the root's state directory, so every mutating
/// process that canonicalizes to the same root contends on the same file
/// regardless of path spelling (`./dir`, `dir`, or a symlink alias). The lock
/// is released when the guard is dropped or when the owning process exits or
/// is killed, so ownership never dangles after a crash.
#[derive(Debug)]
pub struct RootLock {
    file: File,
}

impl RootLock {
    /// Acquire an exclusive lock on `path`, waiting up to `timeout` for a
    /// competing process to release it.
    ///
    /// `owner` is only used for diagnostics in the failure message and for the
    /// best-effort owner marker written into the lock file after acquisition.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock file cannot be opened, or is still held by
    /// another process once `timeout` elapses.
    pub fn acquire(path: &Path, owner: &Path, timeout: Duration) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open root lock file {}", path.display()))?;

        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(true) => {
                    // Best-effort owner marker for diagnostics; never fatal.
                    let _ = file.set_len(0);
                    let _ = writeln!(file, "{}", owner.display());
                    let _ = file.sync_data();
                    return Ok(Self { file });
                }
                Ok(false) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "another sync process holds the root lock for {} (lock file {}); \
                             waited up to {timeout:?}",
                            owner.display(),
                            path.display()
                        );
                    }
                    std::thread::sleep(LOCK_POLL_INTERVAL);
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("cannot lock {}", path.display()));
                }
            }
        }
    }
}

impl Drop for RootLock {
    fn drop(&mut self) {
        // The OS also releases the lock when the process dies; this early
        // unlock only shortens the window on the normal (clean) exit path.
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::RootLock;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempdir().unwrap();
        let lock_path = dir.path().join("sync.lock");
        let owner = dir.path().join("root");

        let first = RootLock::acquire(&lock_path, &owner, Duration::from_millis(200)).unwrap();
        let err = RootLock::acquire(&lock_path, &owner, Duration::from_millis(200)).unwrap_err();
        assert!(
            err.to_string().contains("holds the root lock"),
            "second acquisition while held must fail with a clear message: {err}"
        );

        drop(first);
        // Once released, the same lock file is immediately acquirable.
        let _second = RootLock::acquire(&lock_path, &owner, Duration::from_millis(200)).unwrap();
    }
}
