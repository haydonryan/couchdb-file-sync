use crate::models::{Change, IgnoreMatcher};
use anyhow::Result;
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent,
    notify::{EventKind, RecursiveMode}
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
    pub fn new(
        root_dir: PathBuf,
        ignore_matcher: IgnoreMatcher,
        debounce_ms: u64,
    ) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(100);
        let ignore_matcher = Arc::new(ignore_matcher);
        let root = root_dir.clone();

        let mut debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            None,
            move |result: DebounceEventResult| {
                match result {
                    Ok(events) => {
                        for event in events {
                            let _ = process_event(event, &event_tx, &ignore_matcher, &root);
                        }
                    }
                    Err(errors) => {
                        for error in errors {
                            error!("Watcher error: {:?}", error);
                        }
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
            ignore_matcher: Arc::new(IgnoreMatcher::empty()),
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
                Some(Change::local_deleted(relative.to_string_lossy().to_string()))
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
        path.strip_prefix(&self.root_dir).ok().map(|p| p.to_path_buf())
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
        EventKind::Modify(_) => {
            for path in &paths {
                if should_ignore(path, matcher, root) {
                    continue;
                }
                let event = WatcherEvent::FileModified(path.to_path_buf());
                let _ = tx.try_send(event);
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
}
