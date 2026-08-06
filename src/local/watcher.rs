use crate::models::{Change, IgnoreMatcher, SyncDirPath};
use anyhow::Result;
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
    notify::{EventKind, RecommendedWatcher, RecursiveMode},
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// File system watcher with debouncing
pub struct FileWatcher {
    root_dir: SyncDirPath,
    /// The debouncer is stored to keep the watcher alive; dropped on `FileWatcher::drop`.
    _debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
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

/// How often the "dropped file-watcher event" warning may fire while the
/// internal 100-capacity event channel stays full. Repeated drops during a
/// burst are pulled into a single rate-limited warning instead of spamming
/// the log per dropped event.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(1);

/// A minimal cooldown guard: once a warning fires, further warnings are
/// suppressed until `interval` has elapsed since the last emitted warning.
struct RateLimitedWarn {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl RateLimitedWarn {
    const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    /// Returns `true` when a warning may be emitted now, claiming the cooldown
    /// so concurrent callers cannot all log at once.
    fn should_emit(&self) -> bool {
        // Recover the inner value if another thread panicked while holding the
        // lock (Mutex poisoning) instead of panicking on the live watcher hot path.
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if let Some(prev) = *last
            && now.duration_since(prev) < self.interval
        {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// Couples the debounced-event channel with the rate-limited drop warning so
/// every `try_send` failure in `process_event` routes through a single,
/// rate-limited warning point instead of being silently discarded.
struct EventSender {
    tx: mpsc::Sender<WatcherEvent>,
    drop_warn: RateLimitedWarn,
}

impl EventSender {
    const fn new(tx: mpsc::Sender<WatcherEvent>, warn_interval: Duration) -> Self {
        Self {
            tx,
            drop_warn: RateLimitedWarn::new(warn_interval),
        }
    }

    /// Try to enqueue `event`; on failure log a rate-limited warning rather
    /// than silently dropping the event (which would be lost to sync).
    fn send(&self, event: WatcherEvent) {
        if let Err(err) = self.tx.try_send(event)
            && self.drop_warn.should_emit()
        {
            warn!(
                error = %err,
                "dropped file-watcher event: internal event channel is full; event will not be synced"
            );
        }
    }
}

impl FileWatcher {
    /// Create a new file watcher
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying notify debouncer cannot be created.
    pub fn new(
        root_dir: SyncDirPath,
        ignore_matcher: IgnoreMatcher,
        debounce_ms: u64,
    ) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(100);
        let ignore_matcher = Arc::new(ignore_matcher);
        let closure_matcher = ignore_matcher;
        let root = root_dir.clone();
        let sender = EventSender::new(event_tx, DROP_WARN_INTERVAL);
        let mut debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    for event in events {
                        process_event(&event, &sender, &closure_matcher, &root);
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
        debouncer.watch(root_dir.as_path(), RecursiveMode::Recursive)?;
        debug!(
            "Started watching directory: {}",
            root_dir.as_path().display()
        );

        // Stored in the struct so Drop cleans up the watcher thread.

        Ok(Self {
            root_dir,
            _debouncer: Some(debouncer),
            event_rx,
        })
    }

    /// Get the event receiver
    pub const fn events(&mut self) -> &mut mpsc::Receiver<WatcherEvent> {
        &mut self.event_rx
    }

    /// Convert watcher events to changes
    #[must_use]
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
        path.strip_prefix(self.root_dir.as_path())
            .ok()
            .map(std::path::Path::to_path_buf)
    }
}

/// Process a debounced event and send to channel
fn process_event(
    event: &DebouncedEvent,
    sender: &EventSender,
    matcher: &IgnoreMatcher,
    root: &Path,
) {
    let paths: Vec<_> = event.paths.iter().collect();

    match event.kind {
        EventKind::Create(_) => {
            for path in &paths {
                if should_ignore(path, matcher, root) {
                    continue;
                }
                let event = WatcherEvent::FileCreated((*path).clone());
                sender.send(event);
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
                                sender.send(WatcherEvent::FileDeleted((*path).clone()));
                            }
                        }
                        RenameMode::To => {
                            // File was renamed TO this path (treat as create)
                            for path in &paths {
                                if should_ignore(path, matcher, root) {
                                    continue;
                                }
                                sender.send(WatcherEvent::FileCreated((*path).clone()));
                            }
                        }
                        RenameMode::Both if paths.len() >= 2 => {
                            // Both paths in one event - first is old, second is new
                            if !should_ignore(paths[0], matcher, root) {
                                sender.send(WatcherEvent::FileDeleted(paths[0].clone()));
                            }
                            if !should_ignore(paths[1], matcher, root) {
                                sender.send(WatcherEvent::FileCreated(paths[1].clone()));
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
                        sender.send(WatcherEvent::FileModified((*path).clone()));
                    }
                }
            }
        }
        EventKind::Remove(_) => {
            for path in &paths {
                if should_ignore(path, matcher, root) {
                    continue;
                }
                let event = WatcherEvent::FileDeleted((*path).clone());
                sender.send(event);
            }
        }
        _ => {}
    }
}

/// Check if a path should be ignored
fn should_ignore(path: &Path, matcher: &IgnoreMatcher, root: &Path) -> bool {
    // Get relative path
    let Ok(relative) = path.strip_prefix(root) else {
        return true; // Ignore if not under root
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
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying [`FileWatcher`] cannot be created.
    pub fn start(
        root_dir: SyncDirPath,
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
    #[must_use]
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
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::writer::MakeWriter;

    // -----------------------------------------------------------------------
    // Helper: construct a minimal FileWatcher for testing
    // -----------------------------------------------------------------------
    fn test_watcher(root: &Path) -> FileWatcher {
        let (_tx, event_rx) = tokio::sync::mpsc::channel(100);
        FileWatcher {
            root_dir: SyncDirPath::new(root).expect("valid test root"),
            _debouncer: None,
            event_rx,
        }
    }

    fn debounced_event(kind: EventKind, paths: Vec<PathBuf>) -> DebouncedEvent {
        DebouncedEvent {
            event: Event {
                kind,
                paths,
                attrs: notify_debouncer_full::notify::event::EventAttributes::default(),
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
        let w = test_watcher(&root);

        let path = Path::new("/home/user/sync/docs/file.txt");
        assert_eq!(w.relative_path(path), Some(PathBuf::from("docs/file.txt")));
    }

    #[test]
    fn test_relative_path_root_itself() {
        let root = PathBuf::from("/home/user/sync");
        let w = test_watcher(&root);

        // strip_prefix on the root itself gives an empty path
        assert_eq!(w.relative_path(&root), Some(PathBuf::from("")));
    }

    #[test]
    fn test_relative_path_not_under_root() {
        let root = PathBuf::from("/home/user/sync");
        let w = test_watcher(&root);

        let path = Path::new("/other/path.txt");
        assert_eq!(w.relative_path(path), None);
    }

    #[test]
    fn test_relative_path_deeply_nested() {
        let root = PathBuf::from("/a/b");
        let w = test_watcher(&root);

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
        let w = test_watcher(&root);

        let event = WatcherEvent::FileCreated(PathBuf::from("/root/new.txt"));
        let change = w.event_to_change(event).unwrap();
        assert_eq!(change.path(), "new.txt");
        assert_eq!(change.change_type(), ChangeType::Created);
        assert_eq!(change.source(), ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_modified() {
        let root = PathBuf::from("/root");
        let w = test_watcher(&root);

        let event = WatcherEvent::FileModified(PathBuf::from("/root/existing.txt"));
        let change = w.event_to_change(event).unwrap();
        assert_eq!(change.path(), "existing.txt");
        assert_eq!(change.change_type(), ChangeType::Modified);
        assert_eq!(change.source(), ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_deleted() {
        let root = PathBuf::from("/root");
        let w = test_watcher(&root);

        let event = WatcherEvent::FileDeleted(PathBuf::from("/root/gone.txt"));
        let change = w.event_to_change(event).unwrap();
        assert_eq!(change.path(), "gone.txt");
        assert_eq!(change.change_type(), ChangeType::Deleted);
        assert_eq!(change.source(), ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_renamed() {
        let root = PathBuf::from("/root");
        let w = test_watcher(&root);

        let event = WatcherEvent::FileRenamed(
            PathBuf::from("/root/old.txt"),
            PathBuf::from("/root/new.txt"),
        );
        let change = w.event_to_change(event).unwrap();
        // Renamed produces a local_created for the destination
        assert_eq!(change.path(), "new.txt");
        assert_eq!(change.change_type(), ChangeType::Created);
        assert_eq!(change.source(), ChangeSource::Local);
    }

    #[test]
    fn test_event_to_change_path_outside_root_returns_none() {
        let root = PathBuf::from("/root");
        let w = test_watcher(&root);

        let event = WatcherEvent::FileCreated(PathBuf::from("/outside/file.txt"));
        assert!(w.event_to_change(event).is_none());
    }

    // -----------------------------------------------------------------------
    // process_event – Create
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_create_sends_file_created() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Create(CreateKind::Any),
            vec![PathBuf::from("/root/new.txt")],
        );

        process_event(&event, &sender, &matcher, root);

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileCreated(path) => assert_eq!(path, PathBuf::from("/root/new.txt")),
            other => panic!("Expected FileCreated, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_create_ignored_path_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::from_content("*.tmp");
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Create(CreateKind::Any),
            vec![PathBuf::from("/root/file.tmp")],
        );

        process_event(&event, &sender, &matcher, root);

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_process_event_create_path_outside_root_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Create(CreateKind::Any),
            vec![PathBuf::from("/outside/file.txt")],
        );

        process_event(&event, &sender, &matcher, root);

        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // process_event – Modify (content change → FileModified)
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_modify_data_sends_file_modified() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            vec![PathBuf::from("/root/file.txt")],
        );

        process_event(&event, &sender, &matcher, root);

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileModified(path) => assert_eq!(path, PathBuf::from("/root/file.txt")),
            other => panic!("Expected FileModified, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_modify_any_sends_file_modified() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Any),
            vec![PathBuf::from("/root/file.txt")],
        );

        process_event(&event, &sender, &matcher, root);

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
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![PathBuf::from("/root/old.txt")],
        );

        process_event(&event, &sender, &matcher, root);

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileDeleted(path) => assert_eq!(path, PathBuf::from("/root/old.txt")),
            other => panic!("Expected FileDeleted, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_rename_to_sends_file_created() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            vec![PathBuf::from("/root/new.txt")],
        );

        process_event(&event, &sender, &matcher, root);

        let received = rx.try_recv().unwrap();
        match received {
            WatcherEvent::FileCreated(path) => assert_eq!(path, PathBuf::from("/root/new.txt")),
            other => panic!("Expected FileCreated, got {other:?}"),
        }
    }

    #[test]
    fn test_process_event_rename_both_sends_delete_and_create() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![
                PathBuf::from("/root/old.txt"),
                PathBuf::from("/root/new.txt"),
            ],
        );

        process_event(&event, &sender, &matcher, root);

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
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        // RenameMode::Both with only one path – the code's match arm that
        // handles RenameMode::Both && paths.len() >= 2 is skipped.
        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            vec![PathBuf::from("/root/only.txt")],
        );

        process_event(&event, &sender, &matcher, root);

        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // process_event – Remove
    // -----------------------------------------------------------------------
    #[test]
    fn test_process_event_remove_sends_file_deleted() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::empty();
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Remove(RemoveKind::Any),
            vec![PathBuf::from("/root/gone.txt")],
        );

        process_event(&event, &sender, &matcher, root);

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
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::from_content("*.old");
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            vec![PathBuf::from("/root/file.old")],
        );

        process_event(&event, &sender, &matcher, root);

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_process_event_remove_ignored_path_skipped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let sender = EventSender::new(tx, Duration::from_secs(1));
        let matcher = IgnoreMatcher::from_content("*.swp");
        let root = Path::new("/root");

        let event = debounced_event(
            EventKind::Remove(RemoveKind::Any),
            vec![PathBuf::from("/root/file.swp")],
        );

        process_event(&event, &sender, &matcher, root);

        assert!(rx.try_recv().is_err());
    }

    // -----------------------------------------------------------------------
    // Debouncer ownership (story #2663 regression guard)
    // -----------------------------------------------------------------------
    #[test]
    #[allow(clippy::used_underscore_binding)]
    fn file_watcher_owns_debouncer() {
        // FileWatcher must own the notify Debouncer (not Box::leak it) so the
        // watcher thread is cleaned up when FileWatcher is dropped. If the
        // debouncer were leaked again and the struct field removed, this test
        // would stop compiling; if a watcher were constructed without owning
        // the debouncer, this assertion would fail at runtime.
        let tmp = tempfile::tempdir().expect("temporary dir");
        let watcher = FileWatcher::new(
            SyncDirPath::new(tmp.path()).expect("valid test root"),
            IgnoreMatcher::empty(),
            100,
        )
        .expect("watcher starts");

        assert!(
            watcher._debouncer.is_some(),
            "FileWatcher must hold the notify debouncer so it is cleaned up on Drop"
        );
    }

    // -----------------------------------------------------------------------
    // Dropped-event warning on a full channel (story #2894)
    // -----------------------------------------------------------------------
    /// A writer that records formatted tracing output into a shared buffer so
    /// tests can assert on the emitted warning text.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl MakeWriter<'_> for SharedBuf {
        type Writer = SharedBufSink;

        fn make_writer(&self) -> Self::Writer {
            SharedBufSink(self.0.clone())
        }
    }

