use crate::models::{Change, IgnoreMatcher};
use anyhow::Result;
use notify_debouncer_full::{
    new_debouncer,
    notify::{EventKind, RecursiveMode},
    DebounceEventResult, DebouncedEvent,
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
        let mut debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    for event in events {
                        let _ = process_event(event, &event_tx, &closure_matcher, &root);
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
                        RenameMode::Both if paths.len() >= 2 => {
                            // Both paths in one event - first is old, second is new
                            if !should_ignore(paths[0], matcher, root) {
                                let _ =
                                    tx.try_send(WatcherEvent::FileDeleted(paths[0].to_path_buf()));
                            }
                            if !should_ignore(paths[1], matcher, root) {
                                let _ =
                                    tx.try_send(WatcherEvent::FileCreated(paths[1].to_path_buf()));
                            }
                        }
                        RenameMode::Both => {}
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
    use notify_debouncer_full::notify::event::{
        CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
    };
    use notify_debouncer_full::notify::{Event, EventKind};
    use std::time::Instant;

    // -----------------------------------------------------------------------
    // Helper: construct a minimal FileWatcher for testing
    // -----------------------------------------------------------------------
    fn test_watcher(root: PathBuf) -> FileWatcher {
        let (_tx, event_rx) = tokio::sync::mpsc::channel(100);
        FileWatcher {
            root_dir: root,
            ignore_matcher: Arc::new(IgnoreMatcher::empty()),
            event_rx,
        }
    }

    fn debounced_event(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        DebouncedEvent {
            event: Event {
                kind,
                paths,
                attrs: Default::default(),
            },
            time: Instant::now(),
        }
    }

    // -----------------------------------------------------------------------
    // relative_path
    // -----------------------------------------------------------------------
    #[test]
    fn test_relative_path_basic() {
        let root = PathBuf::from("/home/user/sync");
        let w = test_watcher(root);

        let path = Path::new("/home/user/sync/docs/file.txt");
        assert_eq!(w.relative_path(path), Some(PathBuf::from("docs/file.txt")));
    }

    #[test]
    fn test_relative_path_root_itself() {
        let root = PathBuf::from("/home/user/sync");
        let w = test_watcher(root.clone());

        // strip_prefix on the root itself gives an empty path
        assert_eq!(w.relative_path(&root), Some(PathBuf::from("")));
    }

    #[test]
    fn test_relative_path_not_under_root() {
        let root = PathBuf::from("/home/user/sync");
        let w = test_watcher(root);

        let path = Path::new("/other/path.txt");
        assert_eq!(w.relative_path(path), None);
    }

    #[test]
    fn test_relative_path_deeply_nested() {
        let root = PathBuf::from("/a/b");
        let w = test_watcher(root);

        let path = Path::new("/a/b/c/d/e/f.txt");
        assert_eq!(w.relative_path(path), Some(PathBuf::from("c/d/e/f.txt")));
    }

    // -----------------------------------------------------------------------
    // should_ignore (free function)
    // -----------------------------------------------------------------------
    #[test]
    fn test_should_ignore_path_under_root_not_matched() {
        let matcher = IgnoreMatcher::from_content("*.log");
        let root = Path::new("/root");
        let path = Path::new("/root/docs/readme.md");
        assert!(!should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_path_matches_pattern() {
        let matcher = IgnoreMatcher::from_content("*.log");
        let root = Path::new("/root");
        let path = Path::new("/root/debug.log");
        assert!(should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_path_outside_root() {
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");
        let path = Path::new("/other/file.txt");
        assert!(should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_dotfile() {
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");
        let path = Path::new("/root/.hidden");
        assert!(should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_dotfile_nested() {
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");
        let path = Path::new("/root/folder/.hidden");
        assert!(should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_sync_ignore_file() {
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");
        let path = Path::new("/root/.sync-ignore");
        // .sync-ignore is always ignored by the model's should_ignore
        assert!(should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_couchfs_dir() {
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");
        let path = Path::new("/root/some/.couchfs/tmp");
        assert!(should_ignore(path, &matcher, root));
    }

    #[test]
    fn test_should_ignore_regular_file_visible() {
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");
        let path = Path::new("/root/work/file.txt");
        assert!(!should_ignore(path, &matcher, root));
    }

    // -----------------------------------------------------------------------
    // event_to_change
    // -----------------------------------------------------------------------
    #[test]
    fn test_event_to_change_created() {
        let root = PathBuf::from("/root");
        let w = test_watcher(root);

        let event = WatcherEvent::FileCreated(PathBuf::from("/root/new.txt"));
        let change = w.event_to_change(event).unwrap();
        assert_eq!(change.path, "new.txt");
        assert_eq!(change.change_type, ChangeType::Created);
        assert_eq!(change.source, ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_modified() {
        let root = PathBuf::from("/root");
        let w = test_watcher(root);

        let event = WatcherEvent::FileModified(PathBuf::from("/root/existing.txt"));
        let change = w.event_to_change(event).unwrap();
        assert_eq!(change.path, "existing.txt");
        assert_eq!(change.change_type, ChangeType::Modified);
        assert_eq!(change.source, ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_deleted() {
        let root = PathBuf::from("/root");
        let w = test_watcher(root);

        let event = WatcherEvent::FileDeleted(PathBuf::from("/root/gone.txt"));
        let change = w.event_to_change(event).unwrap();
        assert_eq!(change.path, "gone.txt");
        assert_eq!(change.change_type, ChangeType::Deleted);
        assert_eq!(change.source, ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_renamed() {
        let root = PathBuf::from("/root");
        let w = test_watcher(root);

        let event = WatcherEvent::FileRenamed(
            PathBuf::from("/root/old.txt"),
            PathBuf::from("/root/new.txt"),
        );
        let change = w.event_to_change(event).unwrap();
        // Renamed produces a local_created for the destination
        assert_eq!(change.path, "new.txt");
        assert_eq!(change.change_type, ChangeType::Created);
        assert_eq!(change.source, ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_path_outside_root_returns_none() {
        let root = PathBuf::from("/root");
        let w = test_watcher(root);

        let event = WatcherEvent::FileCreated(PathBuf::from("/outside/file.txt"));
        assert!(w.event_to_change(event).is_none());
    }

    // -----------------------------------------------------------------------
    // process_event – Create
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_create_sends_file_created() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Create(CreateKind::Any),
            vec![PathBuf::from("/root/new.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileCreated(path) => assert_eq!(path, PathBuf::from("/root/new.txt")),
            other => panic!("Expected FileCreated, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_create_ignored_path_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::from_content("*.tmp");
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Create(CreateKind::Any),
            vec![PathBuf::from("/root/file.tmp")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_process_event_create_path_outside_root_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Create(CreateKind::Any),
            vec![PathBuf::from("/outside/file.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // process_event – Modify (content change → FileModified)
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_modify_data_sends_file_modified() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            vec![PathBuf::from("/root/file.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileModified(path) => assert_eq!(path, PathBuf::from("/root/file.txt")),
            other => panic!("Expected FileModified, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_modify_any_sends_file_modified() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Any),
            vec![PathBuf::from("/root/file.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileModified(path) => assert_eq!(path, PathBuf::from("/root/file.txt")),
            other => panic!("Expected FileModified, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // process_event – Modify / Name (rename variants)
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_rename_from_sends_file_deleted() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![PathBuf::from("/root/old.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileDeleted(path) => assert_eq!(path, PathBuf::from("/root/old.txt")),
            other => panic!("Expected FileDeleted, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_rename_to_sends_file_created() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            vec![PathBuf::from("/root/new.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileCreated(path) => assert_eq!(path, PathBuf::from("/root/new.txt")),
            other => panic!("Expected FileCreated, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_rename_both_sends_delete_and_create() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![
                PathBuf::from("/root/old.txt"),
                PathBuf::from("/root/new.txt"),
            ],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        // Should emit FileDeleted for the first path and FileCreated for the second
        let mut deleted = false;
        let mut created = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                WatcherEvent::FileDeleted(p) => {
                    assert_eq!(p, PathBuf::from("/root/old.txt"));
                    deleted = true;
                }
                WatcherEvent::FileCreated(p) => {
                    assert_eq!(p, PathBuf::from("/root/new.txt"));
                    created = true;
                }
                other => panic!("Unexpected event: {other:?}"),
            }
        }
        assert!(deleted, "Expected FileDeleted for old path");
        assert!(created, "Expected FileCreated for new path");
    }

    #[test]
    fn test_process_event_rename_both_single_path_does_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        // RenameMode::Both with only one path – the code's match arm that
        // handles RenameMode::Both && paths.len() >= 2 is skipped.
        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![PathBuf::from("/root/only.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // process_event – Remove
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_remove_sends_file_deleted() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Remove(RemoveKind::Any),
            vec![PathBuf::from("/root/gone.txt")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileDeleted(path) => assert_eq!(path, PathBuf::from("/root/gone.txt")),
            other => panic!("Expected FileDeleted, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // process_event – ignored paths in rename variants
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_rename_from_ignored_path_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::from_content("*.old");
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![PathBuf::from("/root/file.old")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_process_event_remove_ignored_path_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let matcher = IgnoreMatcher::from_content("*.swp");
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Remove(RemoveKind::Any),
            vec![PathBuf::from("/root/file.swp")],
        );

        process_event(event, &tx, &matcher, root).unwrap();

        assert!(rx.try_recv().is_err());
    }
}
