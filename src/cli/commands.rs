use crate::config::{default_user_config_file, AppConfig, SyncPath};
use crate::couchdb::{ChangeFeedEntry, CouchDb};
use crate::local::{AsyncFileWatcher, LocalDb};
use crate::models::{Change, ChangeType, IgnoreMatcher, ResolutionStrategy};
use crate::sync::{SyncEngine, SyncReport};
use crate::telegram::TelegramNotifier;
use anyhow::{Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use reqwest::StatusCode;
use similar::{ChangeTag, TextDiff};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const EMBEDDED_CONFIG_TEMPLATE: &str = include_str!("../../couchdb-file-sync.yaml.example");
const SYSTEMD_UNIT_NAME: &str = "couchdb-file-sync.service";

/// Initialize a new sync directory
pub async fn init(
    path: PathBuf,
    _db_url: Option<String>,
    _db_name: Option<String>,
    path_configured: bool,
) -> Result<()> {
    info!(
        "Initializing CouchDB File Sync directory: {}",
        path.display()
    );

    let new_db_path = path.join(".couchdb-file-sync").join("state.db");
    let old_db_path = path.join(".couchfs").join("state.db");
    if new_db_path.exists() || old_db_path.exists() {
        println!(
            "✓ Already initialized (state database exists) in {}",
            path.display()
        );
        return Ok(());
    }

    // Create directory if it doesn't exist
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }

    // Create .couchdb-file-sync subdirectory for state
    let state_dir = path.join(".couchdb-file-sync");
    std::fs::create_dir_all(&state_dir)?;

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
    println!("  Ignore file: {}", sync_ignore.display());
    println!();
    println!("Next steps:");
    if path_configured {
        println!("  1. Run: couchdb-file-sync sync {}", path.display());
    } else {
        println!("  1. Add this path to your config file (couchdb-file-sync.yaml) under `paths`");
        println!("  2. Run: couchdb-file-sync sync {}", path.display());
    }

    Ok(())
}

pub fn install_user_service() -> Result<()> {
    let paths = InstallPaths::detect()?;

    fs::create_dir_all(&paths.bin_dir)
        .with_context(|| format!("Failed to create {}", paths.bin_dir.display()))?;
    fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("Failed to create {}", paths.config_dir.display()))?;
    fs::create_dir_all(&paths.systemd_user_dir)
        .with_context(|| format!("Failed to create {}", paths.systemd_user_dir.display()))?;

    let current_exe = std::env::current_exe().context("Failed to locate the running executable")?;
    install_binary(&current_exe, &paths.binary_path)?;
    let config_status = ensure_config_file(&paths.config_file)?;
    write_service_file(&paths)?;
    reload_and_enable_service()?;

    println!("✓ Installed binary to {}", paths.binary_path.display());
    match config_status {
        ConfigStatus::Existing => {
            println!("✓ Using existing config: {}", paths.config_file.display());
        }
        ConfigStatus::CreatedTemplate => {
            println!("✓ Created config template: {}", paths.config_file.display());
        }
    }
    println!("✓ User service: {}", paths.service_file.display());
    println!("✓ Enabled and started {}", SYSTEMD_UNIT_NAME);

    Ok(())
}

pub fn uninstall_user_service() -> Result<()> {
    let paths = InstallPaths::detect()?;

    stop_and_disable_service()?;
    remove_if_exists(&paths.service_file)?;
    run_systemctl_user(&["daemon-reload"])?;
    remove_if_exists(&paths.binary_path)?;

    println!("✓ Removed user service {}", paths.service_file.display());
    println!("✓ Removed installed binary {}", paths.binary_path.display());
    println!("Config file left in place: {}", paths.config_file.display());

    Ok(())
}

fn install_binary(current_exe: &Path, target_binary: &Path) -> Result<()> {
    if current_exe == target_binary {
        return Ok(());
    }

    fs::copy(current_exe, target_binary).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            current_exe.display(),
            target_binary.display()
        )
    })?;
    set_mode(target_binary, 0o755)?;
    Ok(())
}

fn ensure_config_file(config_file: &Path) -> Result<ConfigStatus> {
    match fs::metadata(config_file) {
        Ok(_) => Ok(ConfigStatus::Existing),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let should_create = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "No config found at {}. Create a template there?",
                    config_file.display()
                ))
                .default(true)
                .interact()
                .context("Failed to read config creation prompt")?;

            if !should_create {
                anyhow::bail!(
                    "Installation aborted: no config found at {}",
                    config_file.display()
                );
            }

            fs::write(config_file, EMBEDDED_CONFIG_TEMPLATE)
                .with_context(|| format!("Failed to write {}", config_file.display()))?;
            set_mode(config_file, 0o644)?;
            Ok(ConfigStatus::CreatedTemplate)
        }
        Err(err) => {
            Err(err).with_context(|| format!("Failed to inspect {}", config_file.display()))
        }
    }
}

