use crate::config::{AppConfig, SyncPath, default_user_config_file};
use crate::couchdb::{ChangeFeedEntry, CouchDb};
use crate::local::{AsyncFileWatcher, LocalDb, compute_file_hash};
use crate::matrix::MatrixNotifier;
use crate::models::{
    Change, ChangeType, Conflict, CouchRev, DatabaseName, ExpectedRemote, FileState, IgnoreMatcher,
    RemotePath, RemoteState, ResolutionStrategy, SyncDirPath,
};
use crate::sync::{SyncEngine, SyncReport, triage};
use crate::telegram::TelegramNotifier;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
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

/// Initialize a new sync directory.
///
/// # Errors
///
/// Returns an error if the embedded config template cannot be read or the
/// initial ignore file cannot be written.
pub fn init(
    path: &Path,
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
        std::fs::create_dir_all(path)?;
    }

    // Create .couchdb-file-sync subdirectory for state
    let state_dir = path.join(".couchdb-file-sync");
    std::fs::create_dir_all(&state_dir)?;

    // Create .sync-ignore if it doesn't exist
    let sync_ignore = path.join(".sync-ignore");
    if !sync_ignore.exists() {
        let default_ignore = r"# CouchDB File Sync ignore patterns
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
";
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

/// Install a user-scoped systemd service for this binary.
///
/// # Errors
///
/// Returns an error if the install paths cannot be detected, directories or
/// the binary cannot be written, the config file cannot be ensured, the
/// service file cannot be written, or `systemctl --user` operations fail.
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
    println!("✓ Enabled and started {SYSTEMD_UNIT_NAME}");

    Ok(())
}

/// Remove the user-scoped systemd service for this binary.
///
/// # Errors
///
/// Returns an error if the service cannot be stopped/disabled, files cannot
/// be removed, or `systemctl --user` operations fail.
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
        .with_context(|| format!("Failed to run systemctl --user status {SYSTEMD_UNIT_NAME}"))?;

    if missing_status.success() {
        anyhow::bail!(
            "systemctl --user {} exited with status {}",
            args.join(" "),
            status
        );
    }
    Ok(())
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

/// Resolve the sync root directory for a CLI command.
///
/// Canonicalizes the given path into a [`SyncDirPath`], returning a
/// contextual, user-facing error instead of panicking when the path is empty
/// or cannot be resolved (e.g. `sync ""`).
fn resolve_sync_dir(path: &Path) -> Result<SyncDirPath> {
    SyncDirPath::new(path)
        .with_context(|| format!("cannot resolve sync directory '{}'", path.display()))
}

/// Run a one-time sync.
///
/// When `dry_run` is true the full sync pipeline (triage, upload/download/
/// delete detection, and conflict identification) is walked through without
/// writing anything to `CouchDB`, the local filesystem, or the state DB, and a
/// summary of what would happen is printed instead.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened, the `CouchDB`
/// connection cannot be established, or the sync pipeline fails.
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
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    // Run sync (dry-run walks the pipeline without writing)
    let retention = (config.sync.tombstone_retention_secs > 0)
        .then(|| std::time::Duration::from_secs(config.sync.tombstone_retention_secs));
    let mut engine =
        SyncEngine::with_ignore(couchdb, local_db, resolve_sync_dir(&path)?, ignore_matcher)
            .with_tombstone_retention(retention);
    let report = if dry_run {
        engine.sync_dry_run().await?
    } else {
        engine.sync().await?
    };

    print_sync_report(&report, dry_run);

    // In dry-run mode no notifications are sent and no state is updated.
    if !dry_run {
        if !report.errors.is_empty() {
            let summary = format_sync_error_summary(&report.errors);
            notify_sync_error_telegram(&config, Some(&path), &summary).await;
            notify_sync_error_matrix(&config, Some(&path), &summary).await;
        }

        // Send Telegram notifications for conflicts (one-time sync uses DB tracking)
        if report.conflicts > 0 {
            notify_conflicts_telegram(&config, &db_path, &path, None).await;
            notify_conflicts_matrix(&config, &db_path, &path, None).await;
        }
    }

    Ok(report)
}

/// Rebuild the remote scope from the local filesystem.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened, the `CouchDB`
/// connection cannot be established, or the rebuild fails.
pub async fn rebuild_remote(path: PathBuf, config: AppConfig) -> Result<SyncReport> {
    info!("Rebuilding remote from local in: {}", path.display());

    let ignore_matcher = load_ignore_patterns(&path);
    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    let mut engine =
        SyncEngine::with_ignore(couchdb, local_db, resolve_sync_dir(&path)?, ignore_matcher);
    let report = engine.rebuild_remote_from_local().await?;
    print_sync_report(&report, false);

    Ok(report)
}

/// Rebuild the local filesystem from the remote scope.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened, the `CouchDB`
/// connection cannot be established, or the rebuild fails.
pub async fn rebuild_local(path: PathBuf, config: AppConfig) -> Result<SyncReport> {
    info!("Rebuilding local from remote in: {}", path.display());

    let ignore_matcher = load_ignore_patterns(&path);
    let db_path = state_db_path(&path);
    let local_db = LocalDb::open(&db_path)?;
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    let mut engine =
        SyncEngine::with_ignore(couchdb, local_db, resolve_sync_dir(&path)?, ignore_matcher);
    let report = engine.rebuild_local_from_remote().await?;
    print_sync_report(&report, false);

    Ok(report)
}

