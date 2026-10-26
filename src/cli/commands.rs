use crate::config::{
    AppConfig, ConflictStrategy, IgnoreConfig, SyncPath, default_user_config_file,
};
use crate::couchdb::{ChangeFeedEntry, ChangesFeed, CouchDb};
use crate::local::{AsyncFileWatcher, LocalDb, SyncIdentityCheck, compute_file_hash};
use crate::matrix::MatrixNotifier;
use crate::models::{
    Change, ChangeType, Conflict, CouchRev, DatabaseName, ExpectedRemote, FileState, IgnoreMatcher,
    RemotePath, RemoteState, ResolutionStrategy, SyncDirPath, SyncIdentity,
};
use crate::notify::{ConflictNotifier, conflict_generation};
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
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};
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

/// Build the validated sync-mapping identity for a local root, remote
/// database, and (raw) remote prefix. The prefix is normalized via
/// [`RemotePath`] so equivalent spellings always compare equal, and the remote
/// database UUID is fetched so a recreated database is detected.
async fn sync_identity_for(
    couchdb: &CouchDb,
    root: &SyncDirPath,
    database: &str,
    raw_prefix: &str,
) -> Result<SyncIdentity> {
    let db_uuid = couchdb.get_db_uuid().await?;
    Ok(SyncIdentity::new(
        root.as_path().display().to_string(),
        database,
        db_uuid,
        RemotePath::new(raw_prefix).as_str(),
    ))
}

/// Synchronously validate that stored local state may be reused for the given
/// mapping. `LocalDb` is not `Sync`, so callers must fetch the identity (via
/// [`sync_identity_for`]) before holding a `&LocalDb` across any await.
///
/// A database with no recorded identity (fresh or legacy) is conservatively
/// bound to the current mapping and its state kept; a mapping change errors
/// out, requiring an explicit reset/rebuild.
fn validate_sync_mapping(local_db: &LocalDb, identity: &SyncIdentity) -> Result<()> {
    match local_db.validate_sync_identity(identity)? {
        SyncIdentityCheck::Matched => {}
        SyncIdentityCheck::Bound => {
            info!(
                "Bound local state to sync mapping (root '{}', db '{}', prefix '{}')",
                identity.root, identity.database, identity.prefix
            );
        }
        SyncIdentityCheck::Mismatched { stored, current } => {
            anyhow::bail!(
                "Sync mapping changed since the local state was recorded: \
                 stored root '{}', db '{}', prefix '{}'; current root '{}', db '{}', prefix '{}'. \
                 Reusing the old state could miss or corrupt files. \
                 Run `couchdb-file-sync rebuild-remote <path>` or `rebuild-local <path>` to \
                 explicitly reset state for the new mapping.",
                stored.root,
                stored.database,
                stored.prefix,
                current.root,
                current.database,
                current.prefix
            );
        }
    }
    Ok(())
}

/// Synchronously bind a freshly-reset local state DB to the current mapping.
/// Used by the explicit rebuild commands, which are the sanctioned way to reset
/// state after a mapping change.
fn bind_sync_mapping(local_db: &LocalDb, identity: &SyncIdentity) -> Result<()> {
    local_db.set_sync_identity(identity)
}

