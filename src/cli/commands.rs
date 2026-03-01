use crate::config::{AppConfig, SyncPath};
use crate::couchdb::{ChangeFeedEntry, CouchDb};
use crate::local::{AsyncFileWatcher, LocalDb};
use crate::models::{Change, ChangeType, IgnoreMatcher, ResolutionStrategy};
use crate::sync::{SyncEngine, SyncReport};
use crate::telegram::TelegramNotifier;
use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Select};
use similar::{ChangeTag, TextDiff};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Initialize a new sync directory
pub async fn init(path: PathBuf, _db_url: Option<String>, _db_name: Option<String>) -> Result<()> {
    info!(
        "Initializing CouchDB File Sync directory: {}",
        path.display()
    );

    // Create directory if it doesn't exist
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }

    // Create .couchdb-file-sync subdirectory for state
    let state_dir = path.join(".couchdb-file-sync");
    std::fs::create_dir_all(&state_dir)?;

    // Create couchdb-file-sync.yaml.example in .couchdb-file-sync directory
    let config_example = state_dir.join("couchdb-file-sync.yaml.example");
    std::fs::write(
        &config_example,
        include_str!("../../couchdb-file-sync.yaml.example"),
    )?;

    // Create .sync-ignore if it doesn't exist
    let sync_ignore = path.join(".sync-ignore");
    if !sync_ignore.exists() {
        let default_ignore = r#"# CouchDB File Sync ignore patterns
# Add files/directories to ignore (gitignore-style syntax)

# CouchDB File Sync internal files
.couchdb-file-sync/
.couchfs/
couchdb-file-sync.yaml
couchfs.yaml

# Common build artifacts
*.log
*.tmp
*.swp
*~
.DS_Store
Thumbs.db

# Version control
.git/
.svn/
.hg/

# IDE
.idea/
.vscode/
*.iml

# Dependencies
node_modules/
target/
"#;
        std::fs::write(&sync_ignore, default_ignore)?;
    }

    // Create initial database
    let db_path = state_dir.join("state.db");
    let local_db = LocalDb::open(&db_path)?;
    drop(local_db);

    println!("✓ Initialized CouchDB File Sync in {}", path.display());
    println!("  State database: {}", db_path.display());
    println!("  Config example: {}", config_example.display());
    println!("  Ignore file: {}", sync_ignore.display());
    println!();
    println!("Next steps:");
    println!("  1. Copy .couchdb-file-sync/couchdb-file-sync.yaml.example to .couchdb-file-sync/couchdb-file-sync.yaml");
    println!("  2. Edit .couchdb-file-sync/couchdb-file-sync.yaml with your CouchDB credentials");
    println!("  3. Run: couchdb-file-sync sync {}", path.display());

    Ok(())
}

/// Run a one-time sync
pub async fn sync(path: PathBuf, config: AppConfig, dry_run: bool) -> Result<SyncReport> {
    info!("Running sync in: {}", path.display());

    // Load ignore patterns
    let _ignore_matcher = load_ignore_patterns(&path);

    // Open local database
    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;

    // Connect to CouchDB
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    if dry_run {
        println!("Dry run mode - no changes will be made");
        // TODO: Implement dry-run logic
        return Ok(SyncReport::default());
    }

    // Run sync
    let mut engine = SyncEngine::new(couchdb, local_db, path.clone());
    let report = engine.sync().await?;

    print_sync_report(&report);

    // Send Telegram notifications for conflicts (one-time sync uses DB tracking)
    if report.conflicts > 0 {
        notify_conflicts_telegram(&config, &db_path, &path, None).await;
    }

    Ok(report)
}