fn write_service_file(paths: &InstallPaths) -> Result<()> {
    let service = render_systemd_service(paths);
    fs::write(&paths.service_file, service)
        .with_context(|| format!("Failed to write {}", paths.service_file.display()))?;
    set_mode(&paths.service_file, 0o644)?;
    Ok(())
}

fn reload_and_enable_service() -> Result<()> {
    run_systemctl_user(&["daemon-reload"])?;
    run_systemctl_user(&["enable", "--now", SYSTEMD_UNIT_NAME])?;
    Ok(())
}

fn stop_and_disable_service() -> Result<()> {
    run_systemctl_user_ignore_missing(&["disable", "--now", SYSTEMD_UNIT_NAME])?;
    Ok(())
}

fn run_systemctl_user(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(["--user"])
        .args(args)
        .status()
        .with_context(|| format!("Failed to run systemctl --user {}", args.join(" ")))?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "systemctl --user {} exited with status {}",
            args.join(" "),
            status
        );
    }
}

fn run_systemctl_user_ignore_missing(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(["--user"])
        .args(args)
        .status()
        .with_context(|| format!("Failed to run systemctl --user {}", args.join(" ")))?;

    if status.success() {
        return Ok(());
    }

    let missing_status = Command::new("systemctl")
        .args(["--user", "status", SYSTEMD_UNIT_NAME])
        .status()
        .with_context(|| {
            format!(
                "Failed to run systemctl --user status {}",
                SYSTEMD_UNIT_NAME
            )
        })?;

    if !missing_status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "systemctl --user {} exited with status {}",
            args.join(" "),
            status
        );
    }
}