/// Reject duplicate or conflicting sync mappings before any worker starts.
///
/// Two mappings that resolve to the same canonical local root share the same
/// state database and would double-sync (and mutually corrupt) each other, so
/// they are an error rather than a silent overlap.
///
/// # Errors
///
/// Returns an error if any mapping's local root cannot be resolved, or if two
/// mappings resolve to the same canonical root.
pub fn validate_unique_mappings(paths: &[SyncPath]) -> Result<()> {
    let mut seen: std::collections::HashMap<PathBuf, String> = std::collections::HashMap::new();
    for p in paths {
        let canonical = SyncDirPath::new(&p.local)?.as_path_buf().clone();
        if let Some(prev_remote) = seen.insert(canonical.clone(), p.remote.clone()) {
            anyhow::bail!(
                "Duplicate sync mapping: '{}' (remote '{}') and '{}' (remote '{}') both resolve \
                 to the same local root '{}'. Each sync root must have exactly one mapping; \
                 remove or merge the duplicate.",
                p.remote,
                canonical.display(),
                prev_remote,
                p.local.display(),
                canonical.display()
            );
        }
    }
    Ok(())
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

    // Fail closed on a conflict policy the application does not execute before
    // any sync work begins.
    validate_conflict_policy(&config)?;

    // Load ignore patterns
    let ignore_matcher = load_ignore_patterns(&path, &config)?;

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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    // Validate that the stored state belongs to the current mapping before
    // reusing it (conservatively binding fresh/legacy state).
    let root = resolve_sync_dir(&path)?;
    let identity = sync_identity_for(
        &couchdb,
        &root,
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;
    validate_sync_mapping(&local_db, &identity)?;

    // Run sync (dry-run walks the pipeline without writing)
    let retention = (config.sync.tombstone_retention_secs > 0)
        .then(|| std::time::Duration::from_secs(config.sync.tombstone_retention_secs));
    let orphan_grace = (config.sync.orphan_chunk_grace_secs > 0)
        .then(|| std::time::Duration::from_secs(config.sync.orphan_chunk_grace_secs));
    let mut engine = SyncEngine::with_ignore(couchdb, local_db, root, ignore_matcher)
        .with_tombstone_retention(retention)
        .with_orphan_chunk_collection(
            orphan_grace,
            u64::try_from(config.sync.batch_size).unwrap_or(1000),
        )
        .with_max_file_size(size_limit(&config))
        .with_apply_settings(
            config.sync.parallel,
            config.sync.max_parallel,
            config.sync.batch_size,
        );
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

        // Send conflict notifications over each configured channel, tracking
        // delivery per channel/root/generation.
        if report.conflicts > 0 {
            if let Some(notifier) = telegram_conflict_notifier(&config) {
                notify_conflicts(
                    "telegram",
                    &notifier,
                    &config,
                    &db_path,
                    &path,
                    NOTIFICATION_DELIVERY_TIMEOUT,
                )
                .await;
            }
            if let Some(notifier) = matrix_conflict_notifier(&config) {
                notify_conflicts(
                    "matrix",
                    &notifier,
                    &config,
                    &db_path,
                    &path,
                    NOTIFICATION_DELIVERY_TIMEOUT,
                )
                .await;
            }
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

    validate_conflict_policy(&config)?;

    let ignore_matcher = load_ignore_patterns(&path, &config)?;
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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    // Rebuild is the explicit decision to reset state for the current mapping.
    let root = resolve_sync_dir(&path)?;
    let identity = sync_identity_for(
        &couchdb,
        &root,
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;
    bind_sync_mapping(&local_db, &identity)?;

    let mut engine = SyncEngine::with_ignore(couchdb, local_db, root, ignore_matcher)
        .with_max_file_size(size_limit(&config));
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

    validate_conflict_policy(&config)?;

    let ignore_matcher = load_ignore_patterns(&path, &config)?;
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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    // Rebuild is the explicit decision to reset state for the current mapping.
    let root = resolve_sync_dir(&path)?;
    let identity = sync_identity_for(
        &couchdb,
        &root,
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;
    bind_sync_mapping(&local_db, &identity)?;

    let mut engine = SyncEngine::with_ignore(couchdb, local_db, root, ignore_matcher)
        .with_max_file_size(size_limit(&config));
    let report = engine.rebuild_local_from_remote().await?;
    print_sync_report(&report, false);

    Ok(report)
}

/// Upper bound on the whole delivery attempt for one channel. If a notifier
/// hangs (its HTTP transport should already time out after
/// `NOTIFICATION_HTTP_TIMEOUT`), this backstop lets sync continue instead of
/// blocking indefinitely.
const NOTIFICATION_DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Send conflict notifications over one channel, tracking delivery per
/// channel/root/generation.
///
/// Each opted-in channel receives each brand-new conflict exactly once per
/// root/generation; a failed or timed-out channel remains retryable without
/// duplicating channels that already succeeded; and `notify_on_conflict`
/// disabled means nothing is sent. Delivery state is persisted in the per-root
/// local DB, so it survives daemon restarts and separates roots.
async fn notify_conflicts(
    channel: &str,
    notifier: &dyn ConflictNotifier,
    config: &AppConfig,
    db_path: &Path,
    sync_dir: &Path,
    delivery_timeout: Duration,
) {
    if !config.notifications.notify_on_conflict {
        info!("Conflict notifications disabled; skipping {channel}");
        return;
    }

    // Re-open database to get conflicts and delivery state.
    let local_db = match LocalDb::open(db_path) {
        Ok(db) => db,
        Err(e) => {
            warn!("Failed to open database for {channel} notifications: {}", e);
            return;
        }
    };

    let delivered = match local_db.delivered_conflicts(channel) {
        Ok(set) => set,
        Err(e) => {
            warn!("Failed to read {channel} delivery state: {}", e);
            return;
        }
    };

    let conflicts = match local_db.get_conflicts() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to get conflicts for {channel} notification: {}", e);
            return;
        }
    };

    // Only brand-new conflicts (not yet delivered on this channel for this
    // root/generation) are sent.
    let new_conflicts: Vec<&Conflict> = conflicts
        .iter()
        .filter(|c| !delivered.contains(&(c.path.clone(), conflict_generation(c))))
        .collect();

    if new_conflicts.is_empty() {
        return;
    }

    let sync_dir_str = sync_dir.display().to_string();
    let attempt = tokio::time::timeout(
        delivery_timeout,
        notifier.notify_new_conflicts(&new_conflicts, &sync_dir_str),
    )
    .await;

    match attempt {
        Ok(Ok(())) => {
            info!(
                "Sent {channel} notification for {} new conflict(s)",
                new_conflicts.len()
            );
            // Mark only this channel's deliveries, so a failed sibling channel
            // is never suppressed and retries on the next cycle.
            for conflict in &new_conflicts {
                if let Err(e) = local_db.mark_conflict_delivered(
                    channel,
                    &conflict.path,
                    &conflict_generation(conflict),
                ) {
                    warn!("Failed to mark {channel} conflict as delivered: {}", e);
                }
            }
        }
        Ok(Err(e)) => {
            warn!("Failed to send {channel} notification: {}", e);
        }
        Err(_elapsed) => {
            warn!(
                "{channel} notification timed out after {}s; sync continues",
                NOTIFICATION_DELIVERY_TIMEOUT.as_secs()
            );
        }
    }
}

/// Build a Telegram conflict notifier if Telegram is configured.
fn telegram_conflict_notifier(config: &AppConfig) -> Option<TelegramNotifier> {
    let (bot_token, chat_id) = match &config.notifications.telegram.credentials {
        Some(creds) if !creds.bot_token.is_empty() && !creds.chat_id.is_empty() => {
            (creds.bot_token.clone(), creds.chat_id.clone())
        }
        _ => {
            info!("Telegram not configured, skipping conflict notifications");
            return None;
        }
    };
    Some(TelegramNotifier::new(bot_token, chat_id))
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

/// Build a Matrix conflict notifier if Matrix is configured.
fn matrix_conflict_notifier(config: &AppConfig) -> Option<MatrixNotifier> {
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
    // Reject duplicate/conflicting mappings before any worker starts: two
    // mappings that share a canonical root would double-sync the same state DB.
    validate_unique_mappings(&paths)?;

    // Fail closed on an unsupported conflict policy before any worker starts.
    validate_conflict_policy(&config)?;

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

        // One supervised live-sync worker per path. Each worker owns its
        // producers and restarts with a bounded budget; a worker only finishes
        // when its path is complete or the restart budget is exhausted (Err).
        let mut workers = tokio::task::JoinSet::new();
        for sync_path in paths {
            let mut path_config = config.clone();
            path_config
                .couchdb
                .remote_path
                .clone_from(&sync_path.remote);
            let local_path = sync_path.local.clone();
            workers.spawn(supervise_live_worker(local_path, path_config));
        }

        // Run until Ctrl-C or a path worker gives up (bounded restart budget
        // exhausted). A worker's death must not leave the daemon silently
        // running with that path unmonitored.
        let shutdown = tokio::signal::ctrl_c();
        tokio::pin!(shutdown);
        let mut result = Ok(());
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!("Ctrl-C received; shutting down live sync workers");
                    break;
                }
                joined = workers.join_next() => {
                    match joined {
                        None => break, // all workers finished
                        Some(Ok(Ok(()))) => {
                            // A path worker completed cleanly; keep the daemon
                            // running for the remaining paths.
                            info!("A live sync worker completed cleanly");
                        }
                        Some(Ok(Err(e))) => {
                            error!("Live sync worker failed after restart budget: {e}");
                            result = Err(e);
                            break;
                        }
                        Some(Err(join_err)) => {
                            error!("Live sync worker task failed: {join_err}");
                            result = Err(anyhow::anyhow!(
                                "live sync worker task failed: {join_err}"
                            ));
                            break;
                        }
                    }
                }
            }
        }

        // Cancel and join all owned workers so none is left running on shutdown.
        workers.shutdown().await;
        return result;
    }

    println!("CouchDB File Sync daemon started (interval: {interval}s)");
    println!("Syncing {} path(s): {}", paths.len(), path_list.join(", "));
    println!("Press Ctrl+C to stop");

    let mut interval_timer = tokio::time::interval(tokio::time::Duration::from_secs(interval));

    loop {
        interval_timer.tick().await;

        for sync_path in &paths {
            let mut path_config = config.clone();
            path_config
                .couchdb
                .remote_path
                .clone_from(&sync_path.remote);

            match daemon_sync(&sync_path.local, &path_config).await {
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

/// Internal sync function for daemon. Conflict delivery state is persisted per
/// channel/root/generation in the local DB, so the daemon needs no in-memory
/// session set.
async fn daemon_sync(path: &Path, config: &AppConfig) -> Result<SyncReport> {
    info!("Running sync in: {}", path.display());

    // Load ignore patterns
    let ignore_matcher = load_ignore_patterns(path, config)?;

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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    // Validate that the stored state belongs to the current mapping before
    // reusing it (conservatively binding fresh/legacy state).
    let root = resolve_sync_dir(path)?;
    let identity = sync_identity_for(
        &couchdb,
        &root,
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;
    validate_sync_mapping(&local_db, &identity)?;

    // Run sync
    let retention = (config.sync.tombstone_retention_secs > 0)
        .then(|| std::time::Duration::from_secs(config.sync.tombstone_retention_secs));
    let orphan_grace = (config.sync.orphan_chunk_grace_secs > 0)
        .then(|| std::time::Duration::from_secs(config.sync.orphan_chunk_grace_secs));
    let mut engine = SyncEngine::with_ignore(couchdb, local_db, root, ignore_matcher)
        .with_tombstone_retention(retention)
        .with_orphan_chunk_collection(
            orphan_grace,
            u64::try_from(config.sync.batch_size).unwrap_or(1000),
        )
        .with_max_file_size(size_limit(config))
        .with_apply_settings(
            config.sync.parallel,
            config.sync.max_parallel,
            config.sync.batch_size,
        );
    let report = engine.sync().await?;

    print_sync_report(&report, false);

    if !report.errors.is_empty() {
        let summary = format_sync_error_summary(&report.errors);
        notify_sync_error_telegram(config, Some(path), &summary).await;
        notify_sync_error_matrix(config, Some(path), &summary).await;
    }

    // Send conflict notifications over each configured channel, tracking
    // delivery per channel/root/generation.
    if report.conflicts > 0 {
        if let Some(notifier) = telegram_conflict_notifier(config) {
            notify_conflicts(
                "telegram",
                &notifier,
                config,
                &db_path,
                path,
                NOTIFICATION_DELIVERY_TIMEOUT,
            )
            .await;
        }
        if let Some(notifier) = matrix_conflict_notifier(config) {
            notify_conflicts(
                "matrix",
                &notifier,
                config,
                &db_path,
                path,
                NOTIFICATION_DELIVERY_TIMEOUT,
            )
            .await;
        }
    }

    Ok(report)
}

/// Expected on-disk state for a path that the tool itself just installed,
/// used to suppress the watcher's echo of that write without hiding a
/// distinct user edit that lands in the same moment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpectedState {
    /// The tool successfully deleted the file. Only a matching delete echo is
    /// suppressed; any recreated content (a real user edit) must propagate.
    Absent,
    /// The tool successfully wrote this exact content hash. Only a local
    /// event whose on-disk bytes match is suppressed; different bytes are a
    /// real edit and must sync.
    Content(String),
}

struct TouchTracker {
    entries: Vec<(String, ExpectedState)>,
}

impl TouchTracker {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Record that the tool successfully installed `hash` at `path`. Only the
    /// latest identity per path is kept so a fresh write supersedes an older
    /// marker.
    fn mark_content(&mut self, path: &str, hash: String) {
        self.mark(path, ExpectedState::Content(hash));
    }

    /// Record that the tool successfully removed `path`.
    fn mark_absent(&mut self, path: &str) {
        self.mark(path, ExpectedState::Absent);
    }

    fn mark(&mut self, path: &str, state: ExpectedState) {
        self.entries.retain(|(p, _)| p != path);
        self.entries.push((path.to_string(), state));
        if self.entries.len() > 50 {
            let drain = self.entries.len() - 50;
            self.entries.drain(0..drain);
        }
    }

    /// Whether the tool may have just written `path`, so a caller knows a
    /// content hash is worth computing before asking `is_touched`.
    fn contains(&self, path: &str) -> bool {
        self.entries.iter().any(|(p, _)| p == path)
    }

    /// True when `path`'s current on-disk state matches the identity the tool
    /// itself installed, i.e. this local change is an echo of our own write
    /// rather than a distinct user edit.
    fn is_touched(&self, path: &str, change_type: ChangeType, current_hash: Option<&str>) -> bool {
        let Some((_, state)) = self.entries.iter().find(|(p, _)| p == path) else {
            return false;
        };
        match state {
            ExpectedState::Absent => change_type == ChangeType::Deleted,
            ExpectedState::Content(written) => {
                change_type != ChangeType::Deleted && current_hash == Some(written.as_str())
            }
        }
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

/// Bounded reconciliation state for the live sync loop.
///
/// Local changes can fail to apply (transient upload errors) or be lost when
/// the file watcher's internal event channel overflows (a burst larger than
/// its capacity). Neither case must strand a local edit until the next user
/// edit or daemon restart, so the live loop coalesces the affected paths into
/// `dirty` (and sets `full_rescan` when the watcher dropped events of unknown
/// scope) and re-runs a full sync cycle as reconciliation. The rescan is
/// bounded: it only runs when something is pending, and never more often than
/// `interval`.
struct Reconcile {
    /// Coalesced paths whose local apply failed and must be retried.
    dirty: HashSet<String>,
    /// Set when the watcher overflowed and dropped events of unknown scope;
    /// forces a full reconciliation rescan rather than a path-scoped retry.
    full_rescan: bool,
    /// When the last reconciliation pass ran (cooldown for boundedness).
    last: Instant,
    /// Minimum time between reconciliation passes.
    interval: Duration,
}

impl Reconcile {
    fn new(interval: Duration) -> Self {
        Self {
            dirty: HashSet::new(),
            full_rescan: false,
            last: Instant::now(),
            interval,
        }
    }

    /// Record that `path` failed to apply and must be retried.
    fn mark_dirty(&mut self, path: &str) {
        self.dirty.insert(path.to_string());
    }

    /// Record that the watcher overflowed and dropped events of unknown scope.
    const fn mark_full_rescan(&mut self) {
        self.full_rescan = true;
    }

    /// Whether any reconciliation work is pending.
    // `HashSet::is_empty` is not `const`, so this cannot be a `const fn`
    // despite clippy's `missing_const_for_fn` suggestion.
    #[allow(clippy::missing_const_for_fn)]
    fn pending(&self) -> bool {
        self.full_rescan || !self.dirty.is_empty()
    }

    /// Whether a reconciliation pass may run now (bounded by `interval`).
    fn due(&self) -> bool {
        self.last.elapsed() >= self.interval
    }

    /// Clear pending state after a successful reconciliation pass.
    fn reset(&mut self) {
        self.dirty.clear();
        self.full_rescan = false;
        self.last = Instant::now();
    }
}

/// Apply one remote changes-feed entry under ordered application
/// acknowledgement: apply the change, advance the durable cursor only when the
/// contiguous prefix is fully applied, and report the cursor back to the
/// producer. Returns `true` when the live loop should stop (the producer is
/// gone).
#[allow(clippy::too_many_arguments)] // single cohesive per-entry apply; bundled state would be opaque
async fn apply_live_remote_entry(
    engine: &mut SyncEngine,
    touched: &mut TouchTracker,
    ignore_matcher: &IgnoreMatcher,
    entry: ChangeFeedEntry,
    remote_cursor: &mut RemoteFeedCursor,
    ack_tx: &mpsc::Sender<String>,
    config: &AppConfig,
    path: &Path,
) -> Result<bool> {
    let seq = entry.seq.clone();
    match handle_remote_change(engine, touched, ignore_matcher, entry).await {
        Ok(()) => {
            // A failed persistence leaves the durable cursor behind (safe:
            // nothing is skipped); a restart re-applies the entry idempotently.
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
                config,
                Some(path),
                &format!("Live remote change error: {e}"),
            )
            .await;
            notify_sync_error_matrix(
                config,
                Some(path),
                &format!("Live remote change error: {e}"),
            )
            .await;
        }
    }
    // Acknowledge the durable cursor back to the producer after every entry
    // (success or failure) so the feed resumes from the highest
    // contiguously-applied sequence.
    if ack_tx
        .send(remote_cursor.checkpoint().to_string())
        .await
        .is_err()
    {
        return Ok(true);
    }
    Ok(false)
}

/// Recover from an invalid remote resume cursor by running a full
/// reconciliation and reporting the fresh checkpoint back to the producer, so
/// the feed resumes from a known-good position instead of jumping to "now".
/// Returns `true` when the live loop should stop (the producer is gone).
async fn reconcile_live_remote_cursor(
    engine: &mut SyncEngine,
    ack_tx: &mpsc::Sender<String>,
    remote_cursor: &mut RemoteFeedCursor,
) -> Result<bool> {
    if let Err(e) = engine.sync().await {
        warn!("Live reconciliation failed: {}", e);
    }
    let seq = engine
        .get_checkpoint()
        .await
        .ok()
        .flatten()
        .map_or_else(|| remote_cursor.checkpoint().to_string(), |cp| cp.last_seq);
    *remote_cursor = RemoteFeedCursor::new(seq.clone());
    if ack_tx.send(seq).await.is_err() {
        return Ok(true);
    }
    Ok(false)
}

/// Apply one local watcher change; a transient apply failure is coalesced into
/// the bounded reconciliation set so it is retried on the rescan cadence.
async fn apply_live_local_change(
    engine: &mut SyncEngine,
    reconcile: Option<&mut Reconcile>,
    touched: &TouchTracker,
    change: &Change,
    config: &AppConfig,
    path: &Path,
) {
    if let Err(e) = handle_local_change(engine, reconcile, touched, change).await {
        warn!("Live local change error for {}: {}", change.path(), e);
        notify_sync_error_telegram(
            config,
            Some(path),
            &format!("Live local change error for {}: {}", change.path(), e),
        )
        .await;
        notify_sync_error_matrix(
            config,
            Some(path),
            &format!("Live local change error for {}: {}", change.path(), e),
        )
        .await;
    }
}

/// Run a bounded reconciliation rescan when pending local work exists and the
/// cooldown has elapsed. A full sync cycle re-scans the filesystem and retries
/// every failed/dropped local change; it also heals remote divergence.
async fn attempt_live_reconciliation(
    engine: &mut SyncEngine,
    reconcile: Option<&mut Reconcile>,
    config: &AppConfig,
    path: &Path,
    ignore_policy: &Arc<RwLock<IgnoreMatcher>>,
) {
    let Some(r) = reconcile else { return };
    if !(r.pending() && r.due()) {
        return;
    }
    // Refresh the engine from the shared policy so a live ignore-file change
    // (reloaded by the watcher into `ignore_policy`) is applied before this
    // reconciliation rescan re-evaluates the tree with the new rules. Clone
    // the matcher out of the lock so no read guard is held across the await.
    let current_policy = if let Ok(guard) = ignore_policy.read() {
        guard.clone()
    } else {
        warn!("Ignore policy lock poisoned; skipping reconciliation");
        return;
    };
    engine.reload_ignore(current_policy);
    match engine.sync().await {
        Ok(report) => {
            if !report.errors.is_empty() {
                // Some changes still failed; surface them and retry on the
                // next cadence.
                let summary = format_sync_error_summary(&report.errors);
                warn!(
                    "Live reconciliation left {} error(s): {}",
                    report.errors.len(),
                    summary
                );
                notify_sync_error_telegram(config, Some(path), &summary).await;
                notify_sync_error_matrix(config, Some(path), &summary).await;
            }
            r.reset();
        }
        Err(e) => {
            // Fatal sync failure: notify the supervisor and back off before the
            // next retry.
            warn!("Live reconciliation failed: {}", e);
            notify_sync_error_telegram(
                config,
                Some(path),
                &format!("Live reconciliation failed: {e}"),
            )
            .await;
            notify_sync_error_matrix(
                config,
                Some(path),
                &format!("Live reconciliation failed: {e}"),
            )
            .await;
            r.last = Instant::now();
        }
    }
}

/// Maximum consecutive failures before a live sync worker gives up and the
/// daemon exits nonzero.
const MAX_LIVE_WORKER_RESTARTS: u32 = 3;
/// Backoff between live sync worker restarts.
const LIVE_WORKER_RESTART_BACKOFF: Duration = Duration::from_secs(5);

/// Run one path's live sync worker with a bounded restart budget. A source
/// exit (clean `Ok` return, e.g. the feed closed) or a failure (`Err`) both
/// restart the worker; exhausting the budget surfaces the error so the daemon
/// exits nonzero instead of silently dropping the path.
async fn supervise_live_worker(path: PathBuf, config: AppConfig) -> Result<()> {
    run_with_restart_budget(
        {
            let path = path.clone();
            let config = config.clone();
            move || {
                let path = path.clone();
                let config = config.clone();
                async move {
                    let outcome = live_sync_path(path.clone(), config.clone()).await;
                    let message = match &outcome {
                        Ok(()) => format!("Live sync source for {} exited", path.display()),
                        Err(e) => format!("Live sync error for {}: {e}", path.display()),
                    };
                    error!("{message}");
                    notify_sync_error_telegram(&config, Some(&path), &message).await;
                    notify_sync_error_matrix(&config, Some(&path), &message).await;
                    outcome
                }
            }
        },
        "live sync worker",
        MAX_LIVE_WORKER_RESTARTS,
        LIVE_WORKER_RESTART_BACKOFF,
    )
    .await
}

/// Run `worker` to completion with a bounded restart budget. A clean source
/// exit (`Ok`) and a failure (`Err`) both restart the worker; exhausting the
/// budget surfaces an error so the daemon exits nonzero. `label` names the
/// worker for logging; `max_attempts` and `backoff` bound the restart policy
/// (split out for direct testing).
async fn run_with_restart_budget<F, Fut>(
    mut worker: F,
    label: &str,
    max_attempts: u32,
    backoff: Duration,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut attempts: u32 = 0;
    loop {
        let outcome = worker().await;
        attempts += 1;
        if attempts >= max_attempts {
            return Err(match outcome {
                Ok(()) => anyhow::anyhow!("{label} exited cleanly {attempts} times; giving up"),
                Err(e) => e,
            });
        }
        info!("{label} restarted (attempt {attempts}/{max_attempts}); backing off {backoff:?}");
        tokio::time::sleep(backoff).await;
    }
}

async fn live_sync_path(path: PathBuf, config: AppConfig) -> Result<()> {
    info!("Starting live sync in: {}", path.display());

    let (ignore_policy, mut engine) = build_live_sync_engine(&path, &config).await?;

    // Watcher coverage BEFORE bootstrap: start the local watcher first so any
    // edit made during the bootstrap sync (the bootstrap-to-watch transition)
    // is captured by the watcher rather than silently missed. The remote feed
    // starts only after bootstrap, which fixes the resume cursor.
    let (local_tx, local_rx) = mpsc::channel::<Change>(256);
    let (overflow_tx, overflow_rx) = mpsc::channel::<()>(1);
    let local_handle = spawn_local_watcher_source(
        config.clone(),
        &resolve_sync_dir(&path)?,
        ignore_policy.clone(),
        local_tx,
        overflow_tx,
    );

    let initial_since = bootstrap_live_sync(&mut engine).await?;

    // The consumer acknowledges each durably-applied sequence back to the
    // producer (ordered ack), and can be asked to run a full reconciliation
    // when the producer hits an invalid resume cursor.
    let (remote_tx, remote_rx) = mpsc::channel::<ChangeFeedEntry>(256);
    let (ack_tx, ack_rx) = mpsc::channel::<String>(256);
    let (reconcile_tx, reconcile_rx) = mpsc::channel::<()>(1);
    let remote_handle = spawn_remote_changes_source(
        config.clone(),
        &path,
        initial_since.clone(),
        remote_tx,
        ack_rx,
        reconcile_tx,
    );

    // Guard: on ANY exit (including cancellation of this worker by the daemon
    // supervisor) abort the owned producers so no detached watcher/feed task
    // leaks; on the normal return path `shutdown` also joins them.
    let mut sources = LiveProducerGuard::new(vec![local_handle, remote_handle]);

    let loop_result = run_live_loop(
        &mut engine,
        &config,
        &path,
        &ignore_policy,
        local_rx,
        remote_rx,
        ack_tx,
        reconcile_rx,
        overflow_rx,
        initial_since,
    )
    .await;

    // Cancel and join all owned producers before returning, so shutdown is
    // complete (the guard still aborts them if this worker is itself cancelled
    // mid-run by the daemon).
    sources.shutdown().await;
    loop_result
}

/// The live sync select-loop core. Owns the bounded reconciliation state and
/// applies events from the local watcher and remote feed. Returns `Err` when a
/// source channel closes (its producer exited) or a fatal error occurs, so the
/// supervisor can apply the restart/failure policy; `Ok(())` only when the
/// remote feed signals completion.
#[allow(clippy::too_many_arguments)]
async fn run_live_loop(
    engine: &mut SyncEngine,
    config: &AppConfig,
    path: &Path,
    ignore_policy: &Arc<RwLock<IgnoreMatcher>>,
    mut local_rx: mpsc::Receiver<Change>,
    mut remote_rx: mpsc::Receiver<ChangeFeedEntry>,
    ack_tx: mpsc::Sender<String>,
    mut reconcile_rx: mpsc::Receiver<()>,
    mut overflow_rx: mpsc::Receiver<()>,
    initial_since: String,
) -> Result<()> {
    let mut touched = TouchTracker::new();
    let mut remote_cursor = RemoteFeedCursor::new(initial_since);

    // Bounded reconciliation: a local apply that fails transiently or a watcher
    // event that is dropped on overflow must still reach remote without another
    // user edit or daemon restart. Pending work is coalesced and retried on the
    // rescan cadence (`0` disables the periodic rescan).
    let mut reconcile = (config.sync.rescan_interval_secs > 0)
        .then(|| Reconcile::new(Duration::from_secs(config.sync.rescan_interval_secs)));
    let mut rescan_timer =
        tokio::time::interval(Duration::from_secs(config.sync.rescan_interval_secs.max(1)));
    // The first `interval` tick fires immediately; consume it so the cooldown
    // drives the cadence rather than running a spurious first pass.
    rescan_timer.tick().await;

    // Reconcile after bootstrap: now that the watcher and feed are both live,
    // run one pass so any change that landed during the bootstrap-to-watch
    // transition converges immediately rather than waiting for the next event.
    if let Some(r) = reconcile.as_mut() {
        r.mark_full_rescan();
        attempt_live_reconciliation(engine, Some(r), config, path, ignore_policy).await;
    } else {
        let _ = engine.sync().await;
    }

    loop {
        tokio::select! {
            change = local_rx.recv() => {
                // A closed channel means its producer (the local watcher)
                // exited; surface that to the supervisor instead of silently
                // disabling the branch and spinning on the remaining sources.
                let Some(change) = change else {
                    return Err(anyhow::anyhow!(
                        "local watcher source for {} closed (source exited)",
                        path.display()
                    ));
                };
                apply_live_local_change(
                    engine,
                    reconcile.as_mut(),
                    &touched,
                    &change,
                    config,
                    path,
                )
                .await;
            }
            entry = remote_rx.recv() => {
                let Some(entry) = entry else {
                    return Err(anyhow::anyhow!(
                        "remote changes source for {} closed (source exited)",
                        path.display()
                    ));
                };
                // Clone the matcher out of the lock so the read guard is not
                // held across the await (RwLockReadGuard is not Send).
                let current_policy = ignore_policy
                    .read()
                    .map_err(|_| anyhow::anyhow!("ignore policy lock poisoned"))?
                    .clone();
                if apply_live_remote_entry(
                    engine,
                    &mut touched,
                    &current_policy,
                    entry,
                    &mut remote_cursor,
                    &ack_tx,
                    config,
                    path,
                )
                .await?
                {
                    return Ok(());
                }
            }
            rec = reconcile_rx.recv() => {
                let Some(()) = rec else {
                    return Err(anyhow::anyhow!(
                        "remote reconciliation source for {} closed (source exited)",
                        path.display()
                    ));
                };
                // The producer hit an invalid resume cursor; reconcile from a
                // known-good position instead of jumping to "now".
                if reconcile_live_remote_cursor(engine, &ack_tx, &mut remote_cursor).await? {
                    return Ok(());
                }
            }
            ov = overflow_rx.recv() => {
                let Some(()) = ov else {
                    return Err(anyhow::anyhow!(
                        "watcher overflow source for {} closed (source exited)",
                        path.display()
                    ));
                };
                if let Some(r) = reconcile.as_mut() {
                    r.mark_full_rescan();
                }
            }
            _ = rescan_timer.tick(), if reconcile.is_some() => {
                // Periodic cadence tick; reconciliation attempt below.
            }
        }

        // Attempt a bounded reconciliation whenever work is pending and the
        // cooldown has elapsed (runs after any event, and on each timer tick).
        attempt_live_reconciliation(engine, reconcile.as_mut(), config, path, ignore_policy).await;
    }
}

/// Effective per-file size limit from config: `0` (or unset default) means
/// unlimited, any positive value bounds the largest file sync will transfer.
fn size_limit(config: &AppConfig) -> Option<u64> {
    (config.sync.max_file_size > 0).then_some(config.sync.max_file_size)
}

async fn build_live_sync_engine(
    path: &Path,
    config: &AppConfig,
) -> Result<(Arc<RwLock<IgnoreMatcher>>, SyncEngine)> {
    // The live policy is shared and reloadable: the watcher rebuilds it when an
    // ignore file changes and the reconciliation loop refreshes the engine from
    // it before each rescan.
    let ignore_policy = Arc::new(RwLock::new(load_ignore_patterns(path, config)?));

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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    let sync_dir = resolve_sync_dir(path)?;
    let identity = sync_identity_for(
        &couchdb,
        &sync_dir,
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;
    validate_sync_mapping(&local_db, &identity)?;

    let initial_policy = ignore_policy
        .read()
        .map_err(|_| anyhow::anyhow!("ignore policy lock poisoned"))?
        .clone();
    let engine = SyncEngine::with_ignore(couchdb, local_db, sync_dir, initial_policy)
        .with_tombstone_retention(
            (config.sync.tombstone_retention_secs > 0)
                .then(|| std::time::Duration::from_secs(config.sync.tombstone_retention_secs)),
        )
        .with_orphan_chunk_collection(
            (config.sync.orphan_chunk_grace_secs > 0)
                .then(|| std::time::Duration::from_secs(config.sync.orphan_chunk_grace_secs)),
            u64::try_from(config.sync.batch_size).unwrap_or(1000),
        )
        .with_max_file_size(size_limit(config))
        .with_apply_settings(
            config.sync.parallel,
            config.sync.max_parallel,
            config.sync.batch_size,
        );

    Ok((ignore_policy, engine))
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

/// Owns the producer tasks (local watcher, remote changes feed) spawned by a
/// live-sync worker. `shutdown` cancels and joins them on the normal exit
/// path; `Drop` aborts them if the owning worker is itself cancelled (e.g.
/// daemon Ctrl-C), so no detached producer is ever left running.
struct LiveProducerGuard {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl LiveProducerGuard {
    const fn new(handles: Vec<tokio::task::JoinHandle<()>>) -> Self {
        Self { handles }
    }

    /// Cancel and join all owned producers.
    async fn shutdown(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
    }
}

impl Drop for LiveProducerGuard {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// Start the local filesystem watcher producer. Returns a handle to the owned
/// task. A watcher startup failure ends the task and closes its channel, which
/// the live loop surfaces so the supervisor can apply the restart/failure
/// policy instead of silently continuing without coverage.
fn spawn_local_watcher_source(
    config: AppConfig,
    root: &SyncDirPath,
    ignore_policy: Arc<RwLock<IgnoreMatcher>>,
    local_tx: mpsc::Sender<Change>,
    overflow_tx: mpsc::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let watcher_root = root.clone();
    let watcher_policy = ignore_policy;
    let debounce_ms = config.sync.debounce_ms;
    let watcher_config = config;
    // On an ignore-file edit the watcher rebuilds the full policy from the
    // config + root so live rule changes take effect and trigger a rescan.
    let reload_root = watcher_root.clone();
    let reload_config = watcher_config.clone();
    let reload: Arc<dyn Fn() -> anyhow::Result<IgnoreMatcher> + Send + Sync> =
        Arc::new(move || load_ignore_patterns(reload_root.as_path(), &reload_config));
    let ignore_files = policy_ignore_files(&watcher_config.ignore);
    tokio::spawn(async move {
        if let Err(e) = run_local_watcher(
            watcher_root.clone(),
            watcher_policy,
            ignore_files,
            reload,
            debounce_ms,
            local_tx,
            overflow_tx,
        )
        .await
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
    })
}

/// Start the remote `CouchDB` changes-feed producer, resuming from `since`.
/// Returns a handle to the owned task; the feed runs until the consumer is
/// gone.
fn spawn_remote_changes_source(
    config: AppConfig,
    path: &Path,
    since: String,
    remote_tx: mpsc::Sender<ChangeFeedEntry>,
    ack_rx: mpsc::Receiver<String>,
    reconcile_tx: mpsc::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let remote_config = config;
    let remote_config_notify = remote_config.clone();
    let remote_root = path.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) =
            run_remote_changes(remote_config, since, remote_tx, ack_rx, reconcile_tx).await
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
    })
}

async fn run_local_watcher(
    root: SyncDirPath,
    ignore_policy: Arc<RwLock<IgnoreMatcher>>,
    ignore_files: Vec<PathBuf>,
    reload: Arc<dyn Fn() -> anyhow::Result<IgnoreMatcher> + Send + Sync>,
    debounce_ms: u64,
    tx: mpsc::Sender<Change>,
    overflow_tx: mpsc::Sender<()>,
) -> Result<()> {
    let mut watcher = AsyncFileWatcher::start(
        root.clone(),
        ignore_policy,
        ignore_files,
        reload,
        debounce_ms,
        overflow_tx,
    )?;

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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    loop {
        // Budget the live long-poll wait from the configured timeout so it
        // works with short timeouts, and let `get_changes_feed` leave
        // transport headroom below the request client timeout.
        let wait_budget_ms = couchdb.timeout_seconds().saturating_mul(1000);
        match couchdb
            .get_changes_feed(&since, ChangesFeed::LongPoll, wait_budget_ms)
            .await
        {
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
    reconcile: Option<&mut Reconcile>,
    touched: &TouchTracker,
    change: &Change,
) -> Result<()> {
    // A directory-scoped watcher event (a rename/move-in/move-out of a
    // populated directory) carries only the top-level directory path, never
    // its children. It cannot be applied as a single file — an upload would
    // try to read the directory and fail, and a directory delete is not a
    // tracked file. Route it into the bounded full reconciliation rescan,
    // which expands the event into tracked subtree reconciliation (removing
    // old remote child paths, uploading new ones) and safely orders filesystem
    // type transitions.
    if engine.is_directory_scoped(change).await? {
        if let Some(r) = reconcile {
            r.mark_full_rescan();
        }
        info!(
            "Directory-scoped local change ({}); routing to subtree reconciliation: {}",
            change.change_type(),
            change.path()
        );
        return Ok(());
    }

    // Only hash paths the tool may have just written; ordinary user edits of
    // arbitrary files skip the (potentially expensive) content check.
    let current_hash = if touched.contains(change.path()) {
        match change.change_type() {
            ChangeType::Created | ChangeType::Modified => {
                compute_file_hash(&engine.root_dir().join(change.path())).ok()
            }
            ChangeType::Deleted => None,
        }
    } else {
        None
    };
    if touched.is_touched(change.path(), change.change_type(), current_hash.as_deref()) {
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
                // Coalesce the failed path so the bounded reconciliation rescan
                // retries it and the error reaches the supervisor.
                if let Some(r) = reconcile {
                    r.mark_dirty(change.path());
                }
                return Err(e);
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
                    engine.apply_remote_change(&change).await?;
                    // Only a successful delete leaves an "absent" marker, so a
                    // failed/slow removal never suppresses a later user edit.
                    touched.mark_absent(&local_rel);
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
            apply_live_remote_write(engine, touched, &local_rel, &change).await?;
        }
    }

    Ok(())
}

/// Apply a remote create/modify to the local tree, arbitrating against any
/// concurrent local edit and recording the installed content identity only
/// after a successful write.
async fn apply_live_remote_write(
    engine: &mut SyncEngine,
    touched: &mut TouchTracker,
    local_rel: &str,
    change: &Change,
) -> Result<()> {
    let file_path = engine.root_dir().join(local_rel);
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
    let state = engine.get_file_state(local_rel).await?;
    let apply_remote =
        triage::live_should_apply_remote(change.rev(), state.as_ref(), local_exists, local_mtime);

    if apply_remote {
        engine.apply_remote_change(change).await?;
        // Only a successful write records the installed content as the
        // expected identity, so a failed/slow download never leaves a marker
        // that would swallow a real user edit. The on-disk hash is
        // authoritative: only bytes matching exactly what we wrote are treated
        // as our own echo; same-bucket edits with different bytes (including
        // mtime-preserving writes) sync.
        if let Ok(hash) = compute_file_hash(&file_path) {
            touched.mark_content(local_rel, hash);
        }
    } else {
        // Upload the local version to arbitrate. Carry the observed remote
        // revision as the publication baseline so a further concurrent remote
        // write landing during the upload is rejected (surfaced as an error)
        // instead of silently overwritten.
        let local_change = Change::local_modified(local_rel.to_string(), String::new(), 0)
            .with_expected_rev(change.rev().map_or(ExpectedRemote::Force, |rev| {
                ExpectedRemote::At(rev.to_string())
            }));
        engine.apply_local_change(&local_change).await?;
    }

    Ok(())
}

/// Load the conflict list for a single root without rendering it.
///
/// Shared by the `conflicts` command and the multi-root JSON aggregator in the
/// binary.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or read.
pub fn conflicts_for(path: &Path) -> Result<Vec<Conflict>> {
    let db_path = state_db_path(path);
    let local_db = LocalDb::open(&db_path)?;
    local_db.get_conflicts()
}

/// List conflicts for a sync directory.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or read.
pub fn conflicts(path: &Path, json: bool) -> Result<()> {
    let conflicts = conflicts_for(path)?;

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
#[allow(clippy::too_many_lines)] // large interactive resolution flow; per-conflict UI dominates
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
    .await?
    .with_legacy_chunk_data(config.sync.legacy_chunk_data);

    let root = resolve_sync_dir(&path)?;
    let identity = sync_identity_for(
        &couchdb,
        &root,
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;
    validate_sync_mapping(&local_db, &identity)?;

    let mut engine = SyncEngine::new(couchdb, local_db, root)
        .with_conflict_dir(config.conflicts.conflict_dir.clone());

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
        // Honor the configured `default_strategy` as the prompt's default
        // selection instead of always preselecting the first option.
        let default_strategy: ResolutionStrategy = match config.conflicts.default_strategy {
            ConflictStrategy::KeepLocal => ResolutionStrategy::KeepLocal,
            ConflictStrategy::KeepRemote => ResolutionStrategy::KeepRemote,
            ConflictStrategy::KeepBoth => ResolutionStrategy::KeepBoth,
            ConflictStrategy::Skip => ResolutionStrategy::Skip,
        };
        let default_index = strategies
            .iter()
            .position(|(strategy, _)| *strategy == default_strategy)
            .unwrap_or(0);
        let options: Vec<&str> = strategies.iter().map(|(_, label)| *label).collect();
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("How do you want to resolve this conflict?")
            .items(&options)
            .default(default_index)
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
/// Machine-readable sync status for a single root.
///
/// Exposed so the binary can aggregate per-root results into one JSON document
/// for multi-root output while the single-root `status` command keeps its
/// documented shape.
#[derive(Clone, Debug)]
pub struct StatusSummary {
    pub sync_directory: PathBuf,
    pub tracked_files: usize,
    pub local_files: usize,
    pub pending_conflicts: usize,
    pub last_sync: Option<DateTime<Utc>>,
    pub last_seq: Option<String>,
}

impl StatusSummary {
    /// Render the single-root status document. The shape mirrors the previous
    /// `status --json` output exactly so one-root compatibility is preserved.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sync_directory": self.sync_directory,
            "tracked_files": self.tracked_files,
            "local_files": self.local_files,
            "pending_conflicts": self.pending_conflicts,
            "last_sync": self.last_sync,
        })
    }
}

/// Compute the status summary for a single root without rendering it.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or read.
pub fn status_summary(path: &Path) -> Result<StatusSummary> {
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

    Ok(StatusSummary {
        sync_directory: path.to_path_buf(),
        tracked_files: file_states.len(),
        local_files: file_count,
        pending_conflicts: conflicts.len(),
        last_sync: checkpoint.as_ref().map(|cp| cp.last_sync_at),
        last_seq: checkpoint.as_ref().map(|cp| cp.last_seq.clone()),
    })
}

/// List the sync status for a sync directory.
///
/// # Errors
///
/// Returns an error if the local state DB cannot be opened or read.
pub fn status(path: &Path, json: bool, _config: &AppConfig) -> Result<()> {
    let summary = status_summary(path)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&summary.to_json())?);
    } else {
        println!("CouchDB File Sync Status");
        println!("==============");
        println!("Sync directory: {}", summary.sync_directory.display());
        println!("Local files:    {}", summary.local_files);
        println!("Tracked files:  {}", summary.tracked_files);
        println!("Pending conflicts: {}", summary.pending_conflicts);

        if let Some(cp) = &summary.last_sync {
            println!("Last sync:      {}", cp.format("%Y-%m-%d %H:%M:%S UTC"));
            if let Some(seq) = &summary.last_seq {
                println!("Last sequence:  {seq}");
            }
        } else {
            println!("Last sync:      Never");
        }

        if summary.pending_conflicts > 0 {
            println!();
            println!(
                "⚠️  {} conflict(s) need resolution",
                summary.pending_conflicts
            );
        }
    }

    Ok(())
}

/// Helper to load the effective ignore policy from all configured sources.
///
/// Composes the YAML `ignore.patterns`, each `ignore.ignore_files` entry, and
/// the default `.sync-ignore` file with documented precedence (see
/// [`IgnoreMatcher::from_config`]). Fails closed: an unreadable/malformed
/// explicitly-configured rule aborts the load so mutation planning never runs
/// with a silently-degraded exclusion policy.
///
/// # Errors
///
/// Returns an error if a configured pattern is malformed or a configured
/// ignore file is missing, unreadable, or malformed.
fn load_ignore_patterns(root: &Path, config: &AppConfig) -> anyhow::Result<IgnoreMatcher> {
    let mut matcher = IgnoreMatcher::from_config(root, &config.ignore)?;
    // Preservation copies written under the configured `conflict_dir` must
    // never reenter sync, whether or not the directory is dot-prefixed. Add it
    // to the effective policy so its contents are excluded from every scan.
    if let Some(conflict_dir) = &config.conflicts.conflict_dir {
        let pattern = format!("{}/", conflict_dir.display());
        matcher.extend_from_content_result(&pattern)?;
    }
    info!("Loaded ignore policy from config and ignore files");
    Ok(matcher)
}

/// Fail closed when the configured conflict policy requests behavior the
/// application does not execute. `auto_resolve` is documented in the example
/// config but automatic resolution is not implemented; silently accepting it
/// would imply conflicts are resolved when they never are, so a config that
/// enables it is rejected before any sync runs.
///
/// # Errors
///
/// Returns an error when `conflicts.auto_resolve` is true.
fn validate_conflict_policy(config: &AppConfig) -> anyhow::Result<()> {
    config.conflicts.validate_supported()
}

/// The root-relative ignore files that participate in the effective policy.
/// Used by the live watcher to detect edits that should trigger a policy
/// reload. Always includes `.sync-ignore` (the default, highest-precedence
/// source) alongside any explicitly configured `ignore_files` entries.
fn policy_ignore_files(config: &IgnoreConfig) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = config.ignore_files.iter().map(PathBuf::from).collect();
    if !config.ignore_files.iter().any(|f| f == ".sync-ignore") {
        files.push(PathBuf::from(".sync-ignore"));
    }
    files
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
    use crate::config::{MatrixCredentials, TelegramCredentials};
    use crate::couchdb::db::CannedCouch;
    use crate::models::{DownloadCount, FileDoc, UploadCount};
    use crate::notify::HttpResponse;
    use crate::notify::tests::{FakeSender, HangingSender, make_conflict};
    use tempfile::TempDir;

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

    // ── validate_unique_mappings (duplicate canonical-root rejection) ────

    #[test]
    fn distinct_roots_are_accepted() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let paths = vec![
            SyncPath {
                local: a,
                remote: "notes/".into(),
            },
            SyncPath {
                local: b,
                remote: "photos/".into(),
            },
        ];
        assert!(validate_unique_mappings(&paths).is_ok());
    }

    #[test]
    fn canonical_root_alias_is_rejected_as_duplicate() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("base");
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        // Two spellings that canonicalize to the same root are a duplicate.
        let direct = base.clone();
        let via_sub = base.join("sub").join("..");
        let paths = vec![
            SyncPath {
                local: direct,
                remote: "notes/".into(),
            },
            SyncPath {
                local: via_sub,
                remote: "notes/".into(),
            },
        ];
        let err = validate_unique_mappings(&paths).unwrap_err();
        assert!(
            format!("{err:#}").contains("Duplicate sync mapping"),
            "expected duplicate-mapping error, got: {err:#}"
        );
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

    // ── load_ignore_patterns ──────────────────────────────────────────────

    fn default_config() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn test_load_ignore_patterns_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let matcher = load_ignore_patterns(root, &default_config()).unwrap();
        assert!(matcher.is_empty());
    }

    #[test]
    fn test_load_ignore_patterns_with_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".sync-ignore"), "*.log\n*.tmp\nbuild/\n").unwrap();
        let matcher = load_ignore_patterns(root, &default_config()).unwrap();
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
        let matcher = load_ignore_patterns(root, &default_config()).unwrap();
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
        let matcher = load_ignore_patterns(root, &default_config()).unwrap();
        assert!(matcher.is_empty());
    }

    #[test]
    fn test_load_ignore_patterns_yaml_patterns_applied() {
        // YAML ignore.patterns are loaded even without a .sync-ignore file.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = default_config();
        config.ignore.patterns = vec!["*.log".into(), "secret/".into()];
        let matcher = load_ignore_patterns(root, &config).unwrap();
        assert!(matcher.is_ignored("debug.log", false));
        assert!(matcher.is_ignored("secret/creds.txt", false));
        assert!(!matcher.is_ignored("main.rs", false));
    }

    #[test]
    fn test_load_ignore_patterns_extra_ignore_file_loaded() {
        // An explicitly configured ignore_files entry is read and applied.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("custom.ignore"), "*.tmp\n").unwrap();
        let mut config = default_config();
        config.ignore.ignore_files = vec!["custom.ignore".into()];
        let matcher = load_ignore_patterns(root, &config).unwrap();
        assert!(matcher.is_ignored("file.tmp", false));
        assert!(!matcher.is_ignored("file.log", false));
    }

    #[test]
    fn test_load_ignore_patterns_excludes_conflict_dir_dot_prefix() {
        // A dot-prefixed configured conflict_dir is excluded from sync so its
        // preservation copies never reenter a cycle.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = default_config();
        config.conflicts.conflict_dir = Some(PathBuf::from(".conflicts"));
        let matcher = load_ignore_patterns(root, &config).unwrap();
        assert!(
            matcher.should_ignore(Path::new(".conflicts/report.pdf.remote")),
            "preservation copy under dot-prefixed conflict_dir must be ignored"
        );
        assert!(matcher.should_ignore(Path::new(".conflicts/docs/report.pdf.remote")));
    }

    #[test]
    fn test_load_ignore_patterns_excludes_conflict_dir_non_dot_prefix() {
        // Even a non-dot-prefixed conflict_dir is excluded, so copies cannot
        // reenter sync regardless of the directory name.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = default_config();
        config.conflicts.conflict_dir = Some(PathBuf::from("conflicts"));
        let matcher = load_ignore_patterns(root, &config).unwrap();
        assert!(
            matcher.should_ignore(Path::new("conflicts/report.pdf.remote")),
            "preservation copy under non-dot conflict_dir must be ignored"
        );
        assert!(
            !matcher.should_ignore(Path::new("docs/report.pdf")),
            "unrelated files must not be ignored"
        );
    }

    #[test]
    fn test_validate_conflict_policy_rejects_auto_resolve() {
        let mut config = default_config();
        config.conflicts.auto_resolve = true;
        let err = validate_conflict_policy(&config).unwrap_err();
        assert!(
            err.to_string().contains("auto_resolve is not supported"),
            "error should name the unsupported setting: {err}"
        );
    }

    #[test]
    fn test_validate_conflict_policy_accepts_supported_policies() {
        // Each supported default strategy and a configured conflict_dir pass.
        for strategy in [
            ConflictStrategy::KeepLocal,
            ConflictStrategy::KeepRemote,
            ConflictStrategy::KeepBoth,
            ConflictStrategy::Skip,
        ] {
            let mut config = default_config();
            config.conflicts.default_strategy = strategy;
            config.conflicts.conflict_dir = Some(PathBuf::from(".conflicts"));
            assert!(validate_conflict_policy(&config).is_ok());
        }
    }

    #[test]
    fn test_load_ignore_patterns_precedence_sync_ignore_overrides_yaml() {
        // .sync-ignore (highest precedence, applied last) can re-include a
        // path ignored by the YAML patterns.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".sync-ignore"), "!important.log\n").unwrap();
        let mut config = default_config();
        config.ignore.patterns = vec!["*.log".into()];
        let matcher = load_ignore_patterns(root, &config).unwrap();
        assert!(matcher.is_ignored("debug.log", false));
        assert!(!matcher.is_ignored("important.log", false));
    }

    #[test]
    fn test_load_ignore_patterns_fails_closed_on_invalid_yaml_pattern() {
        // A malformed YAML pattern aborts the load instead of being skipped.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = default_config();
        config.ignore.patterns = vec!["[invalid".into()];
        assert!(load_ignore_patterns(root, &config).is_err());
    }

    #[test]
    fn test_load_ignore_patterns_fails_closed_on_unreadable_ignore_file() {
        // A configured ignore_files entry that is missing aborts the load
        // (fail closed) rather than silently degrading to ignore-nothing.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut config = default_config();
        config.ignore.ignore_files = vec!["missing.ignore".into()];
        assert!(load_ignore_patterns(root, &config).is_err());
    }

    #[test]
    fn test_load_ignore_patterns_fails_closed_on_malformed_ignore_file() {
        // A malformed pattern inside a configured ignore file aborts the load.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("custom.ignore"), "*.tmp\n[bad\n").unwrap();
        let mut config = default_config();
        config.ignore.ignore_files = vec!["custom.ignore".into()];
        assert!(load_ignore_patterns(root, &config).is_err());
    }

    #[test]
    fn test_load_ignore_patterns_absent_sync_ignore_is_optional() {
        // The default .sync-ignore is optional: a missing one is fine, unlike
        // an explicitly configured ignore_files entry.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(load_ignore_patterns(root, &default_config()).is_ok());
    }

    #[test]
    fn test_policy_ignore_files_includes_sync_ignore() {
        let mut config = default_config();
        config.ignore.ignore_files = vec!["a.ignore".into()];
        let files = policy_ignore_files(&config.ignore);
        assert!(files.contains(&PathBuf::from("a.ignore")));
        assert!(files.contains(&PathBuf::from(".sync-ignore")));
    }

    // ── TouchTracker (content-identity echo suppression) ──────────────────

    #[test]
    fn touch_tracker_suppresses_matching_content_echo() {
        let mut t = TouchTracker::new();
        t.mark_content("a.txt", "hash-AAA".to_string());
        // The watcher's echo of our own write carries the same bytes.
        assert!(t.is_touched("a.txt", ChangeType::Modified, Some("hash-AAA")));
        assert!(t.is_touched("a.txt", ChangeType::Created, Some("hash-AAA")));
        // A different path is never suppressed.
        assert!(!t.is_touched("b.txt", ChangeType::Modified, Some("hash-AAA")));
    }

    #[test]
    fn touch_tracker_propagates_different_bytes_in_same_moment() {
        let mut t = TouchTracker::new();
        t.mark_content("a.txt", "hash-AAA".to_string());
        // A real user edit lands with different bytes; even an instant later it
        // must not be suppressed.
        assert!(!t.is_touched("a.txt", ChangeType::Modified, Some("hash-BBB")));
        // A deleted-echo check with content still present is not ours.
        assert!(!t.is_touched("a.txt", ChangeType::Deleted, None));
    }

    #[test]
    fn touch_tracker_propagates_mtime_preserving_edit() {
        let mut t = TouchTracker::new();
        t.mark_content("a.txt", "hash-AAA".to_string());
        // A tool like `touch -r`/`cp -p` rewrites content while preserving the
        // mtime; the differing bytes are a genuine edit and must sync.
        assert!(!t.is_touched("a.txt", ChangeType::Modified, Some("hash-NEW")));
    }

    #[test]
    fn touch_tracker_absent_suppresses_delete_echo_but_not_recreate() {
        let mut t = TouchTracker::new();
        t.mark_absent("gone.txt");
        // The delete echo of our own removal is suppressed.
        assert!(t.is_touched("gone.txt", ChangeType::Deleted, None));
        // Recreating the file is a distinct user action and must propagate.
        assert!(!t.is_touched("gone.txt", ChangeType::Created, Some("hash-NEW")));
        assert!(!t.is_touched("gone.txt", ChangeType::Modified, Some("hash-NEW")));
    }

    #[test]
    fn touch_tracker_unmarked_path_never_suppresses() {
        // Simulates a failed/slow write that left no marker: no identity was
        // recorded, so a subsequent user edit is never swallowed.
        let t = TouchTracker::new();
        assert!(!t.is_touched("a.txt", ChangeType::Modified, Some("hash-AAA")));
        assert!(!t.is_touched("a.txt", ChangeType::Deleted, None));
    }

    #[test]
    fn touch_tracker_latest_write_supersedes_older() {
        let mut t = TouchTracker::new();
        t.mark_content("a.txt", "hash-OLD".to_string());
        // A newer install of different content replaces the prior identity.
        t.mark_content("a.txt", "hash-NEW".to_string());
        assert!(!t.is_touched("a.txt", ChangeType::Modified, Some("hash-OLD")));
        assert!(t.is_touched("a.txt", ChangeType::Modified, Some("hash-NEW")));
    }

    #[test]
    fn touch_tracker_is_bounded() {
        let mut t = TouchTracker::new();
        for i in 0..80 {
            t.mark_content(&format!("f{i}.txt"), format!("hash-{i}"));
        }
        assert!(t.entries.len() <= 50);
        // The oldest entries are evicted; the newest 50 remain effective.
        assert!(t.is_touched("f79.txt", ChangeType::Modified, Some("hash-79")));
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

    // ── Live reconciliation after transient failure / watcher overflow (#6347) ─

    /// Build a live engine against a canned `CouchDB` that fails the first
    /// `save_file`/`delete_file` for the given remote path(s), so the live apply
    /// fails transiently but a reconciliation retry succeeds.
    fn live_engine_with_transient_failures(
        local: LocalDb,
        root: SyncDirPath,
        save_once: Vec<String>,
        delete_once: Vec<String>,
    ) -> SyncEngine {
        use crate::couchdb::db::CannedCouch;
        let canned = CannedCouch {
            save_errors_once: Arc::new(std::sync::Mutex::new(HashSet::from_iter(save_once))),
            delete_errors_once: Arc::new(std::sync::Mutex::new(HashSet::from_iter(delete_once))),
            ..Default::default()
        };
        SyncEngine::new(
            CouchDb::for_test_with_canned("prefix/", canned),
            local,
            root,
        )
    }

    #[tokio::test]
    async fn live_transient_create_failure_marks_dirty_and_converges() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("new.txt"), "hello").unwrap();

        let mut engine = live_engine_with_transient_failures(
            LocalDb::open_in_memory().expect("in-memory LocalDb"),
            root.clone(),
            vec!["prefix/new.txt".to_string()],
            Vec::new(),
        );
        let mut reconcile = Reconcile::new(Duration::from_secs(30));
        let touched = TouchTracker::new();
        let change = Change::local_created("new.txt".to_string(), String::new(), 0);

        // The live apply fails transiently; the path must be queued for
        // reconciliation and the error must reach the caller (supervisor).
        handle_local_change(&mut engine, Some(&mut reconcile), &touched, &change)
            .await
            .expect_err("transient upload failure must surface as an error");
        assert!(reconcile.pending(), "failed path must be queued");
        assert!(reconcile.dirty.contains("new.txt"));

        // The reconciliation rescan retries the failed upload and converges.
        let report = engine.sync().await.expect("reconciliation sync");
        assert!(report.errors.is_empty(), "reconcile must not leave errors");
        assert!(report.uploaded.0 >= 1, "the failed create must be retried");
    }

    #[tokio::test]
    async fn live_transient_edit_failure_marks_dirty_and_converges() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("edit.txt"), "v1").unwrap();
        // Seed a tracked state, then edit the file so triage sees a change.
        let local = LocalDb::open_in_memory().expect("in-memory LocalDb");
        local
            .save_file_state(&FileState::new(
                "edit.txt".to_string(),
                "stale-hash".to_string(),
                2,
                Utc::now(),
            ))
            .expect("seed state");
        std::fs::write(dir.path().join("edit.txt"), "v2").unwrap();

        let mut engine = live_engine_with_transient_failures(
            local,
            root.clone(),
            vec!["prefix/edit.txt".to_string()],
            Vec::new(),
        );

        let mut reconcile = Reconcile::new(Duration::from_secs(30));
        let touched = TouchTracker::new();
        let change = Change::local_modified("edit.txt".to_string(), String::new(), 0);

        handle_local_change(&mut engine, Some(&mut reconcile), &touched, &change)
            .await
            .expect_err("transient upload failure must surface as an error");
        assert!(reconcile.dirty.contains("edit.txt"));

        let report = engine.sync().await.expect("reconciliation sync");
        assert!(report.errors.is_empty(), "reconcile must not leave errors");
        assert!(report.uploaded.0 >= 1, "the failed edit must be retried");
    }

    #[tokio::test]
    async fn live_transient_delete_failure_marks_dirty_and_converges() {
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("del.txt"), "data").unwrap();
        let local = LocalDb::open_in_memory().expect("in-memory LocalDb");
        local
            .save_file_state(&FileState::new(
                "del.txt".to_string(),
                "stale-hash".to_string(),
                4,
                Utc::now(),
            ))
            .expect("seed state");
        let mut engine = live_engine_with_transient_failures(
            local,
            root.clone(),
            Vec::new(),
            vec!["prefix/del.txt".to_string()],
        );

        let mut reconcile = Reconcile::new(Duration::from_secs(30));
        let touched = TouchTracker::new();
        std::fs::remove_file(dir.path().join("del.txt")).unwrap();
        let change = Change::local_deleted("del.txt".to_string());

        handle_local_change(&mut engine, Some(&mut reconcile), &touched, &change)
            .await
            .expect_err("transient delete failure must surface as an error");
        assert!(reconcile.dirty.contains("del.txt"));

        let report = engine.sync().await.expect("reconciliation sync");
        assert!(report.errors.is_empty(), "reconcile must not leave errors");
        assert!(
            report.deleted_remote >= 1,
            "the failed delete must be retried"
        );
    }

    #[tokio::test]
    async fn watcher_overflow_schedules_full_rescan_that_converges() {
        // A file is created but its watcher event is lost to overflow (a burst
        // larger than the channel capacity). The overflow notice sets
        // `full_rescan`, and the bounded reconciliation converges the untracked
        // file without another user edit.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("burst.txt"), "data").unwrap();

        let mut engine = live_engine_with_transient_failures(
            LocalDb::open_in_memory().expect("in-memory LocalDb"),
            root.clone(),
            Vec::new(),
            Vec::new(),
        );
        let mut reconcile = Reconcile::new(Duration::from_secs(30));

        // Watcher overflow notice consumed by the live loop.
        reconcile.mark_full_rescan();
        assert!(reconcile.pending(), "overflow must schedule reconciliation");
        assert!(
            !reconcile.dirty.contains("burst.txt"),
            "overflow marks a full rescan, not a specific path"
        );

        let report = engine.sync().await.expect("reconciliation sync");
        assert!(report.errors.is_empty());
        assert!(
            report.uploaded.0 >= 1,
            "overflow-dropped create must converge"
        );
        reconcile.reset();
        assert!(
            !reconcile.pending(),
            "reconciliation must clear pending work"
        );
    }

    // ── Live worker supervision + source closure (#6349) ────────────────

    #[tokio::test]
    async fn live_worker_restart_budget_restarts_failures_then_gives_up() {
        // A source failure restarts the worker up to the budget, then surfaces
        // the error so the daemon exits nonzero instead of silently dropping
        // the path.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let worker = move || {
            let calls_clone = calls_clone.clone();
            async move {
                calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(anyhow::anyhow!("injected source failure"))
            }
        };
        let err = run_with_restart_budget(worker, "test-worker", 3, Duration::ZERO)
            .await
            .expect_err("budget exhaustion must surface an error");
        assert!(err.to_string().contains("injected source failure"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "worker must run exactly up to the restart budget"
        );
    }

    #[tokio::test]
    async fn live_worker_restart_budget_restarts_clean_exit_then_gives_up() {
        // A clean source exit (feed closed) is also a restart trigger: it must
        // not silently end the path's monitoring.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let worker = move || {
            let calls_clone = calls_clone.clone();
            async move {
                calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        };
        let err = run_with_restart_budget(worker, "test-worker", 3, Duration::ZERO)
            .await
            .expect_err("a clean source exit is also a restart trigger");
        assert!(err.to_string().contains("giving up"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "clean source exits must still restart up to the budget"
        );
    }

    #[tokio::test]
    async fn live_loop_surfaces_closed_source_instead_of_spinning() {
        // A closed local watcher source (producer exited) must surface as an
        // error so the supervisor can restart it, not leave the loop spinning
        // on the remaining channels.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        let mut engine = live_engine_with_transient_failures(
            LocalDb::open_in_memory().expect("in-memory LocalDb"),
            root.clone(),
            Vec::new(),
            Vec::new(),
        );
        let (local_tx, local_rx) = mpsc::channel::<Change>(1);
        drop(local_tx); // watcher producer gone
        let (_remote_tx, remote_rx) = mpsc::channel::<ChangeFeedEntry>(1);
        let (ack_tx, _ack_rx) = mpsc::channel::<String>(1);
        let (_reconcile_tx, reconcile_rx) = mpsc::channel::<()>(1);
        let (_overflow_tx, overflow_rx) = mpsc::channel::<()>(1);
        let mut config = AppConfig::default();
        config.sync.rescan_interval_secs = 0;

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_live_loop(
                &mut engine,
                &config,
                dir.path(),
                &Arc::new(RwLock::new(IgnoreMatcher::empty())),
                local_rx,
                remote_rx,
                ack_tx,
                reconcile_rx,
                overflow_rx,
                "0".to_string(),
            ),
        )
        .await
        .expect("live loop must return on a closed source, not spin");

        assert!(
            result.is_err(),
            "a closed source must surface an error to the supervisor"
        );
    }

    #[tokio::test]
    async fn live_loop_reconciles_edit_during_bootstrap_transition() {
        // An edit that lands during the bootstrap-to-watch transition (captured
        // by the watcher started before bootstrap) must be reconciled by the
        // after-bootstrap pass rather than left stranded.
        let dir = tempfile::tempdir().unwrap();
        let root = SyncDirPath::new(dir.path()).unwrap();
        std::fs::write(dir.path().join("transition.txt"), "hi").unwrap();

        let mut engine = live_engine_with_transient_failures(
            LocalDb::open_in_memory().expect("in-memory LocalDb"),
            root.clone(),
            Vec::new(),
            Vec::new(),
        );
        let (local_tx, local_rx) = mpsc::channel::<Change>(1);
        drop(local_tx); // closed source so the loop returns after the reconcile pass
        let (_remote_tx, remote_rx) = mpsc::channel::<ChangeFeedEntry>(1);
        let (ack_tx, _ack_rx) = mpsc::channel::<String>(1);
        let (_reconcile_tx, reconcile_rx) = mpsc::channel::<()>(1);
        let (_overflow_tx, overflow_rx) = mpsc::channel::<()>(1);
        let mut config = AppConfig::default();
        config.sync.rescan_interval_secs = 0;

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_live_loop(
                &mut engine,
                &config,
                dir.path(),
                &Arc::new(RwLock::new(IgnoreMatcher::empty())),
                local_rx,
                remote_rx,
                ack_tx,
                reconcile_rx,
                overflow_rx,
                "0".to_string(),
            ),
        )
        .await
        .expect("live loop must return after the reconcile pass");

        assert!(result.is_err(), "closed source surfaces as an error");

        // The after-bootstrap reconcile pass must have uploaded and tracked the
        // transition-window edit.
        assert!(
            engine
                .get_file_state("transition.txt")
                .await
                .unwrap()
                .is_some(),
            "edit landed during bootstrap must be reconciled after bootstrap"
        );
    }

    // ── Configured file-size limit (zero = unlimited) ─────────────────────
    #[test]
    fn size_limit_zero_means_unlimited() {
        // Zero (documented as unlimited) must yield no bound.
        let mut unlimited = AppConfig::default();
        unlimited.sync.max_file_size = 0;
        assert_eq!(size_limit(&unlimited), None);

        // Any positive value becomes an enforced bound.
        let mut bounded = AppConfig::default();
        bounded.sync.max_file_size = 4096;
        assert_eq!(size_limit(&bounded), Some(4096));
    }

    // ── Conflict notification delivery (per channel/root/generation) ──────
    //
    // The delivery orchestration is exercised with real notifiers backed by
    // fake HTTP senders so no network access is needed.

    /// An `AppConfig` with both channels configured; `notify_on_conflict` is
    /// controlled by the caller.
    fn delivery_config(notify_on_conflict: bool) -> AppConfig {
        let mut config = AppConfig::default();
        config.notifications.notify_on_conflict = notify_on_conflict;
        config.notifications.telegram.credentials = Some(TelegramCredentials {
            bot_token: "token".into(),
            chat_id: "chat".into(),
        });
        config.notifications.matrix.credentials = Some(MatrixCredentials {
            homeserver_url: "https://matrix.example.com".into(),
            access_token: "tok".into(),
            room_id: "room".into(),
        });
        config
    }

    /// A Telegram notifier backed by `sender`.
    fn telegram_notifier(sender: Arc<dyn crate::notify::HttpSender>) -> TelegramNotifier {
        TelegramNotifier::with_sender("token".into(), "chat".into(), sender)
    }

    /// A Matrix notifier backed by `sender`.
    fn matrix_notifier(sender: Arc<dyn crate::notify::HttpSender>) -> MatrixNotifier {
        MatrixNotifier::with_sender(
            "https://matrix.example.com".into(),
            "tok".into(),
            "room".into(),
            "m.notice".into(),
            sender,
        )
    }

    /// A success response for a fake HTTP sender.
    fn ok_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: String::new(),
        }
    }

    /// Store `conflict` in a fresh per-root DB and return its path.
    fn db_with_conflict(dir: &TempDir, conflict: &Conflict) -> PathBuf {
        let db_path = dir.path().join("state.db");
        let db = LocalDb::open(&db_path).unwrap();
        db.store_conflict(conflict).unwrap();
        drop(db);
        db_path
    }

    #[tokio::test]
    async fn both_channels_each_receive_conflict_once() {
        let config = delivery_config(true);
        let dir = TempDir::new().unwrap();
        let db_path = db_with_conflict(&dir, &make_conflict("/a.txt", "2-abc"));

        let tg_sender = Arc::new(FakeSender::new(vec![Ok(ok_response())]));
        let mx_sender = Arc::new(FakeSender::new(vec![Ok(ok_response())]));
        let tg = telegram_notifier(tg_sender.clone());
        let mx = matrix_notifier(mx_sender.clone());

        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        notify_conflicts(
            "matrix",
            &mx,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;

        // Each channel is delivered independently: Telegram marking the
        // conflict must not suppress Matrix.
        assert_eq!(tg_sender.call_count("/bot"), 1);
        assert_eq!(mx_sender.call_count("/_matrix"), 1);
    }

    #[tokio::test]
    async fn failed_channel_retries_without_duplicating_successful_channel() {
        let config = delivery_config(true);
        let dir = TempDir::new().unwrap();
        let db_path = db_with_conflict(&dir, &make_conflict("/a.txt", "2-abc"));

        // Telegram succeeds; Matrix fails on its first attempt.
        let tg_sender = Arc::new(FakeSender::new(vec![Ok(ok_response())]));
        let mx_sender = Arc::new(FakeSender::new(vec![
            Err(anyhow::anyhow!("matrix unavailable")),
            Ok(ok_response()),
        ]));
        let tg = telegram_notifier(tg_sender.clone());
        let mx = matrix_notifier(mx_sender.clone());

        // First pass: Telegram delivered, Matrix fails (stays retryable).
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        notify_conflicts(
            "matrix",
            &mx,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;

        // Second pass: Telegram already delivered (no duplicate); Matrix
        // retries and now succeeds.
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        notify_conflicts(
            "matrix",
            &mx,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            tg_sender.call_count("/bot"),
            1,
            "successful channel must not duplicate"
        );
        assert_eq!(
            mx_sender.call_count("/_matrix"),
            2,
            "failed channel must retry without affecting the successful one"
        );
    }

    #[tokio::test]
    async fn identical_relative_path_in_two_roots_delivered_per_root() {
        let config = delivery_config(true);
        // Two independent roots, each holding a conflict at the same relative
        // path. Each root has its own local DB, so both are delivered.
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let db_a = db_with_conflict(&dir_a, &make_conflict("/same.txt", "2-abc"));
        let db_b = db_with_conflict(&dir_b, &make_conflict("/same.txt", "2-abc"));

        let tg_sender = Arc::new(FakeSender::new(vec![Ok(ok_response()), Ok(ok_response())]));
        let tg = telegram_notifier(tg_sender.clone());

        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_a,
            dir_a.path(),
            Duration::from_secs(1),
        )
        .await;
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_b,
            dir_b.path(),
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            tg_sender.call_count("/bot"),
            2,
            "identical relative path must be delivered once per root"
        );
    }

    #[tokio::test]
    async fn recurrent_conflict_new_generation_is_notified_again() {
        let config = delivery_config(true);
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        let tg_sender = Arc::new(FakeSender::new(vec![Ok(ok_response()), Ok(ok_response())]));
        let tg = telegram_notifier(tg_sender.clone());

        // First conflict at generation 2.
        {
            let db = LocalDb::open(&db_path).unwrap();
            db.store_conflict(&make_conflict("/a.txt", "2-abc"))
                .unwrap();
        }
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(tg_sender.call_count("/bot"), 1);

        // Re-detecting the same conflict (same generation) must not re-notify.
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(tg_sender.call_count("/bot"), 1);

        // A recurrent conflict at the same path with a new generation is a
        // brand-new conflict and is delivered again.
        {
            let db = LocalDb::open(&db_path).unwrap();
            db.store_conflict(&make_conflict("/a.txt", "3-def"))
                .unwrap();
        }
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            tg_sender.call_count("/bot"),
            2,
            "new generation at the same path must be notified again"
        );
    }

    #[tokio::test]
    async fn disabled_conflict_notifications_send_none() {
        let config = delivery_config(false);
        let dir = TempDir::new().unwrap();
        let db_path = db_with_conflict(&dir, &make_conflict("/a.txt", "2-abc"));

        let tg_sender = Arc::new(FakeSender::new(vec![Ok(ok_response())]));
        let tg = telegram_notifier(tg_sender.clone());

        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            tg_sender.call_count("/bot"),
            0,
            "notify_on_conflict=false must send no conflict notifications"
        );
    }

    #[tokio::test]
    async fn hung_notifier_times_out_and_sync_continues() {
        let config = delivery_config(true);
        let dir = TempDir::new().unwrap();
        let db_path = db_with_conflict(&dir, &make_conflict("/a.txt", "2-abc"));

        // A notifier whose HTTP transport never responds must not block sync:
        // the delivery is bounded by the timeout and left retryable.
        let hanging = Arc::new(HangingSender);
        let tg = telegram_notifier(hanging);

        let start = Instant::now();
        notify_conflicts(
            "telegram",
            &tg,
            &config,
            &db_path,
            dir.path(),
            Duration::from_millis(200),
        )
        .await;

        assert!(
            start.elapsed() < Duration::from_secs(5),
            "hung notifier must time out rather than block sync"
        );

        // Nothing was marked delivered, so it remains retryable next cycle.
        let db = LocalDb::open(&db_path).unwrap();
        assert!(
            db.delivered_conflicts("telegram").unwrap().is_empty(),
            "timed-out delivery must stay retryable"
        );
    }
}