/// Send Telegram notifications for any unnotified conflicts
/// If session_notified is provided, only notify about conflicts not in that set (daemon mode)
/// If session_notified is None, notify about all conflicts not marked as notified in DB (one-time sync)
async fn notify_conflicts_telegram(
    config: &AppConfig,
    db_path: &Path,
    sync_dir: &Path,
    mut session_notified: Option<&mut HashSet<String>>,
) {
    // Check if Telegram is configured
    let (bot_token, chat_id) = match (
        &config.notifications.telegram.bot_token,
        &config.notifications.telegram.chat_id,
    ) {
        (Some(token), Some(id)) if !token.is_empty() && !id.is_empty() => {
            (token.clone(), id.clone())
        }
        _ => {
            info!("Telegram not configured, skipping conflict notifications");
            return;
        }
    };

    // Re-open database to get conflicts
    let local_db = match LocalDb::open(db_path) {
        Ok(db) => db,
        Err(e) => {
            warn!("Failed to open database for notifications: {}", e);
            return;
        }
    };

    let notifier = TelegramNotifier::new(bot_token, chat_id);
    let sync_dir_str = sync_dir.display().to_string();

    // Get all conflicts
    let conflicts = match local_db.get_conflicts() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to get conflicts for notification: {}", e);
            return;
        }
    };

    // Filter to only new conflicts based on mode
    let new_conflicts: Vec<_> = match &session_notified {
        Some(notified) => {
            // Daemon mode: only conflicts not notified this session
            conflicts
                .iter()
                .filter(|c| !notified.contains(&c.path))
                .collect()
        }
        None => {
            // One-time sync: only conflicts not marked notified in DB
            conflicts.iter().filter(|c| !c.notified).collect()
        }
    };

    if new_conflicts.is_empty() {
        return;
    }

    // Send notification for all new conflicts at once
    match notifier
        .notify_new_conflicts(&new_conflicts, &sync_dir_str)
        .await
    {
        Ok(_) => {
            info!(
                "Sent Telegram notification for {} new conflict(s)",
                new_conflicts.len()
            );
            // Mark conflicts as notified
            for conflict in &new_conflicts {
                if let Some(ref mut notified) = session_notified {
                    // Daemon mode: track in session
                    notified.insert(conflict.path.clone());
                } else {
                    // One-time sync: mark in DB
                    if let Err(e) = local_db.mark_conflict_notified(&conflict.path) {
                        warn!("Failed to mark conflict as notified: {}", e);
                    }
                }
            }
        }
        Err(e) => {
            warn!("Failed to send Telegram notification: {}", e);
        }
    }
}

/// Run continuous sync daemon
pub async fn daemon(
    paths: Vec<SyncPath>,
    config: AppConfig,
    interval: u64,
    live: bool,
) -> Result<()> {
    let path_list: Vec<_> = paths
        .iter()
        .map(|p| p.local.display().to_string())
        .collect();
    info!(
        "Starting CouchDB File Sync daemon for: {}",
        path_list.join(", ")
    );

    if live {
        println!("CouchDB File Sync daemon started (live mode)");
        println!("Syncing {} path(s): {}", paths.len(), path_list.join(", "));
        println!("Press Ctrl+C to stop");

        let mut handles = Vec::new();
        for sync_path in paths {
            let mut path_config = config.clone();
            path_config.couchdb.remote_path = sync_path.remote.clone();
            let local_path = sync_path.local.clone();

            handles.push(tokio::spawn(async move {
                if let Err(e) = live_sync_path(local_path.clone(), path_config).await {
                    error!("Live sync error for {}: {}", local_path.display(), e);
                }
            }));
        }

        tokio::signal::ctrl_c().await?;
        for handle in handles {
            handle.abort();
        }
        return Ok(());
    }

    println!("CouchDB File Sync daemon started (interval: {}s)", interval);
    println!("Syncing {} path(s): {}", paths.len(), path_list.join(", "));
    println!("Press Ctrl+C to stop");

    // Track which conflicts have been notified during this daemon session
    let mut session_notified: HashSet<String> = HashSet::new();

    let mut interval_timer = tokio::time::interval(tokio::time::Duration::from_secs(interval));

    loop {
        interval_timer.tick().await;

        for sync_path in &paths {
            let mut path_config = config.clone();
            path_config.couchdb.remote_path = sync_path.remote.clone();

            match daemon_sync(&sync_path.local, &path_config, &mut session_notified).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Sync error for {}: {}", sync_path.local.display(), e);
                }
            }
        }
    }
}

