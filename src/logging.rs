use crate::config::RotationConfig;
use chrono::{Local, NaiveDate};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

const ROTATION_INTERVAL: StdDuration = StdDuration::from_hours(24);

/// State of the underlying log file.
enum LogFileState {
    /// The log file is open and ready for writing.
    Open(File),
    /// The log file is closed (e.g. after a failed rotation).
    Closed,
}

pub struct AppLogWriter {
    path: PathBuf,
    rotation: RotationConfig,
    opened_on: NaiveDate,
    last_rotation_at: Instant,
    file: LogFileState,
}

impl AppLogWriter {
    /// Create a log writer for the given path and rotation policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the log file cannot be opened for appending.
    pub fn new(path: &Path, rotation: RotationConfig) -> io::Result<Self> {
        Self::new_for_date(path, rotation, Local::now().date_naive())
    }

    fn new_for_date(
        path: &Path,
        rotation: RotationConfig,
        current_date: NaiveDate,
    ) -> io::Result<Self> {
        // Expand a leading `~` to the user's home directory so a configured
        // path like `~/logs/app.log` writes there instead of creating a
        // literal `~` directory in the working directory.
        let path = expand_home(path).into_owned();
        let file = open_log_file(&path)?;
        Ok(Self {
            path,
            rotation,
            opened_on: current_date,
            last_rotation_at: Instant::now(),
            file: LogFileState::Open(file),
        })
    }

    fn rotate_if_needed(&mut self) -> io::Result<()> {
        if self.rotation != RotationConfig::DailyKeep
            && self.rotation != RotationConfig::DailyDelete
            || self.last_rotation_at.elapsed() < ROTATION_INTERVAL
        {
            return Ok(());
        }

        self.rotate(Local::now().date_naive())
    }

    fn rotate(&mut self, next_opened_on: NaiveDate) -> io::Result<()> {
        if let LogFileState::Open(file) = &mut self.file {
            file.flush()?;
        }
        self.file = LogFileState::Closed;

        let rotation_result = match self.rotation {
            RotationConfig::DailyDelete => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            },
            RotationConfig::DailyKeep => {
                let rotated_path = rotated_log_path(&self.path, self.opened_on);
                let remove_result = match fs::remove_file(&rotated_path) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(err) => Err(err),
                };
                remove_result.and_then(|()| fs::rename(&self.path, rotated_path))
            }
            RotationConfig::Never => Ok(()),
        };

        // Reopen the log file regardless of whether rotation succeeded. A
        // recoverable rotation failure must not strand the writer in the
        // Closed state and silently stop diagnostics: logging continues to the
        // unrotated file and the error is surfaced below.
        let reopen_result = open_log_file(&self.path).map(|file| {
            self.file = LogFileState::Open(file);
        });

        match (rotation_result, reopen_result) {
            (Ok(()), Ok(())) => {
                self.opened_on = next_opened_on;
                self.last_rotation_at = Instant::now();
                Ok(())
            }
            (Err(rotation_err), Ok(())) => {
                // Logging recovered; keep the original file unrotated and
                // report the failure visibly. Bound the retry to the rotation
                // interval so a persistent failure does not spin on every
                // write.
                self.last_rotation_at = Instant::now();
                eprintln!(
                    "couchdb-file-sync: log rotation failed ({}); continuing to write {}",
                    rotation_err,
                    self.path.display()
                );
                Ok(())
            }
            (Ok(()), Err(reopen_err)) => {
                self.last_rotation_at = Instant::now();
                Err(reopen_err)
            }
            (Err(rotation_err), Err(reopen_err)) => {
                self.last_rotation_at = Instant::now();
                Err(io::Error::new(
                    reopen_err.kind(),
                    format!(
                        "log rotation failed ({rotation_err}) and the log file could not be \
                         reopened ({reopen_err})"
                    ),
                ))
            }
        }
    }
}

impl Write for AppLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed()?;
        match &mut self.file {
            LogFileState::Open(file) => file.write(buf),
            LogFileState::Closed => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "log file is closed",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            LogFileState::Open(file) => file.flush(),
            LogFileState::Closed => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "log file is closed",
            )),
        }
    }
}

/// Expand a leading `~` component to the user's home directory.
///
/// Only a path that starts with `~` (exactly, or `~/…`) is rewritten; any
/// other path is borrowed unchanged. When `HOME` is unset, the path is left
/// as-is.
#[must_use]
fn expand_home(path: &Path) -> std::borrow::Cow<'_, Path> {
    let Some(s) = path.to_str() else {
        return std::borrow::Cow::Borrowed(path);
    };
    let Some(home) = std::env::var_os("HOME") else {
        return std::borrow::Cow::Borrowed(path);
    };
    if s == "~" {
        return std::borrow::Cow::Owned(PathBuf::from(home));
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return std::borrow::Cow::Owned(PathBuf::from(home).join(rest));
    }
    std::borrow::Cow::Borrowed(path)
}

fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    OpenOptions::new().create(true).append(true).open(path)
}

