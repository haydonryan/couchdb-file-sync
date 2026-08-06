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
    pub fn new(path: PathBuf, rotation: RotationConfig) -> io::Result<Self> {
        Self::new_for_date(path, rotation, Local::now().date_naive())
    }

    fn new_for_date(
        path: PathBuf,
        rotation: RotationConfig,
        current_date: NaiveDate,
    ) -> io::Result<Self> {
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

        match self.rotation {
            RotationConfig::DailyDelete => match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            },
            RotationConfig::DailyKeep => {
                let rotated_path = rotated_log_path(&self.path, self.opened_on);
                match fs::remove_file(&rotated_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
                fs::rename(&self.path, rotated_path)?;
            }
            RotationConfig::Never => {}
        }

        self.file = LogFileState::Open(open_log_file(&self.path)?);
        self.opened_on = next_opened_on;
        self.last_rotation_at = Instant::now();
        Ok(())
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

fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
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
    use super::{rotated_log_path, AppLogWriter};
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
            AppLogWriter::new_for_date(log_path.clone(), RotationConfig::DailyDelete, today)
                .unwrap();
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
            AppLogWriter::new_for_date(log_path.clone(), RotationConfig::DailyKeep, today).unwrap();
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
            AppLogWriter::new_for_date(log_path.clone(), RotationConfig::Never, today).unwrap();

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
}