/// Internal sync function for daemon that uses session-based notification tracking
async fn daemon_sync(
    path: &Path,
    config: &AppConfig,
    session_notified: &mut HashSet<String>,
) -> Result<SyncReport> {
    info!("Running sync in: {}", path.display());

    // Load ignore patterns
    let _ignore_matcher = load_ignore_patterns(path);

    // Open local database
    let db_path = state_db_path(path);
    let local_db = LocalDb::open(&db_path)?;

    // Connect to CouchDB
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    // Run sync
    let mut engine = SyncEngine::new(couchdb, local_db, path.to_path_buf());
    let report = engine.sync().await?;

    print_sync_report(&report);

    // Send Telegram notifications for NEW conflicts only (session-based tracking)
    if report.conflicts > 0 {
        notify_conflicts_telegram(config, &db_path, path, Some(session_notified)).await;
    }

    Ok(report)
}

struct TouchTracker {
    entries: Vec<(String, i64)>,
}

impl TouchTracker {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn mark(&mut self, path: &str, mtime: SystemTime) {
        let bucket = Self::bucket(mtime);
        self.entries.push((path.to_string(), bucket));
        if self.entries.len() > 50 {
            let drain = self.entries.len() - 50;
            self.entries.drain(0..drain);
        }
    }

    fn is_touched(&self, path: &str, mtime: SystemTime) -> bool {
        let bucket = Self::bucket(mtime);
        self.entries.iter().any(|(p, b)| p == path && *b == bucket)
    }

    fn bucket(mtime: SystemTime) -> i64 {
        let millis = mtime
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        millis / 5000
    }
}

async fn live_sync_path(path: PathBuf, config: AppConfig) -> Result<()> {
    info!("Starting live sync in: {}", path.display());

    let ignore_matcher = Arc::new(load_ignore_patterns(&path));

    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;

    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    let mut engine = SyncEngine::new(couchdb, local_db, path.clone());
    let initial_since = match engine.get_checkpoint()? {
        Some((seq, _)) => seq,
        None => {
            info!("No checkpoint found, starting changes feed from 'now'");
            "now".to_string()
        }
    };

    let (local_tx, mut local_rx) = mpsc::channel::<Change>(256);
    let (remote_tx, mut remote_rx) = mpsc::channel::<ChangeFeedEntry>(256);

    let watcher_root = path.clone();
    let watcher_ignore = ignore_matcher.clone();
    let debounce_ms = config.sync.debounce_ms;
    tokio::spawn(async move {
        if let Err(e) = run_local_watcher(watcher_root, watcher_ignore, debounce_ms, local_tx).await
        {
            error!("Local watcher error: {}", e);
        }
    });

    let remote_config = config.clone();
    tokio::spawn(async move {
        if let Err(e) = run_remote_changes(remote_config, initial_since, remote_tx).await {
            error!("Remote changes feed error: {}", e);
        }
    });

    let mut touched = TouchTracker::new();

    loop {
        tokio::select! {
            Some(change) = local_rx.recv() => {
                if let Err(e) = handle_local_change(&mut engine, &mut touched, &change).await {
                    warn!("Live local change error for {}: {}", change.path, e);
                }
            }
            Some(entry) = remote_rx.recv() => {
                if let Err(e) = handle_remote_change(&mut engine, &mut touched, &ignore_matcher, entry).await {
                    warn!("Live remote change error: {}", e);
                }
            }
        }
    }
}