fn render_systemd_service(paths: &InstallPaths) -> String {
    format!(
        "[Unit]
Description=CouchDB File Sync daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={} --config {} daemon
Restart=always
RestartSec=5
Environment=PATH={}

[Install]
WantedBy=default.target
",
        shell_quote(&paths.binary_path),
        shell_quote(&paths.config_file),
        shell_quote_path_list(&[
            paths.bin_dir.clone(),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
    )
}

fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-:@".contains(&byte))
    {
        value.into_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn shell_quote_path_list(paths: &[PathBuf]) -> String {
    let joined = std::env::join_paths(paths.iter().map(PathBuf::as_path))
        .unwrap_or_else(|_| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    shell_quote(Path::new(&joined))
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions)?;
    }

    #[cfg(not(unix))]
    let _ = (path, mode);

    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
    }
}

#[derive(Debug, Clone)]
struct InstallPaths {
    bin_dir: PathBuf,
    binary_path: PathBuf,
    config_dir: PathBuf,
    config_file: PathBuf,
    systemd_user_dir: PathBuf,
    service_file: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum ConfigStatus {
    Existing,
    CreatedTemplate,
}

impl InstallPaths {
    fn detect() -> Result<Self> {
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let bin_dir = std::env::var_os("XDG_BIN_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home_dir.join(".local/bin"));
        let config_file = default_user_config_file()
            .context("Could not determine the standard config location")?;
        let config_dir = config_file
            .parent()
            .map(Path::to_path_buf)
            .context("Config file path has no parent directory")?;
        let systemd_config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home_dir.join(".config"));
        let systemd_user_dir = systemd_config_home.join("systemd/user");
        let binary_path = bin_dir.join("couchdb-file-sync");
        let service_file = systemd_user_dir.join(SYSTEMD_UNIT_NAME);

        Ok(Self {
            bin_dir,
            binary_path,
            config_dir,
            config_file,
            systemd_user_dir,
            service_file,
        })
    }
}

/// Run a one-time sync
pub async fn sync(path: PathBuf, config: AppConfig, dry_run: bool) -> Result<SyncReport> {
    info!("Running sync in: {}", path.display());

    // Load ignore patterns
    let ignore_matcher = load_ignore_patterns(&path);

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
    let mut engine = SyncEngine::with_ignore(couchdb, local_db, path.clone(), ignore_matcher);
    let report = engine.sync().await?;

    print_sync_report(&report);

    if !report.errors.is_empty() {
        let summary = format_sync_error_summary(&report.errors);
        notify_sync_error_telegram(&config, Some(&path), &summary).await;
    }

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

fn telegram_error_notifier(config: &AppConfig) -> Option<TelegramNotifier> {
    if !config.notifications.enabled || !config.notifications.notify_on_sync_error {
        return None;
    }

    let (bot_token, chat_id) = match (
        &config.notifications.telegram.bot_token,
        &config.notifications.telegram.chat_id,
    ) {
        (Some(token), Some(id)) if !token.is_empty() && !id.is_empty() => {
            (token.clone(), id.clone())
        }
        _ => {
            info!("Telegram not configured, skipping error notifications");
            return None;
        }
    };

    Some(TelegramNotifier::new(bot_token, chat_id))
}

fn format_sync_error_summary(errors: &[String]) -> String {
    let max_errors = 10;
    let mut lines = Vec::new();

    for error in errors.iter().take(max_errors) {
        lines.push(format!("• {}", error));
    }

    if errors.len() > max_errors {
        lines.push(format!("...and {} more", errors.len() - max_errors));
    }

    lines.join("\n")
}

async fn notify_sync_error_telegram(
    config: &AppConfig,
    sync_dir: Option<&Path>,
    error_message: &str,
) {
    let notifier = match telegram_error_notifier(config) {
        Some(notifier) => notifier,
        None => return,
    };

    let message = match sync_dir {
        Some(path) => format!("Sync path: {}\n{}", path.display(), error_message),
        None => error_message.to_string(),
    };

    if let Err(e) = notifier.notify_error(&message).await {
        warn!("Failed to send Telegram error notification: {}", e);
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
                if let Err(e) = live_sync_path(local_path.clone(), path_config.clone()).await {
                    error!("Live sync error for {}: {}", local_path.display(), e);
                    notify_sync_error_telegram(
                        &path_config,
                        Some(&local_path),
                        &format!("Live sync error: {}", e),
                    )
                    .await;
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
                    notify_sync_error_telegram(
                        &path_config,
                        Some(&sync_path.local),
                        &format!("Sync error: {}", e),
                    )
                    .await;
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
    let ignore_matcher = load_ignore_patterns(path);

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
    let mut engine = SyncEngine::with_ignore(couchdb, local_db, path.to_path_buf(), ignore_matcher);
    let report = engine.sync().await?;

    print_sync_report(&report);

    if !report.errors.is_empty() {
        let summary = format_sync_error_summary(&report.errors);
        notify_sync_error_telegram(config, Some(path), &summary).await;
    }

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

    let mut engine =
        SyncEngine::with_ignore(couchdb, local_db, path.clone(), (*ignore_matcher).clone());
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
    let watcher_config = config.clone();
    tokio::spawn(async move {
        if let Err(e) =
            run_local_watcher(watcher_root.clone(), watcher_ignore, debounce_ms, local_tx).await
        {
            error!("Local watcher error: {}", e);
            notify_sync_error_telegram(
                &watcher_config,
                Some(&watcher_root),
                &format!("Local watcher error: {}", e),
            )
            .await;
        }
    });

    let remote_config = config.clone();
    let remote_config_notify = config.clone();
    let remote_root = path.clone();
    tokio::spawn(async move {
        if let Err(e) = run_remote_changes(remote_config, initial_since, remote_tx).await {
            error!("Remote changes feed error: {}", e);
            notify_sync_error_telegram(
                &remote_config_notify,
                Some(&remote_root),
                &format!("Remote changes feed error: {}", e),
            )
            .await;
        }
    });

    let mut touched = TouchTracker::new();

    loop {
        tokio::select! {
            Some(change) = local_rx.recv() => {
                if let Err(e) = handle_local_change(&mut engine, &mut touched, &change).await {
                    warn!("Live local change error for {}: {}", change.path, e);
                    notify_sync_error_telegram(
                        &config,
                        Some(&path),
                        &format!("Live local change error for {}: {}", change.path, e),
                    )
                    .await;
                }
            }
            Some(entry) = remote_rx.recv() => {
                if let Err(e) = handle_remote_change(&mut engine, &mut touched, &ignore_matcher, entry).await {
                    warn!("Live remote change error: {}", e);
                    notify_sync_error_telegram(
                        &config,
                        Some(&path),
                        &format!("Live remote change error: {}", e),
                    )
                    .await;
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
                if let Some(status) = e
                    .downcast_ref::<reqwest::Error>()
                    .and_then(|err| err.status())
                {
                    if status == StatusCode::BAD_REQUEST {
                        warn!("Changes feed returned 400; resetting since to \"now\" and retrying");
                        since = "now".to_string();
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
                error!("Changes feed error: {}", e);
                notify_sync_error_telegram(&config, None, &format!("Changes feed error: {}", e))
                    .await;
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
    let char_count = s.chars().count();

    if char_count <= max_width {
        s.to_string()
    } else if max_width <= 3 {
        ".".repeat(max_width)
    } else {
        let truncated: String = s.chars().take(max_width - 3).collect();
        format!("{truncated}...")
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
