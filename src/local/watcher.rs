use crate::models::{Change, IgnoreMatcher};
use anyhow::Result;
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, new_debouncer,
    notify::{EventKind, RecursiveMode},
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, trace};

/// File system watcher with debouncing
pub struct FileWatcher {
    root_dir: PathBuf,
    #[allow(dead_code)]
    ignore_matcher: Arc<IgnoreMatcher>,
    event_rx: mpsc::Receiver<WatcherEvent>,
}

/// Events emitted by the file watcher
#[derive(Debug, Clone)]
pub enum WatcherEvent {
    FileCreated(PathBuf),
    FileModified(PathBuf),
    FileDeleted(PathBuf),
    FileRenamed(PathBuf, PathBuf), // from, to
}

impl FileWatcher {
    /// Create a new file watcher
    pub fn new(root_dir: PathBuf, ignore_matcher: IgnoreMatcher, debounce_ms: u64) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(100);
        let ignore_matcher = Arc::new(ignore_matcher);
        let root = root_dir.clone();

        let closure_matcher = ignore_matcher.clone();
        let event_tx_clone = event_tx.clone();
        let mut debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    for event in events {
                        let _ = process_event(event, &event_tx_clone, &closure_matcher, &root);
                    }
                }
                Err(errors) => {
                    for error in errors {
                        error!("Watcher error: {:?}", error);
                    }
                }
            },
        )?;

        // Start watching
        debouncer.watch(&root_dir, RecursiveMode::Recursive)?;
        debug!("Started watching directory: {}", root_dir.display());

        // Keep the debouncer alive by moving it into a static
        // (In a real implementation, you'd want to store this in the struct)
        Box::leak(Box::new(debouncer));

        Ok(Self {
            root_dir,
            ignore_matcher,
            event_rx,
        })
    }

    /// Get the event receiver
    pub fn events(&mut self) -> &mut mpsc::Receiver<WatcherEvent> {
        &mut self.event_rx
    }

    /// Convert watcher events to changes
    pub fn event_to_change(&self, event: WatcherEvent) -> Option<Change> {
        match event {
            WatcherEvent::FileCreated(path) => {
                let relative = self.relative_path(&path)?;
                Some(Change::local_created(
                    relative.to_string_lossy().to_string(),
                    String::new(),
                    0,
                ))
            }
            WatcherEvent::FileModified(path) => {
                let relative = self.relative_path(&path)?;
                Some(Change::local_modified(
                    relative.to_string_lossy().to_string(),
                    String::new(),
                    0,
                ))
            }
            WatcherEvent::FileDeleted(path) => {
                let relative = self.relative_path(&path)?;
                Some(Change::local_deleted(
                    relative.to_string_lossy().to_string(),
                ))
            }
            WatcherEvent::FileRenamed(_from, to) => {
                let to_relative = self.relative_path(&to)?;
                Some(Change::local_created(
                    to_relative.to_string_lossy().to_string(),
                    String::new(),
                    0,
                ))
            }
        }
    }

    /// Get path relative to root
    fn relative_path(&self, path: &Path) -> Option<PathBuf> {
        path.strip_prefix(&self.root_dir)
            .ok()
            .map(|p| p.to_path_buf())
    }
}