/// Send Telegram notifications for any unnotified conflicts
/// If `session_notified` is provided, only notify about conflicts not in that set (daemon mode)
/// If `session_notified` is None, notify about all conflicts not marked as notified in DB (one-time sync)
async fn notify_conflicts_telegram(
    config: &AppConfig,
    db_path: &Path,
    sync_dir: &Path,
    mut session_notified: Option<&mut HashSet<String>>,
) {
    // Check if Telegram is configured
    let (bot_token, chat_id) = match &config.notifications.telegram.credentials {
        Some(creds) if !creds.bot_token.is_empty() && !creds.chat_id.is_empty() => {
            (creds.bot_token.clone(), creds.chat_id.clone())
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
    let new_conflicts: Vec<_> = session_notified.as_ref().map_or_else(
        || {
            // One-time sync: only conflicts not marked notified in DB
            conflicts.iter().filter(|c| !c.is_notified()).collect()
        },
        |notified_paths| {
            // Daemon mode: only conflicts not notified this session
            conflicts
                .iter()
                .filter(|c| !notified_paths.contains(&c.path))
                .collect()
        },
    );

    if new_conflicts.is_empty() {
        return;
    }

    // Send notification for all new conflicts at once
    match notifier
        .notify_new_conflicts(&new_conflicts, &sync_dir_str)
        .await
    {
        Ok(()) => {
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
    if !config.notifications.notify_on_sync_error {
        return None;
    }

    let (bot_token, chat_id) = match &config.notifications.telegram.credentials {
        Some(creds) if !creds.bot_token.is_empty() && !creds.chat_id.is_empty() => {
            (creds.bot_token.clone(), creds.chat_id.clone())
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
        lines.push(format!("• {error}"));
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
    let Some(notifier) = telegram_error_notifier(config) else {
        return;
    };

    let message = sync_dir.map_or_else(
        || error_message.to_string(),
        |path| format!("Sync path: {}\n{}", path.display(), error_message),
    );

    if let Err(e) = notifier.notify_error(&message).await {
        warn!("Failed to send Telegram error notification: {}", e);
    }
}

/// Matrix notifier helper for error notifications
fn matrix_error_notifier(config: &AppConfig) -> Option<MatrixNotifier> {
    if !config.notifications.notify_on_sync_error {
        return None;
    }

    let (homeserver_url, access_token, room_id) = match &config.notifications.matrix.credentials {
        Some(c)
            if !c.homeserver_url.is_empty()
                && !c.access_token.is_empty()
                && !c.room_id.is_empty() =>
        {
            (
                c.homeserver_url.clone(),
                c.access_token.clone(),
                c.room_id.clone(),
            )
        }
        _ => {
            info!("Matrix not configured, skipping error notifications");
            return None;
        }
    };

    Some(MatrixNotifier::new(
        homeserver_url,
        access_token,
        room_id,
        config
            .notifications
            .matrix
            .message_type
            .as_str()
            .to_string(),
    ))
}

/// Send Matrix notifications for any unnotified conflicts
/// If `session_notified` is provided, only notify about conflicts not in that set (daemon mode)
/// If `session_notified` is None, notify about all conflicts not marked as notified in DB (one-time sync)
async fn notify_conflicts_matrix(
    config: &AppConfig,
    db_path: &Path,
    sync_dir: &Path,
    mut session_notified: Option<&mut HashSet<String>>,
) {
    // Check if Matrix is configured
    let (homeserver_url, access_token, room_id) = match &config.notifications.matrix.credentials {
        Some(c)
            if !c.homeserver_url.is_empty()
                && !c.access_token.is_empty()
                && !c.room_id.is_empty() =>
        {
            (
                c.homeserver_url.clone(),
                c.access_token.clone(),
                c.room_id.clone(),
            )
        }
        _ => {
            info!("Matrix not configured, skipping conflict notifications");
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

    let notifier = MatrixNotifier::new(
        homeserver_url,
        access_token,
        room_id,
        config
            .notifications
            .matrix
            .message_type
            .as_str()
            .to_string(),
    );
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
    let new_conflicts: Vec<_> = session_notified.as_ref().map_or_else(
        || {
            // One-time sync: only conflicts not marked notified in DB
            conflicts.iter().filter(|c| !c.is_notified()).collect()
        },
        |notified_paths| {
            // Daemon mode: only conflicts not notified this session
            conflicts
                .iter()
                .filter(|c| !notified_paths.contains(&c.path))
                .collect()
        },
    );

    if new_conflicts.is_empty() {
        return;
    }

    // Send notification for all new conflicts at once
    match notifier
        .notify_new_conflicts(&new_conflicts, &sync_dir_str)
        .await
    {
        Ok(()) => {
            info!(
                "Sent Matrix notification for {} new conflict(s)",
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
            warn!("Failed to send Matrix notification: {}", e);
        }
    }
}

async fn notify_sync_error_matrix(
    config: &AppConfig,
    sync_dir: Option<&Path>,
    error_message: &str,
) {
    let Some(notifier) = matrix_error_notifier(config) else {
        return;
    };

    let message = sync_dir.map_or_else(
        || error_message.to_string(),
        |path| {
            format!(
                "Sync path: {}
{}",
                path.display(),
                error_message
            )
        },
    );

    if let Err(e) = notifier.notify_error(&message).await {
        warn!("Failed to send Matrix error notification: {}", e);
    }
}

/// Run continuous sync daemon
/// Run sync continuously on the given paths, either on a fixed interval or in
/// live mode with a filesystem watcher and `CouchDB` changes feed.
///
/// # Errors
///
/// Returns an error if a sync cycle fails fatally or a notification cannot be
/// sent.
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
            path_config
                .couchdb
                .remote_path
                .clone_from(&sync_path.remote);
            let local_path = sync_path.local.clone();

            handles.push(tokio::spawn(async move {
                if let Err(e) = live_sync_path(local_path.clone(), path_config.clone()).await {
                    error!("Live sync error for {}: {}", local_path.display(), e);
                    notify_sync_error_telegram(
                        &path_config,
                        Some(&local_path),
                        &format!("Live sync error: {e}"),
                    )
                    .await;
                    notify_sync_error_matrix(
                        &path_config,
                        Some(&local_path),
                        &format!("Live sync error: {e}"),
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

    println!("CouchDB File Sync daemon started (interval: {interval}s)");
    println!("Syncing {} path(s): {}", paths.len(), path_list.join(", "));
    println!("Press Ctrl+C to stop");

    // Track which conflicts have been notified during this daemon session
    let mut session_notified: HashSet<String> = HashSet::new();

    let mut interval_timer = tokio::time::interval(tokio::time::Duration::from_secs(interval));

    loop {
        interval_timer.tick().await;

        for sync_path in &paths {
            let mut path_config = config.clone();
            path_config
                .couchdb
                .remote_path
                .clone_from(&sync_path.remote);

            match daemon_sync(&sync_path.local, &path_config, &mut session_notified).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Sync error for {}: {}", sync_path.local.display(), e);
                    notify_sync_error_telegram(
                        &path_config,
                        Some(&sync_path.local),
                        &format!("Sync error: {e}"),
                    )
                    .await;
                    notify_sync_error_matrix(
                        &path_config,
                        Some(&sync_path.local),
                        &format!("Sync error: {e}"),
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
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    // Run sync
    let retention = (config.sync.tombstone_retention_secs > 0)
        .then(|| std::time::Duration::from_secs(config.sync.tombstone_retention_secs));
    let mut engine =
        SyncEngine::with_ignore(couchdb, local_db, resolve_sync_dir(path)?, ignore_matcher)
            .with_tombstone_retention(retention);
    let report = engine.sync().await?;

    print_sync_report(&report, false);

    if !report.errors.is_empty() {
        let summary = format_sync_error_summary(&report.errors);
        notify_sync_error_telegram(config, Some(path), &summary).await;
        notify_sync_error_matrix(config, Some(path), &summary).await;
    }

    // Send Telegram notifications for NEW conflicts only (session-based tracking)
    if report.conflicts > 0 {
        notify_conflicts_telegram(config, &db_path, path, Some(session_notified)).await;
        notify_conflicts_matrix(config, &db_path, path, Some(session_notified)).await;
    }

    Ok(report)
}

struct TouchTracker {
    entries: Vec<(String, i64)>,
}

impl TouchTracker {
    const fn new() -> Self {
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
            .as_millis();
        i64::try_from(millis).unwrap_or(i64::MAX) / 5000
    }
}

/// Tracks the live remote changes-feed cursor under ordered application
/// acknowledgement.
///
/// The durable cursor (persisted as the sync checkpoint) advances ONLY to the
/// highest sequence whose contiguous prefix of surfaced changes has been fully
/// applied. A failed entry records a `gap`: while a gap exists, later
/// successfully-applied entries are processed but do NOT advance the cursor, so
/// the checkpoint never passes uncommitted work and the failed entry is never
/// skipped. Because the feed cursor resumes from the checkpoint, a gapped
/// entry is re-fetched and retried on the next pass (live) and after a restart
/// (the checkpoint stays pinned below it).
struct RemoteFeedCursor {
    /// Last sequence that is safe to resume the feed from (the durable
    /// checkpoint reported back to the producer).
    checkpoint: String,
    /// Sequence of an entry that failed to apply and must succeed before the
    /// cursor can advance again.
    gap: Option<String>,
}

impl RemoteFeedCursor {
    const fn new(checkpoint: String) -> Self {
        Self {
            checkpoint,
            gap: None,
        }
    }

    /// Record a successfully applied entry with sequence `seq`.
    ///
    /// Returns the sequence the durable checkpoint should advance to, or
    /// `None` when a prior gap (an earlier failed entry) still pins the cursor.
    fn applied(&mut self, seq: &str) -> Option<String> {
        let clears_gap = self.gap.as_deref() == Some(seq);
        if clears_gap || self.gap.is_none() {
            self.gap = None;
            self.checkpoint = seq.to_string();
            Some(seq.to_string())
        } else {
            None
        }
    }

    /// Record a failed entry with sequence `seq`. The cursor is left pinned
    /// below `seq` (and any earlier failure) so the entry is retried.
    fn failed(&mut self, seq: &str) {
        self.gap = Some(seq.to_string());
    }

    /// The current safe resume cursor (the durable checkpoint).
    fn checkpoint(&self) -> &str {
        &self.checkpoint
    }
}

async fn live_sync_path(path: PathBuf, config: AppConfig) -> Result<()> {
    info!("Starting live sync in: {}", path.display());

    let (ignore_matcher, mut engine) = build_live_sync_engine(&path, &config).await?;
    let initial_since = bootstrap_live_sync(&mut engine).await?;
    let LiveChangeChannels {
        mut local_rx,
        mut remote_rx,
        ack_tx,
        mut reconcile_rx,
    } = spawn_live_change_sources(
        config.clone(),
        &path,
        ignore_matcher.clone(),
        initial_since.clone(),
    )?;

    let mut touched = TouchTracker::new();
    let mut remote_cursor = RemoteFeedCursor::new(initial_since);

    loop {
        tokio::select! {
            Some(change) = local_rx.recv() => {
                if let Err(e) = handle_local_change(&mut engine, &touched, &change).await {
                    warn!("Live local change error for {}: {}", change.path(), e);
                    notify_sync_error_telegram(
                        &config,
                        Some(&path),
                        &format!("Live local change error for {}: {}", change.path(), e),
                    )
                    .await;
                    notify_sync_error_matrix(
                        &config,
                        Some(&path),
                        &format!("Live local change error for {}: {}", change.path(), e),
                    )
                    .await;
                }
            }
            Some(entry) = remote_rx.recv() => {
                let seq = entry.seq.clone();
                match handle_remote_change(&mut engine, &mut touched, &ignore_matcher, entry).await {
                    Ok(()) => {
                        // A failed persistence leaves the durable cursor behind
                        // (safe: nothing is skipped); a restart re-applies the
                        // entry idempotently.
                        if let Some(advance_to) = remote_cursor.applied(&seq)
                            && let Err(e) = engine.save_checkpoint(&advance_to).await
                        {
                            warn!("Failed to persist live checkpoint at {advance_to}: {e}");
                        }
                    }
                    Err(e) => {
                        remote_cursor.failed(&seq);
                        warn!("Live remote change error: {}", e);
                        notify_sync_error_telegram(
                            &config,
                            Some(&path),
                            &format!("Live remote change error: {e}"),
                        )
                        .await;
                        notify_sync_error_matrix(
                            &config,
                            Some(&path),
                            &format!("Live remote change error: {e}"),
                        )
                        .await;
                    }
                }
                // Acknowledge the durable cursor back to the producer after every
                // entry (success or failure) so the feed resumes from the highest
                // contiguously-applied sequence.
                if ack_tx.send(remote_cursor.checkpoint().to_string()).await.is_err() {
                    return Ok(());
                }
            }
            Some(()) = reconcile_rx.recv() => {
                // The producer hit an invalid resume cursor. Run a full
                // reconciliation to rebuild a valid checkpoint, then report the
                // fresh cursor so the feed resumes from a known-good position
                // instead of jumping to "now".
                if let Err(e) = engine.sync().await {
                    warn!("Live reconciliation failed: {}", e);
                }
                let seq = engine
                    .get_checkpoint()
                    .await
                    .ok()
                    .flatten()
                    .map_or_else(|| remote_cursor.checkpoint().to_string(), |cp| cp.last_seq);
                remote_cursor = RemoteFeedCursor::new(seq.clone());
                if ack_tx.send(seq).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

async fn build_live_sync_engine(
    path: &Path,
    config: &AppConfig,
) -> Result<(Arc<IgnoreMatcher>, SyncEngine)> {
    let ignore_matcher = Arc::new(load_ignore_patterns(path));

    let db_path = state_db_path(path);
    let local_db = LocalDb::open(&db_path)?;

    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    let sync_dir = resolve_sync_dir(path)?;

    let engine = SyncEngine::with_ignore(couchdb, local_db, sync_dir, (*ignore_matcher).clone())
        .with_tombstone_retention(
            (config.sync.tombstone_retention_secs > 0)
                .then(|| std::time::Duration::from_secs(config.sync.tombstone_retention_secs)),
        );

    Ok((ignore_matcher, engine))
}

async fn bootstrap_live_sync(engine: &mut SyncEngine) -> Result<String> {
    // Run one full sync cycle before listening so pre-existing remote files
    // are materialized locally. The live changes feed otherwise starts from
    // "now"/the checkpoint and a fresh daemon would never fetch remote files
    // that already exist in the database before it started.
    let bootstrap_report = engine.sync().await?;
    if bootstrap_report.downloaded.0 > 0 || bootstrap_report.uploaded.0 > 0 {
        info!(
            "Live bootstrap sync: {} uploaded, {} downloaded, {} conflicts",
            bootstrap_report.uploaded.0, bootstrap_report.downloaded.0, bootstrap_report.conflicts
        );
    } else {
        info!("Live bootstrap sync: already in sync");
    }

    // The durable checkpoint is the only safe resume cursor for the live feed.
    // A full bootstrap sync always records a checkpoint when it completes, so a
    // missing/unknown checkpoint here means bootstrap did not durably land. Do
    // NOT fall back to "now": a "now" start would skip every remote change that
    // predates the daemon (a fresh daemon must never miss pre-existing remote
    // files), so the unknown-cursor case is an error, not a silent "now".
    let initial_since: String = engine
        .get_checkpoint()
        .await?
        .map(|cp| cp.last_seq)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live bootstrap sync completed without a durable checkpoint; refusing to start the changes feed from an unknown cursor (a 'now' start could skip pre-existing remote files)"
            )
        })?;

    Ok(initial_since)
}

/// Channels bridging the live changes-feed producer task and the consumer
/// select-loop: the remote feed entries, plus the ordered-ack and
/// reconciliation channels that keep the durable cursor replay-safe.
struct LiveChangeChannels {
    local_rx: mpsc::Receiver<Change>,
    remote_rx: mpsc::Receiver<ChangeFeedEntry>,
    ack_tx: mpsc::Sender<String>,
    reconcile_rx: mpsc::Receiver<()>,
}

fn spawn_live_change_sources(
    config: AppConfig,
    path: &Path,
    ignore_matcher: Arc<IgnoreMatcher>,
    initial_since: String,
) -> Result<LiveChangeChannels> {
    let (local_tx, local_rx) = mpsc::channel::<Change>(256);
    let (remote_tx, remote_rx) = mpsc::channel::<ChangeFeedEntry>(256);
    // The consumer acknowledges each durably-applied sequence back to the
    // producer (ordered ack), and can be asked to run a full reconciliation
    // when the producer hits an invalid resume cursor.
    let (ack_tx, ack_rx) = mpsc::channel::<String>(256);
    let (reconcile_tx, reconcile_rx) = mpsc::channel::<()>(1);

    let watcher_root = resolve_sync_dir(path)?;
    let watcher_ignore = ignore_matcher;
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
                &format!("Local watcher error: {e}"),
            )
            .await;
            notify_sync_error_matrix(
                &watcher_config,
                Some(&watcher_root),
                &format!("Local watcher error: {e}"),
            )
            .await;
        }
    });

    let remote_config = config;
    let remote_config_notify = remote_config.clone();
    let remote_root = path.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = run_remote_changes(
            remote_config,
            initial_since,
            remote_tx,
            ack_rx,
            reconcile_tx,
        )
        .await
        {
            error!("Remote changes feed error: {}", e);
            notify_sync_error_telegram(
                &remote_config_notify,
                Some(&remote_root),
                &format!("Remote changes feed error: {e}"),
            )
            .await;
            notify_sync_error_matrix(
                &remote_config_notify,
                Some(&remote_root),
                &format!("Remote changes feed error: {e}"),
            )
            .await;
        }
    });

    Ok(LiveChangeChannels {
        local_rx,
        remote_rx,
        ack_tx,
        reconcile_rx,
    })
}

async fn run_local_watcher(
    root: SyncDirPath,
    ignore_matcher: Arc<IgnoreMatcher>,
    debounce_ms: u64,
    tx: mpsc::Sender<Change>,
) -> Result<()> {
    let mut watcher =
        AsyncFileWatcher::start(root.clone(), (*ignore_matcher).clone(), debounce_ms)?;

    loop {
        if let Some(event) = watcher.next_event().await
            && let Some(change) = watcher.event_to_change(event)
            && tx.send(change).await.is_err()
        {
            break;
        }
    }

    Ok(())
}

async fn run_remote_changes(
    config: AppConfig,
    mut since: String,
    tx: mpsc::Sender<ChangeFeedEntry>,
    mut ack_rx: mpsc::Receiver<String>,
    reconcile_tx: mpsc::Sender<()>,
) -> Result<()> {
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    loop {
        match couchdb.get_changes_feed(&since, 25_000).await {
            Ok((entries, _last_seq)) => {
                // The cursor advances ONLY to the sequence the consumer has
                // durably applied (its ordered ack), never to the feed's
                // trailing `last_seq`. `last_seq` reflects the whole batch,
                // including entries the consumer may not have applied yet; if
                // we jumped straight to it, a failed entry in the batch would
                // be skipped on the next fetch. Each entry is acknowledged in
                // feed order, so `since` climbs exactly to the highest
                // contiguously-applied sequence and a failed entry is re-fetched
                // and retried on the next pass.
                let entry_count = entries.len();
                for entry in entries {
                    if tx.send(entry).await.is_err() {
                        return Ok(());
                    }
                }
                for _ in 0..entry_count {
                    match ack_rx.recv().await {
                        Some(watermark) => since = watermark,
                        None => return Ok(()),
                    }
                }
            }
            Err(e) => {
                if let Some(status) = e
                    .downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    && status == StatusCode::BAD_REQUEST
                {
                    // Invalid resume cursor. Do NOT jump to "now": that would
                    // silently skip every change before the present moment.
                    // Instead ask the consumer to run a full reconciliation
                    // (which rebuilds a fresh, valid checkpoint), then resume
                    // the feed from that reconciled cursor.
                    warn!(
                        "Changes feed rejected cursor {since:?} (400); requesting reconciliation instead of jumping to \"now\""
                    );
                    if reconcile_tx.send(()).await.is_err() {
                        return Ok(());
                    }
                    match ack_rx.recv().await {
                        Some(seq) => since = seq,
                        None => return Ok(()),
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                error!("Changes feed error: {}", e);
                notify_sync_error_telegram(&config, None, &format!("Changes feed error: {e}"))
                    .await;
                notify_sync_error_matrix(&config, None, &format!("Changes feed error: {e}")).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn handle_local_change(
    engine: &mut SyncEngine,
    touched: &TouchTracker,
    change: &Change,
) -> Result<()> {
    let mtime = local_mtime(engine.root_dir(), change.path()).await;
    if touched.is_touched(change.path(), mtime) {
        return Ok(());
    }

    // A local delete racing a remote edit is an edit/delete conflict: do not
    // blindly upload (which would destroy the remote edit). Arbitrate against
    // the live remote using the monotonic revision, like the periodic and
    // live-remote paths.
    if change.change_type() == ChangeType::Deleted
        && engine.arbitrate_local_delete(change.path()).await?
    {
        return Ok(());
    }

    match change.change_type() {
        ChangeType::Created | ChangeType::Modified | ChangeType::Deleted => {
            if let Err(e) = engine.apply_local_change(change).await {
                warn!("Failed to apply local change {}: {}", change.path(), e);
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

    let local_path = engine.remote_to_local_path(change.path());
    let local_rel = local_path.trim_start_matches('/').to_string();

    // The durable checkpoint is advanced by the live-sync consumer (via the
    // ordered-ack cursor), not here: it must only advance past an entry once
    // the whole contiguous prefix of surfaced changes has been applied, so a
    // failed entry is never skipped. These early returns just report success.

    if ignore_matcher.should_ignore(Path::new(&local_rel)) {
        return Ok(());
    }

    if let Some(state) = engine.get_file_state(&local_rel).await?
        && let (Some(remote_rev), Some(local_rev)) = (change.rev(), state.couch_rev.as_deref())
        && remote_rev == local_rev
    {
        return Ok(());
    }

    match change.change_type() {
        ChangeType::Deleted => {
            // Arbitrate a remote delete against the local state exactly like
            // the periodic path: protect local edits and untracked files, and
            // only propagate the deletion when the local file is unchanged and
            // the deletion is newer than our last sync. A delete/edit race
            // becomes a recorded conflict instead of silently deleting the
            // local edit.
            let state = engine.get_file_state(&local_rel).await?;
            let file_path = engine.root_dir().join(&local_rel);
            let local_meta = tokio::fs::metadata(&file_path).await.ok();
            let local_mtime = local_meta
                .as_ref()
                .and_then(|meta| meta.modified().ok())
                .map(Into::into);
            // Same-mtime content edits must be protected even when the mtime
            // matches the stored sync-time mtime.
            let local_content_matches = match (&state, local_meta.as_ref()) {
                (Some(s), Some(_)) => compute_file_hash(&file_path)? == s.hash,
                _ => true,
            };
            match triage::arbitrate_remote_delete(
                state.as_ref(),
                change.rev(),
                change.mtime().copied(),
                local_mtime,
                local_content_matches,
            ) {
                triage::RemoteDeleteVerdict::Apply => {
                    touched.mark(&local_rel, SystemTime::now());
                    engine.apply_remote_change(&change).await?;
                }
                triage::RemoteDeleteVerdict::Conflict => {
                    let local_state = match &state {
                        Some(s) => s.clone(),
                        None => FileState {
                            path: local_rel.clone(),
                            hash: compute_file_hash(&file_path)?,
                            size: file_path.metadata().map_or(0, |m| m.len()),
                            modified_at: local_mtime.unwrap_or_else(Utc::now),
                            couch_rev: None,
                            last_sync_at: Utc::now(),
                        },
                    };
                    let remote_state = RemoteState {
                        hash: String::new(),
                        size: 0,
                        modified_at: change.mtime().copied().unwrap_or_else(Utc::now),
                        couch_rev: change.rev().and_then(CouchRev::new).unwrap_or_default(),
                        deleted: true,
                    };
                    engine
                        .record_conflict(&Conflict::new(
                            local_rel.clone(),
                            local_state,
                            remote_state,
                        ))
                        .await?;
                }
                triage::RemoteDeleteVerdict::Skip => {}
            }
        }
        ChangeType::Created | ChangeType::Modified => {
            let file_path = engine.root_dir().join(&local_rel);
            let local_meta = tokio::fs::metadata(&file_path).await.ok();
            let local_exists = local_meta.is_some();
            let local_mtime_val = local_meta
                .and_then(|meta| meta.modified().ok())
                .unwrap_or_else(SystemTime::now);
            let local_mtime = DateTime::<Utc>::from(local_mtime_val);

            // Arbitrate using the authoritative CouchDB revision (remote side)
            // and the stored local sync state (same-machine mtime, immune to
            // cross-host clock skew) rather than comparing local vs remote
            // mtimes with a 1s tolerance.
            let state = engine.get_file_state(&local_rel).await?;
            let apply_remote = triage::live_should_apply_remote(
                change.rev(),
                state.as_ref(),
                local_exists,
                local_mtime,
            );

            if apply_remote {
                touched.mark(&local_rel, SystemTime::now());
                engine.apply_remote_change(&change).await?;
            } else {
                // Upload the local version to arbitrate. Carry the observed
                // remote revision as the publication baseline so a further
                // concurrent remote write landing during the upload is rejected
                // (surfaced as an error) instead of silently overwritten.
                let local_change = Change::local_modified(local_rel.clone(), String::new(), 0)
                    .with_expected_rev(change.rev().map_or(ExpectedRemote::Force, |rev| {
                        ExpectedRemote::At(rev.to_string())
                    }));
                engine.apply_local_change(&local_change).await?;
            }
        }
    }

    Ok(())
}

async fn local_mtime(root: &Path, relative_path: &str) -> SystemTime {
    let file_path = root.join(relative_path);
    tokio::fs::metadata(&file_path)
        .await
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| SystemTime::now())
}

/// List conflicts
/// List conflicts for a sync directory.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or read.
pub fn conflicts(path: &Path, json: bool) -> Result<()> {
    let db_path = state_db_path(path);
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
/// Interactively resolve conflicts for a sync directory.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or a conflict
/// cannot be resolved.
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
        config.couchdb.auth.as_ref().map(|a| a.username.as_str()),
        config.couchdb.auth.as_ref().map(|a| a.password.as_str()),
        &DatabaseName::new(&config.couchdb.database),
        &RemotePath::new(&config.couchdb.remote_path),
        config.couchdb.timeout_seconds,
        config.couchdb.retry_attempts,
    )
    .await?;

    let mut engine = SyncEngine::new(couchdb, local_db, resolve_sync_dir(&path)?);

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
                println!("Could not read local file: {e}");
                String::new()
            }
        };

        // Fetch remote content
        let remote_content = match engine.get_remote_content(&conflict.path).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(e) => {
                println!("Could not fetch remote file: {e}");
                String::new()
            }
        };

        // Display side-by-side diff
        print_side_by_side_diff(&local_content, &remote_content);

        // Ask user for action
        let strategies = [
            (ResolutionStrategy::KeepLocal, "Keep Local"),
            (ResolutionStrategy::KeepRemote, "Keep Remote"),
            (ResolutionStrategy::KeepBoth, "Keep Both (merge manually)"),
            (ResolutionStrategy::Skip, "Skip"),
        ];
        let options: Vec<&str> = strategies.iter().map(|(_, label)| *label).collect();
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("How do you want to resolve this conflict?")
            .items(&options)
            .default(0)
            .interact()?;

        let strategy = strategies[selection].0;

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
                println!(" {truncated:<width$} │ {truncated:<width$}");
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
/// Show sync status for a sync directory.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or read.
pub fn status(path: &Path, json: bool, _config: &AppConfig) -> Result<()> {
    let db_path = state_db_path(path);
    let local_db = LocalDb::open(&db_path)?;

    let file_states = local_db.get_all_file_states()?;
    let conflicts = local_db.get_conflicts()?;
    let checkpoint = local_db.get_checkpoint()?;

    // Count files by walking directory
    let file_count = walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .count();

    if json {
        let status = serde_json::json!({
            "sync_directory": path,
            "tracked_files": file_states.len(),
            "local_files": file_count,
            "pending_conflicts": conflicts.len(),
            "last_sync": checkpoint.as_ref().map(|cp| cp.last_sync_at),
        });
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("CouchDB File Sync Status");
        println!("==============");
        println!("Sync directory: {}", path.display());
        println!("Local files:    {file_count}");
        println!("Tracked files:  {}", file_states.len());
        println!("Pending conflicts: {}", conflicts.len());

        if let Some(cp) = &checkpoint {
            println!(
                "Last sync:      {}",
                cp.last_sync_at.format("%Y-%m-%d %H:%M:%S UTC")
            );
            println!("Last sequence:  {}", cp.last_seq);
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

/// Helper to print sync report.
///
/// When `dry_run` is true the counts are labelled as "would" actions and the
/// summary makes clear that no changes were made.
fn print_sync_report(report: &SyncReport, dry_run: bool) {
    println!();
    if dry_run {
        println!("Dry run complete — no changes were made");
        println!("  Would upload:  {}", report.uploaded.0);
        println!("  Would download: {}", report.downloaded.0);
        println!("  Would delete (local): {}", report.deleted_local);
        println!("  Would delete (remote): {}", report.deleted_remote);
    } else {
        println!("Sync complete ✓");
        println!("  Uploaded:  {}", report.uploaded.0);
        println!("  Downloaded: {}", report.downloaded.0);
        println!("  Deleted (local): {}", report.deleted_local);
        println!("  Deleted (remote): {}", report.deleted_remote);
    }

    if report.conflicts > 0 {
        println!("  Conflicts: {} ⚠️", report.conflicts);
        println!();
        println!("Run 'couchdb-file-sync conflicts' to see details");
    }

    if !report.errors.is_empty() {
        println!();
        println!("Errors:");
        for error in &report.errors {
            println!("  • {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::couchdb::db::CannedCouch;
    use crate::models::{DownloadCount, FileDoc, UploadCount};
    use std::time::SystemTime;

    // ── resolve_sync_dir (CLI path resolution) ────────────────────────────

    // Regression test for story #2844: `sync ""` / any bad sync path must
    // surface a clean, contextual error instead of panicking on
    // `SyncDirPath::new(path).unwrap()`.
    #[test]
    fn cli_sync_rejects_empty_path_without_panicking() {
        let result = resolve_sync_dir(Path::new(""));
        let err = match result {
            Ok(dir) => panic!("expected empty path to fail, resolved to {}", dir.display()),
            Err(e) => e,
        };
        // The error must be user-facing and contextual, not a panic message.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cannot resolve sync directory"),
            "expected contextual error, got: {msg}"
        );
    }

    #[test]
    fn resolve_sync_dir_accepts_valid_directory() {
        let dir = std::env::temp_dir().join(format!("resolve-sync-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let resolved = resolve_sync_dir(&dir).expect("valid directory should resolve");
        assert_eq!(resolved.as_path(), dir.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── truncate_str ──────────────────────────────────────────────────────

    #[test]
    fn test_truncate_str_short_string() {
        let result = truncate_str("hello", 10);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_truncate_str_exact_width() {
        let result = truncate_str("hello", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_truncate_str_overflow_with_ellipsis() {
        let result = truncate_str("hello world", 8);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn test_truncate_str_width_three() {
        let result = truncate_str("hello world", 3);
        assert_eq!(result, "...");
    }

    #[test]
    fn test_truncate_str_width_two() {
        let result = truncate_str("hello world", 2);
        assert_eq!(result, "..");
    }

    #[test]
    fn test_truncate_str_width_one() {
        let result = truncate_str("hello world", 1);
        assert_eq!(result, ".");
    }

    #[test]
    fn test_truncate_str_width_zero() {
        let result = truncate_str("hello world", 0);
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_str_empty_string() {
        let result = truncate_str("", 10);
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_str_unicode() {
        let result = truncate_str("héllo wörld", 8);
        assert_eq!(result, "héllo...");
    }

    // ── format_sync_error_summary ─────────────────────────────────────────

    #[test]
    fn test_format_sync_error_summary_empty() {
        let result = format_sync_error_summary(&[]);
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_sync_error_summary_single() {
        let result = format_sync_error_summary(&["file not found".to_string()]);
        assert_eq!(result, "• file not found");
    }

    #[test]
    fn test_format_sync_error_summary_multiple_within_limit() {
        let errors: Vec<String> = (1..=5).map(|i| format!("error {i}")).collect();
        let result = format_sync_error_summary(&errors);
        let expected = (1..=5)
            .map(|i| format!("• error {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_format_sync_error_summary_truncation() {
        let errors: Vec<String> = (1..=15).map(|i| format!("error {i}")).collect();
        let result = format_sync_error_summary(&errors);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 11);
        assert!(lines[10].starts_with("...and 5 more"));
    }

    #[test]
    fn test_format_sync_error_summary_exactly_ten() {
        let errors: Vec<String> = (1..=10).map(|i| format!("error {i}")).collect();
        let result = format_sync_error_summary(&errors);
        assert_eq!(result.lines().count(), 10);
        assert!(!result.contains("...and"));
    }

    // ── print_sync_report ─────────────────────────────────────────────────

    #[test]
    fn test_print_sync_report_empty() {
        let report = SyncReport::default();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            print_sync_report(&report, false);
        }));
    }

    #[test]
    fn test_print_sync_report_with_errors() {
        let report = SyncReport {
            uploaded: UploadCount(5),
            downloaded: DownloadCount(3),
            deleted_local: 1,
            deleted_remote: 0,
            conflicts: 2,
            errors: vec!["timeout".to_string(), "permission denied".to_string()],
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            print_sync_report(&report, false);
        }));
    }

    #[test]
    fn test_print_sync_report_with_conflicts() {
        let report = SyncReport {
            uploaded: UploadCount(2),
            downloaded: DownloadCount(1),
            deleted_local: 0,
            deleted_remote: 1,
            conflicts: 3,
            errors: vec![],
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            print_sync_report(&report, false);
        }));
    }

    #[test]
    fn test_print_sync_report_all_zeros() {
        let report = SyncReport {
            uploaded: UploadCount(0),
            downloaded: DownloadCount(0),
            deleted_local: 0,
            deleted_remote: 0,
            conflicts: 0,
            errors: vec![],
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            print_sync_report(&report, false);
        }));
    }

    // ── state_dir / state_db_path ─────────────────────────────────────────

    #[test]
    fn test_state_dir_new_format() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".couchdb-file-sync")).unwrap();
        let result = state_dir(root);
        assert_eq!(result, root.join(".couchdb-file-sync"));
    }

    #[test]
    fn test_state_dir_old_format() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".couchfs")).unwrap();
        let result = state_dir(root);
        assert_eq!(result, root.join(".couchfs"));
    }

    #[test]
    fn test_state_dir_prefers_new_format() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".couchdb-file-sync")).unwrap();
        std::fs::create_dir_all(root.join(".couchfs")).unwrap();
        let result = state_dir(root);
        assert_eq!(result, root.join(".couchdb-file-sync"));
    }

    #[test]
    fn test_state_dir_neither_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let result = state_dir(root);
        assert_eq!(result, root.join(".couchdb-file-sync"));
    }

    #[test]
    fn test_state_db_path_construction() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let result = state_db_path(root);
        assert_eq!(result, root.join(".couchdb-file-sync").join("state.db"));
    }

    #[test]
    fn test_state_db_path_with_existing_new_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".couchdb-file-sync")).unwrap();
        let result = state_db_path(root);
        assert_eq!(result, root.join(".couchdb-file-sync").join("state.db"));
    }

    // ── local_mtime ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_local_mtime_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("test.txt"), "content").unwrap();
        let mtime = local_mtime(root, "test.txt").await;
        let now = SystemTime::now();
        let diff = now.duration_since(mtime).unwrap_or_default();
        assert!(diff.as_secs() < 5);
    }

    #[tokio::test]
    async fn test_local_mtime_file_not_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mtime = local_mtime(root, "nonexistent.txt").await;
        let now = SystemTime::now();
        let diff = now.duration_since(mtime).unwrap_or_default();
        assert!(diff.as_secs() < 5);
    }

    #[tokio::test]
    async fn test_local_mtime_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::write(root.join("subdir/file.txt"), "content").unwrap();
        let mtime = local_mtime(root, "subdir/file.txt").await;
        let now = SystemTime::now();
        let diff = now.duration_since(mtime).unwrap_or_default();
        assert!(diff.as_secs() < 5);
    }

    // ── load_ignore_patterns ──────────────────────────────────────────────

    #[test]
    fn test_load_ignore_patterns_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let matcher = load_ignore_patterns(root);
        assert!(matcher.is_empty());
    }

    #[test]
    fn test_load_ignore_patterns_with_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".sync-ignore"), "*.log\n*.tmp\nbuild/\n").unwrap();
        let matcher = load_ignore_patterns(root);
        assert!(!matcher.is_empty());
        assert!(matcher.is_ignored("debug.log", false));
        assert!(matcher.is_ignored("file.tmp", false));
        assert!(!matcher.is_ignored("main.rs", false));
    }

    #[test]
    fn test_load_ignore_patterns_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".sync-ignore"), "").unwrap();
        let matcher = load_ignore_patterns(root);
        assert!(matcher.is_empty());
    }

    #[test]
    fn test_load_ignore_patterns_comments_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".sync-ignore"),
            "# This is a comment\n# Another comment\n",
        )
        .unwrap();
        let matcher = load_ignore_patterns(root);
        assert!(matcher.is_empty());
    }

    // ── RemoteFeedCursor (ordered-ack live cursor) ────────────────────────

    #[test]
    fn remote_cursor_advances_through_a_clean_run() {
        let mut cursor = RemoteFeedCursor::new("0-before".to_string());
        assert_eq!(cursor.applied("1-first"), Some("1-first".to_string()));
        assert_eq!(cursor.applied("2-second"), Some("2-second".to_string()));
        assert_eq!(cursor.checkpoint(), "2-second");
    }

    #[test]
    fn remote_cursor_pins_on_failure_and_later_success_does_not_skip_it() {
        let mut cursor = RemoteFeedCursor::new("0-before".to_string());
        // First event fails → cursor pins below it.
        cursor.failed("1-first");
        assert_eq!(cursor.checkpoint(), "0-before");
        // Second event succeeds, but the first is still un-applied: the cursor
        // must NOT advance past the failed entry.
        assert_eq!(
            cursor.applied("2-second"),
            None,
            "cursor must not advance past a failed entry"
        );
        assert_eq!(cursor.checkpoint(), "0-before");
    }

    #[test]
    fn remote_cursor_retries_the_failed_entry_then_advances() {
        let mut cursor = RemoteFeedCursor::new("0-before".to_string());
        cursor.failed("1-first");
        assert_eq!(cursor.applied("2-second"), None);
        // Retry pass re-fetches the failed entry first; once it applies the gap
        // clears and the cursor resumes advancing.
        assert_eq!(cursor.applied("1-first"), Some("1-first".to_string()));
        assert_eq!(cursor.applied("2-second"), Some("2-second".to_string()));
        assert_eq!(cursor.checkpoint(), "2-second");
    }

    #[test]
    fn remote_cursor_keeps_pinned_until_every_entry_in_the_pass_applies() {
        let mut cursor = RemoteFeedCursor::new("0-before".to_string());
        // Pass 1: first entry fails, second applies, third fails. The cursor
        // stays pinned at the last fully-applied prefix (before the first).
        cursor.failed("1-first");
        assert_eq!(cursor.applied("2-second"), None);
        cursor.failed("3-third");
        assert_eq!(cursor.checkpoint(), "0-before");
        // Retry pass: the feed is re-fetched from the pinned checkpoint, so
        // 1-first and 2-second are re-applied before 3-third. The cursor only
        // advances once the trailing gap (3-third) applies, which then covers
        // the whole re-applied prefix.
        assert_eq!(cursor.applied("1-first"), None);
        assert_eq!(cursor.applied("2-second"), None);
        assert_eq!(cursor.applied("3-third"), Some("3-third".to_string()));
        assert_eq!(cursor.checkpoint(), "3-third");
    }

    // ── Live remote checkpoint discipline + restart replay ───────────────

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn live_remote_checkpoint_never_passes_a_failed_entry_and_restart_replays() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::create_dir_all(root.as_path().join(".couchdb-file-sync")).unwrap();
        let state_path = root.as_path().join(".couchdb-file-sync/state.db");
        let local = LocalDb::open(&state_path).unwrap();
        local.save_checkpoint("0-before").unwrap();

        let remote_prefix = "prefix/";
        let first_path = format!("{remote_prefix}first.txt");
        let second_path = format!("{remote_prefix}second.txt");

        let canned = CannedCouch {
            metadata: std::collections::HashMap::from([
                (
                    first_path.clone(),
                    FileDoc::new(first_path.clone(), String::new(), 5),
                ),
                (
                    second_path.clone(),
                    FileDoc::new(second_path.clone(), String::new(), 6),
                ),
            ]),
            contents: std::collections::HashMap::from([
                (first_path.clone(), b"first".to_vec()),
                (second_path.clone(), b"second".to_vec()),
            ]),
            // Inject a failure on the FIRST remote event's content download.
            content_errors: std::collections::HashSet::from([first_path.clone()]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            CouchDb::for_test_with_canned(remote_prefix, canned),
            local,
            root.clone(),
            IgnoreMatcher::empty(),
        );
        let mut touched = TouchTracker::new();
        let mut cursor = RemoteFeedCursor::new("0-before".to_string());

        let first_entry = ChangeFeedEntry {
            change: Change::remote_created(
                first_path.clone(),
                "hash".to_string(),
                5,
                Utc::now(),
                "1-first".to_string(),
            ),
            seq: "1-first".to_string(),
        };
        let second_entry = ChangeFeedEntry {
            change: Change::remote_created(
                second_path.clone(),
                "hash".to_string(),
                6,
                Utc::now(),
                "2-second".to_string(),
            ),
            seq: "2-second".to_string(),
        };

        // Session 1: the first event fails to apply...
        let first_result = handle_remote_change(
            &mut engine,
            &mut touched,
            &IgnoreMatcher::empty(),
            first_entry,
        )
        .await;
        assert!(first_result.is_err(), "first event should fail to apply");
        cursor.failed("1-first");
        assert!(
            !root.as_path().join("first.txt").exists(),
            "failed first event must not be applied"
        );

        // ...and the second event succeeds, but the durable cursor must NOT
        // advance past the failed first event.
        handle_remote_change(
            &mut engine,
            &mut touched,
            &IgnoreMatcher::empty(),
            second_entry,
        )
        .await
        .expect("second event should apply");
        assert_eq!(
            cursor.applied("2-second"),
            None,
            "cursor must not advance past the failed entry"
        );
        assert_eq!(cursor.checkpoint(), "0-before");
        assert!(root.as_path().join("second.txt").exists());

        let cp = engine
            .get_checkpoint()
            .await
            .unwrap()
            .expect("checkpoint exists");
        assert_eq!(
            cp.last_seq, "0-before",
            "checkpoint must never pass uncommitted work"
        );

        // Restart: a fresh engine from the same durable DB, now with the content
        // fetch fixed. The failed first event is re-fetched from the pinned
        // checkpoint and must apply; both events end up applied.
        let canned_ok = CannedCouch {
            metadata: std::collections::HashMap::from([
                (
                    first_path.clone(),
                    FileDoc::new(first_path.clone(), String::new(), 5),
                ),
                (
                    second_path.clone(),
                    FileDoc::new(second_path.clone(), String::new(), 6),
                ),
            ]),
            contents: std::collections::HashMap::from([
                (first_path.clone(), b"first".to_vec()),
                (second_path.clone(), b"second".to_vec()),
            ]),
            ..Default::default()
        };
        let mut engine2 = SyncEngine::with_ignore(
            CouchDb::for_test_with_canned(remote_prefix, canned_ok),
            LocalDb::open(&state_path).unwrap(),
            root.clone(),
            IgnoreMatcher::empty(),
        );
        let mut touched2 = TouchTracker::new();
        let mut cursor2 =
            RemoteFeedCursor::new(engine2.get_checkpoint().await.unwrap().unwrap().last_seq);

        for (path, size, seq, rev) in [
            (first_path.clone(), 5, "1-first", "1-first"),
            (second_path.clone(), 6, "2-second", "2-second"),
        ] {
            let entry = ChangeFeedEntry {
                change: Change::remote_created(
                    path.clone(),
                    "hash".to_string(),
                    size,
                    Utc::now(),
                    rev.to_string(),
                ),
                seq: seq.to_string(),
            };
            handle_remote_change(&mut engine2, &mut touched2, &IgnoreMatcher::empty(), entry)
                .await
                .expect("both events must apply after restart");
            if let Some(advance_to) = cursor2.applied(seq) {
                engine2.save_checkpoint(&advance_to).await.unwrap();
            }
        }

        assert!(
            root.as_path().join("first.txt").exists(),
            "failed first event must apply after restart (never skipped)"
        );
        assert!(root.as_path().join("second.txt").exists());
        let cp2 = engine2
            .get_checkpoint()
            .await
            .unwrap()
            .expect("checkpoint exists after restart");
        assert_eq!(cp2.last_seq, "2-second");
    }

    #[tokio::test]
    async fn bootstrap_live_resumes_from_durable_checkpoint_not_now() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let canned = CannedCouch {
            last_seq: "99-after".to_string(),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            CouchDb::for_test_with_canned("prefix/", canned),
            LocalDb::open_in_memory().unwrap(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let since = bootstrap_live_sync(&mut engine).await.expect("bootstrap");
        assert_eq!(
            since, "99-after",
            "live feed must resume from the durable checkpoint, never from 'now'"
        );
    }

    #[tokio::test]
    async fn bootstrap_live_unknown_checkpoint_after_failure_errors_not_now() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        // A remote change whose content cannot be fetched makes the bootstrap
        // sync record an error and therefore NOT save a checkpoint.
        let remote = "prefix/fail.txt".to_string();
        let canned = CannedCouch {
            changes: vec![Change::remote_created(
                remote.clone(),
                "hash".to_string(),
                4,
                Utc::now(),
                "1-a".to_string(),
            )],
            last_seq: "99-after".to_string(),
            metadata: std::collections::HashMap::from([(
                remote.clone(),
                FileDoc::new(remote.clone(), String::new(), 4),
            )]),
            content_errors: std::collections::HashSet::from([remote]),
            ..Default::default()
        };
        let mut engine = SyncEngine::with_ignore(
            CouchDb::for_test_with_canned("prefix/", canned),
            LocalDb::open_in_memory().unwrap(),
            root.clone(),
            IgnoreMatcher::empty(),
        );

        let result = bootstrap_live_sync(&mut engine).await;
        assert!(
            result.is_err(),
            "bootstrap with no durable checkpoint must error, never start from 'now'"
        );
    }
}