async fn run_local_watcher(
    root: PathBuf,
    ignore_matcher: Arc<IgnoreMatcher>,
    debounce_ms: u64,
    tx: mpsc::Sender<Change>,
) -> Result<()> {
    let mut watcher =
        AsyncFileWatcher::start(root.clone(), (*ignore_matcher).clone(), debounce_ms)?;

    loop {
        if let Some(event) = watcher.next_event().await {
            if let Some(change) = watcher.event_to_change(event) {
                if tx.send(change).await.is_err() {
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn run_remote_changes(
    config: AppConfig,
    mut since: String,
    tx: mpsc::Sender<ChangeFeedEntry>,
) -> Result<()> {
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    loop {
        match couchdb.get_changes_feed(&since, 25_000).await {
            Ok((entries, last_seq)) => {
                since = last_seq;
                for entry in entries {
                    if tx.send(entry).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                error!("Changes feed error: {}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn handle_local_change(
    engine: &mut SyncEngine,
    touched: &mut TouchTracker,
    change: &Change,
) -> Result<()> {
    let mtime = local_mtime(engine.root_dir(), &change.path);
    if touched.is_touched(&change.path, mtime) {
        return Ok(());
    }

    match change.change_type {
        ChangeType::Created | ChangeType::Modified | ChangeType::Deleted => {
            if let Err(e) = engine.apply_local_change(change).await {
                warn!("Failed to apply local change {}: {}", change.path, e);
            }
        }
    }

    Ok(())
}

async fn handle_remote_change(
    engine: &mut SyncEngine,
    touched: &mut TouchTracker,
    ignore_matcher: &IgnoreMatcher,
    entry: ChangeFeedEntry,
) -> Result<()> {
    let change = entry.change;
    let seq = entry.seq;

    let local_path = engine.remote_to_local_path(&change.path);
    let local_rel = local_path.trim_start_matches('/').to_string();

    if ignore_matcher.should_ignore(Path::new(&local_rel)) {
        engine.save_checkpoint(&seq)?;
        return Ok(());
    }

    if let Some(state) = engine.get_file_state(&local_rel)? {
        if let (Some(remote_rev), Some(local_rev)) =
            (change.rev.as_deref(), state.couch_rev.as_deref())
        {
            if remote_rev == local_rev {
                engine.save_checkpoint(&seq)?;
                return Ok(());
            }
        }
    }

    match change.change_type {
        ChangeType::Deleted => {
            touched.mark(&local_rel, SystemTime::now());
            engine.apply_remote_change(&change).await?;
        }
        ChangeType::Created | ChangeType::Modified => {
            let file_path = engine.root_dir().join(&local_rel);
            let local_meta = std::fs::metadata(&file_path).ok();
            let local_exists = local_meta.is_some();
            let local_mtime = local_meta
                .and_then(|meta| meta.modified().ok())
                .unwrap_or_else(SystemTime::now);
            let remote_mtime = change
                .mtime
                .map(|dt| UNIX_EPOCH + Duration::from_millis(dt.timestamp_millis() as u64));

            let apply_remote = match remote_mtime {
                Some(remote) => {
                    if !local_exists {
                        true
                    } else {
                        let local_ms = local_mtime
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;
                        let remote_ms = remote
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;
                        let diff = local_ms - remote_ms;
                        let tolerance_ms = 1000;
                        if diff.abs() <= tolerance_ms {
                            false
                        } else {
                            diff < 0
                        }
                    }
                }
                None => true,
            };

            if apply_remote {
                touched.mark(&local_rel, SystemTime::now());
                engine.apply_remote_change(&change).await?;
            } else {
                let local_change = Change::local_modified(local_rel.clone(), String::new(), 0);
                engine.apply_local_change(&local_change).await?;
            }
        }
    }

    engine.save_checkpoint(&seq)?;
    Ok(())
}

fn local_mtime(root: &Path, relative_path: &str) -> SystemTime {
    let file_path = root.join(relative_path);
    std::fs::metadata(&file_path)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| SystemTime::now())
}

/// List conflicts
pub async fn conflicts(path: PathBuf, json: bool) -> Result<()> {
    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;

    let conflicts = local_db.get_conflicts()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&conflicts)?);
    } else if conflicts.is_empty() {
        println!("No conflicts found ✓");
    } else {
        println!("Conflicts ({}):", conflicts.len());
        for conflict in conflicts {
            println!("  • {}", conflict.path);
            println!(
                "    Local:  {} (modified: {})",
                &conflict.local_state.hash[..8.min(conflict.local_state.hash.len())],
                conflict.local_state.modified_at.format("%Y-%m-%d %H:%M")
            );
            println!(
                "    Remote: {} (modified: {})",
                &conflict.remote_state.hash[..8.min(conflict.remote_state.hash.len())],
                conflict.remote_state.modified_at.format("%Y-%m-%d %H:%M")
            );
            println!();
        }
        println!("Resolve with: couchdb-file-sync resolve");
    }

    Ok(())
}

/// Resolve conflicts interactively
pub async fn resolve(path: PathBuf, config: AppConfig) -> Result<()> {
    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;

    let conflicts = local_db.get_conflicts()?;

    if conflicts.is_empty() {
        println!("No conflicts to resolve.");
        return Ok(());
    }

    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    let mut engine = SyncEngine::new(couchdb, local_db, path.clone());

    println!("Found {} conflict(s) to resolve:\n", conflicts.len());

    for (i, conflict) in conflicts.iter().enumerate() {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Conflict {}/{}: {}", i + 1, conflicts.len(), conflict.path);
        println!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"
        );

        // Show file metadata
        println!(
            "Local:  {} bytes, modified {}",
            conflict.local_state.size,
            conflict.local_state.modified_at.format("%Y-%m-%d %H:%M:%S")
        );
        println!(
            "Remote: {} bytes, modified {}\n",
            conflict.remote_state.size,
            conflict
                .remote_state
                .modified_at
                .format("%Y-%m-%d %H:%M:%S")
        );

        // Read local content
        let local_file_path = path.join(&conflict.path);
        let local_content = match tokio::fs::read(&local_file_path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(e) => {
                println!("Could not read local file: {}", e);
                String::new()
            }
        };

        // Fetch remote content
        let remote_content = match engine.get_remote_content(&conflict.path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(e) => {
                println!("Could not fetch remote file: {}", e);
                String::new()
            }
        };

        // Display side-by-side diff
        print_side_by_side_diff(&local_content, &remote_content);

        // Ask user for action
        let options = &[
            "Keep Local",
            "Keep Remote",
            "Keep Both (merge manually)",
            "Skip",
        ];
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("How do you want to resolve this conflict?")
            .items(options)
            .default(0)
            .interact()?;

        let strategy = match selection {
            0 => ResolutionStrategy::KeepLocal,
            1 => ResolutionStrategy::KeepRemote,
            2 => ResolutionStrategy::KeepBoth,
            3 => ResolutionStrategy::Skip,
            _ => unreachable!(),
        };

        if strategy == ResolutionStrategy::Skip {
            println!("Skipped.\n");
            continue;
        }

        engine.resolve_conflict(&conflict.path, strategy).await?;

        match strategy {
            ResolutionStrategy::KeepLocal => {
                println!("Resolved: kept local version.\n");
            }
            ResolutionStrategy::KeepRemote => {
                println!("Resolved: kept remote version.\n");
            }
            ResolutionStrategy::KeepBoth => {
                println!(
                    "Resolved: saved remote as {}.remote - merge manually.\n",
                    conflict.path
                );
            }
            ResolutionStrategy::Skip => {}
        }
    }

    println!("All conflicts processed.");
    Ok(())
}

/// Print a side-by-side diff of two text contents
fn print_side_by_side_diff(local: &str, remote: &str) {
    let diff = TextDiff::from_lines(local, remote);
    let width = 38; // Width for each side

    println!("{:─<width$}┬{:─<width$}", "", "", width = width + 2);
    println!(" {:^width$} │ {:^width$}", "LOCAL", "REMOTE", width = width);
    println!("{:─<width$}┼{:─<width$}", "", "", width = width + 2);

    for change in diff.iter_all_changes() {
        let line = change.value().trim_end();
        match change.tag() {
            ChangeTag::Equal => {
                let truncated = truncate_str(line, width);
                println!(
                    " {:<width$} │ {:<width$}",
                    truncated,
                    truncated,
                    width = width
                );
            }
            ChangeTag::Delete => {
                // Line only in local (deleted from remote's perspective)
                let truncated = truncate_str(line, width);
                println!(
                    " \x1b[31m{:<width$}\x1b[0m │ {:<width$}",
                    truncated,
                    "",
                    width = width
                );
            }
            ChangeTag::Insert => {
                // Line only in remote (inserted from remote's perspective)
                let truncated = truncate_str(line, width);
                println!(
                    " {:<width$} │ \x1b[32m{:<width$}\x1b[0m",
                    "",
                    truncated,
                    width = width
                );
            }
        }
    }

    println!("{:─<width$}┴{:─<width$}\n", "", "", width = width + 2);
}

/// Truncate a string to fit within a given width
fn truncate_str(s: &str, max_width: usize) -> String {
    if s.len() <= max_width {
        s.to_string()
    } else {
        format!("{}...", &s[..max_width.saturating_sub(3)])
    }
}

/// Show sync status
pub async fn status(path: PathBuf, json: bool, _config: &AppConfig) -> Result<()> {
    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;

    let file_states = local_db.get_all_file_states()?;
    let conflicts = local_db.get_conflicts()?;
    let checkpoint = local_db.get_checkpoint()?;

    // Count files by walking directory
    let file_count = walkdir::WalkDir::new(&path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count();

    if json {
        let status = serde_json::json!({
            "sync_directory": path,
            "tracked_files": file_states.len(),
            "local_files": file_count,
            "pending_conflicts": conflicts.len(),
            "last_sync": checkpoint.map(|(_, ts)| ts),
        });
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("CouchDB File Sync Status");
        println!("==============");
        println!("Sync directory: {}", path.display());
        println!("Local files:    {}", file_count);
        println!("Tracked files:  {}", file_states.len());
        println!("Pending conflicts: {}", conflicts.len());

        if let Some((seq, ts)) = checkpoint {
            println!("Last sync:      {}", ts.format("%Y-%m-%d %H:%M:%S UTC"));
            println!("Last sequence:  {}", seq);
        } else {
            println!("Last sync:      Never");
        }

        if !conflicts.is_empty() {
            println!();
            println!("⚠️  {} conflict(s) need resolution", conflicts.len());
        }
    }

    Ok(())
}

/// Helper to load ignore patterns
fn load_ignore_patterns(root: &Path) -> IgnoreMatcher {
    let sync_ignore = root.join(".sync-ignore");
    if sync_ignore.exists() {
        match IgnoreMatcher::from_file(&sync_ignore) {
            Ok(matcher) => {
                info!("Loaded ignore patterns from .sync-ignore");
                matcher
            }
            Err(e) => {
                error!("Failed to load .sync-ignore: {}", e);
                IgnoreMatcher::empty()
            }
        }
    } else {
        IgnoreMatcher::empty()
    }
}

fn state_dir(root: &Path) -> PathBuf {
    let new_dir = root.join(".couchdb-file-sync");
    if new_dir.exists() {
        return new_dir;
    }

    let old_dir = root.join(".couchfs");
    if old_dir.exists() {
        return old_dir;
    }

    new_dir
}

fn state_db_path(root: &Path) -> PathBuf {
    state_dir(root).join("state.db")
}

/// Helper to print sync report
fn print_sync_report(report: &SyncReport) {
    println!();
    println!("Sync complete ✓");
    println!("  Uploaded:  {}", report.uploaded);
    println!("  Downloaded: {}", report.downloaded);
    println!("  Deleted (local): {}", report.deleted_local);
    println!("  Deleted (remote): {}", report.deleted_remote);

    if report.conflicts > 0 {
        println!("  Conflicts: {} ⚠️", report.conflicts);
        println!();
        println!("Run 'couchdb-file-sync conflicts' to see details");
    }

    if !report.errors.is_empty() {
        println!();
        println!("Errors:");
        for error in &report.errors {
            println!("  • {}", error);
        }
    }
}