/// Process a debounced event and send to channel
fn process_event(
    event: DebouncedEvent,
    tx: &mpsc::Sender<WatcherEvent>,
    matcher: &IgnoreMatcher,
    root: &Path,
) -> Result<()> {
    let paths: Vec<_> = event.paths.iter().collect();

    match event.kind {
        EventKind::Create(_) => {
            for path in &paths {
                if should_ignore(path, matcher, root) {
                    continue;
                }
                let event = WatcherEvent::FileCreated(path.to_path_buf());
                let _ = tx.try_send(event);
            }
        }
        EventKind::Modify(modify_kind) => {
            use notify_debouncer_full::notify::event::ModifyKind;

            match modify_kind {
                ModifyKind::Name(rename_mode) => {
                    use notify_debouncer_full::notify::event::RenameMode;
                    match rename_mode {
                        RenameMode::From => {
                            // File was renamed FROM this path (treat as delete)
                            for path in &paths {
                                if should_ignore(path, matcher, root) {
                                    continue;
                                }
                                let _ = tx.try_send(WatcherEvent::FileDeleted(path.to_path_buf()));
                            }
                        }
                        RenameMode::To => {
                            // File was renamed TO this path (treat as create)
                            for path in &paths {
                                if should_ignore(path, matcher, root) {
                                    continue;
                                }
                                let _ = tx.try_send(WatcherEvent::FileCreated(path.to_path_buf()));
                            }
                        }
                        RenameMode::Both => {
                            // Both paths in one event - first is old, second is new
                            if paths.len() >= 2 {
                                if !should_ignore(paths[0], matcher, root) {
                                    let _ = tx.try_send(WatcherEvent::FileDeleted(
                                        paths[0].to_path_buf(),
                                    ));
                                }
                                if !should_ignore(paths[1], matcher, root) {
                                    let _ = tx.try_send(WatcherEvent::FileCreated(
                                        paths[1].to_path_buf(),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    // All other modifications (content changes, metadata, etc.)
                    for path in &paths {
                        if should_ignore(path, matcher, root) {
                            continue;
                        }
                        let _ = tx.try_send(WatcherEvent::FileModified(path.to_path_buf()));
                    }
                }
            }
        }
        EventKind::Remove(_) => {
            for path in &paths {
                if should_ignore(path, matcher, root) {
                    continue;
                }
                let event = WatcherEvent::FileDeleted(path.to_path_buf());
                let _ = tx.try_send(event);
            }
        }
        _ => {}
    }

    Ok(())
}

/// Check if a path should be ignored
fn should_ignore(path: &Path, matcher: &IgnoreMatcher, root: &Path) -> bool {
    // Get relative path
    let relative = match path.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return true, // Ignore if not under root
    };

    // Check ignore patterns
    if matcher.should_ignore(relative) {
        trace!("Ignoring path: {}", path.display());
        return true;
    }

    false
}

/// Async file watcher that integrates with tokio
pub struct AsyncFileWatcher {
    inner: FileWatcher,
}

impl AsyncFileWatcher {
    /// Create and start watching
    pub fn start(
        root_dir: PathBuf,
        ignore_matcher: IgnoreMatcher,
        debounce_ms: u64,
    ) -> Result<Self> {
        let inner = FileWatcher::new(root_dir, ignore_matcher, debounce_ms)?;
        Ok(Self { inner })
    }

    /// Get next event
    pub async fn next_event(&mut self) -> Option<WatcherEvent> {
        self.inner.events().recv().await
    }

    /// Convert watcher events to changes
    pub fn event_to_change(&self, event: WatcherEvent) -> Option<Change> {
        self.inner.event_to_change(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChangeSource, ChangeType};

    // =========================================================================
    // Tests for should_ignore helper
    // =========================================================================

    #[test]
    fn should_ignore_path_outside_root() {
        let root = Path::new("/home/user/docs");
        let matcher = IgnoreMatcher::empty();
        let outside_path = Path::new("/etc/passwd");

        assert!(should_ignore(outside_path, &matcher, root));
    }

    #[test]
    fn should_ignore_respects_matcher() {
        let root = Path::new("/home/user/docs");
        let matcher = IgnoreMatcher::from_content("*.log");
        let ignored_path = Path::new("/home/user/docs/debug.log");

        assert!(should_ignore(ignored_path, &matcher, root));
    }

    #[test]
    fn should_not_ignore_normal_file() {
        let root = Path::new("/home/user/docs");
        let matcher = IgnoreMatcher::from_content("*.log");
        let normal_path = Path::new("/home/user/docs/readme.txt");

        assert!(!should_ignore(normal_path, &matcher, root));
    }

    // =========================================================================
    // Tests for relative_path
    // =========================================================================

    fn test_watcher(root: PathBuf) -> FileWatcher {
        // Create a minimal FileWatcher for testing using a test directory
        // We can't easily test FileWatcher::new() without actual file system setup,
        // so we'll test the methods directly by creating a mock scenario
        let (_event_tx, event_rx) = mpsc::channel(100);
        let ignore_matcher = Arc::new(IgnoreMatcher::empty());

        FileWatcher {
            root_dir: root,
            ignore_matcher,
            event_rx,
        }
    }

    #[test]
    fn relative_path_extracts_subpath() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let full = Path::new("/home/user/docs/notes.txt");

        let relative = watcher.relative_path(full);
        assert_eq!(relative, Some(PathBuf::from("notes.txt")));
    }

    #[test]
    fn relative_path_handles_nested_dirs() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let full = Path::new("/home/user/docs/subdir/nested.txt");

        let relative = watcher.relative_path(full);
        assert_eq!(relative, Some(PathBuf::from("subdir/nested.txt")));
    }

    #[test]
    fn relative_path_returns_none_for_outside() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let outside = Path::new("/etc/passwd");

        let relative = watcher.relative_path(outside);
        assert_eq!(relative, None);
    }

    // =========================================================================
    // Tests for event_to_change
    // =========================================================================

    #[test]
    fn event_to_change_created() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let event = WatcherEvent::FileCreated(PathBuf::from("/home/user/docs/new.txt"));

        let change = watcher.event_to_change(event);
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.path, "new.txt");
        assert!(matches!(change.change_type, ChangeType::Created));
        assert!(matches!(change.source, ChangeSource::Local));
    }

    #[test]
    fn event_to_change_modified() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let event = WatcherEvent::FileModified(PathBuf::from("/home/user/docs/existing.txt"));

        let change = watcher.event_to_change(event);
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.path, "existing.txt");
        assert!(matches!(change.change_type, ChangeType::Modified));
    }

    #[test]
    fn event_to_change_deleted() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let event = WatcherEvent::FileDeleted(PathBuf::from("/home/user/docs/old.txt"));

        let change = watcher.event_to_change(event);
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.path, "old.txt");
        assert!(matches!(change.change_type, ChangeType::Deleted));
    }

    #[test]
    fn event_to_change_renamed() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let event = WatcherEvent::FileRenamed(
            PathBuf::from("/home/user/docs/old.txt"),
            PathBuf::from("/home/user/docs/new.txt"),
        );

        let change = watcher.event_to_change(event);
        assert!(change.is_some());
        let change = change.unwrap();
        assert_eq!(change.path, "new.txt");
        assert!(matches!(change.change_type, ChangeType::Created));
    }

    #[test]
    fn event_to_change_returns_none_for_outside_root() {
        let watcher = test_watcher(PathBuf::from("/home/user/docs"));
        let event = WatcherEvent::FileCreated(PathBuf::from("/etc/passwd"));

        let change = watcher.event_to_change(event);
        assert!(change.is_none());
    }
}