    struct SharedBufSink(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBufSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("shared buffer lock poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_send_after_full_channel_logs_warning() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(SharedBuf(buffer.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();

        with_default(subscriber, || {
            let (tx, _rx) = tokio::sync::mpsc::channel(100);
            let sender = EventSender::new(tx, Duration::ZERO);
            let path = PathBuf::from("/root/dropped.txt");

            // Fill the 100-capacity channel so the next send is dropped.
            for _ in 0..100 {
                sender.send(WatcherEvent::FileCreated(path.clone()));
            }
            sender.send(WatcherEvent::FileCreated(path));
        });

        let output = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .expect("tracing output is utf-8");
        assert!(
            output.contains("dropped file-watcher event"),
            "expected a drop warning to be logged, got: {output}"
        );
    }

    #[test]
    fn test_drop_warning_is_rate_limited() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(SharedBuf(buffer.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();

        with_default(subscriber, || {
            let (tx, _rx) = tokio::sync::mpsc::channel(100);
            let sender = EventSender::new(tx, Duration::from_mins(1));
            let path = PathBuf::from("/root/burst.txt");

            for _ in 0..100 {
                sender.send(WatcherEvent::FileCreated(path.clone()));
            }
            // A burst of dropped events must produce exactly one warning
            // within the rate-limit window.
            for _ in 0..10 {
                sender.send(WatcherEvent::FileCreated(path.clone()));
            }
        });

        let output = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .expect("tracing output is utf-8");
        let warn_count = output.matches("dropped file-watcher event").count();
        assert_eq!(
            warn_count, 1,
            "expected a single rate-limited warning, got {warn_count} in: {output}"
        );
    }
    // -----------------------------------------------------------------------
    // Rate-limit warning survives a poisoned lock (story #2933)
    // -----------------------------------------------------------------------
    #[test]
    fn test_drop_warning_survives_poisoned_lock() {
        let warn = RateLimitedWarn::new(Duration::from_secs(1));

        // Poison the rate-limit mutex by panicking while holding its guard, as
        // would happen if another thread panicked mid-warning on the watcher
        // hot path. catch_unwind prevents the panic from failing this test.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = warn
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            panic!("poison the rate-limit lock");
        }));

        // should_emit must not panic on the poisoned lock: the first call has
        // nothing to suppress and should claim the cooldown and emit.
        assert!(
            warn.should_emit(),
            "should_emit must return true (emit) after the lock is poisoned"
        );

        // A second call within the 1s cooldown window must still be suppressed,
        // proving the rate limiter keeps working after poisoning.
        assert!(
            !warn.should_emit(),
            "should_emit must suppress a second warning within the cooldown after poisoning"
        );
    }
}