fn rotated_log_path(path: &Path, date: NaiveDate) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("log");
    let extension = path.extension().and_then(|ext| ext.to_str());
    let rotated_name = extension.map_or_else(
        || format!("{stem}-{}", date.format("%Y-%m-%d")),
        |ext| format!("{stem}-{}.{}", date.format("%Y-%m-%d"), ext),
    );

    match path.parent() {
        Some(parent) => parent.join(rotated_name),
        None => PathBuf::from(rotated_name),
    }
}

#[cfg(test)]
mod tests {
    use super::{AppLogWriter, rotated_log_path};
    use crate::config::RotationConfig;
    use chrono::{Duration, Local, NaiveDate};
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    #[test]
    fn delete_policy_removes_previous_day_log_on_rotation() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_path = tempdir.path().join("app.log");
        let today = Local::now().date_naive();

        let mut writer =
            AppLogWriter::new_for_date(&log_path, RotationConfig::DailyDelete, today).unwrap();
        writer.write_all(b"day one").unwrap();
        writer.rotate(today + Duration::days(1)).unwrap();
        writer.write_all(b"day two").unwrap();
        writer.flush().unwrap();

        assert_eq!(fs::read_to_string(&log_path).unwrap(), "day two");
        assert!(!tempdir.path().join("app-2026-03-17.log").exists());
    }

    #[test]
    fn keep_policy_renames_previous_day_log_on_rotation() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_path = tempdir.path().join("app.log");
        let today = Local::now().date_naive();

        let mut writer =
            AppLogWriter::new_for_date(&log_path, RotationConfig::DailyKeep, today).unwrap();
        writer.write_all(b"day one").unwrap();
        writer.rotate(today + Duration::days(1)).unwrap();
        writer.write_all(b"day two").unwrap();
        writer.flush().unwrap();

        assert_eq!(fs::read_to_string(&log_path).unwrap(), "day two");
        assert_eq!(
            fs::read_to_string(
                tempdir
                    .path()
                    .join(format!("app-{}.log", today.format("%Y-%m-%d")))
            )
            .unwrap(),
            "day one"
        );
    }

    #[test]
    fn rotated_log_name_preserves_extension() {
        let rotated = rotated_log_path(
            Path::new("/tmp/couchdb-file-sync.log"),
            NaiveDate::from_ymd_opt(2026, 3, 17).unwrap(),
        );

        assert_eq!(
            rotated,
            PathBuf::from("/tmp/couchdb-file-sync-2026-03-17.log")
        );
    }

    #[test]
    fn never_config_does_not_rotate() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_path = tempdir.path().join("app.log");
        let today = Local::now().date_naive();

        let mut writer =
            AppLogWriter::new_for_date(&log_path, RotationConfig::Never, today).unwrap();

        // Ensure we don't attempt rotation for Never
        writer.write_all(b"some content").unwrap();
        // rotate() is a no-op for Never, so calling it explicitly should not change anything
        writer.rotate(today + Duration::days(1)).unwrap();
        writer.write_all(b" more content").unwrap();
        writer.flush().unwrap();

        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            "some content more content"
        );
    }

    #[test]
    fn rotation_failure_recovers_and_continues_logging() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_path = tempdir.path().join("app.log");
        let today = Local::now().date_naive();
        let rotated_path = tempdir
            .path()
            .join(format!("app-{}.log", today.format("%Y-%m-%d")));

        let mut writer =
            AppLogWriter::new_for_date(&log_path, RotationConfig::DailyKeep, today).unwrap();
        writer.write_all(b"day one").unwrap();

        // Force a rotation failure by placing a non-empty directory at the
        // rotated target name so `remove_file`/`rename` both fail.
        fs::create_dir_all(rotated_path.join("blocker")).unwrap();

        // Rotation fails, but the writer recovers and logging continues to the
        // same unrotated file.
        writer.rotate(today + Duration::days(1)).unwrap();
        writer.write_all(b"-day two").unwrap();
        writer.flush().unwrap();

        assert_eq!(fs::read_to_string(&log_path).unwrap(), "day one-day two");
    }

    #[test]
    fn expands_tilde_and_preserves_spaces_in_log_path() {
        // Serialize HOME mutation against other logging tests.
        static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        let _guard = ENV_LOCK.lock();

        let tempdir = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tempdir.path());
        }

        // A path with both a tilde and spaces must expand ~ and keep the
        // spaces intact rather than creating a literal `~` directory.
        let log_path = PathBuf::from("~/My Logs/couchdb-file-sync.log");
        let writer_result =
            AppLogWriter::new_for_date(&log_path, RotationConfig::Never, Local::now().date_naive());
        let mut writer = writer_result.unwrap();

        let expanded = tempdir.path().join("My Logs").join("couchdb-file-sync.log");
        writer.write_all(b"hello").unwrap();
        writer.flush().unwrap();
        assert_eq!(fs::read_to_string(&expanded).unwrap(), "hello");
        assert!(!tempdir.path().join("~").exists());

        match old_home {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
