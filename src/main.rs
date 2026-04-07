use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use semver::Version;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tar::Archive;
use tempfile::tempdir;
use tracing::info;
use zip::ZipArchive;

use couchdb_file_sync::cli;
use couchdb_file_sync::config::{AppConfig, SyncPath, default_log_file, default_user_config_file};
use couchdb_file_sync::logging::{AppLogWriter, RotationMode};
use couchdb_file_sync::slack::SlackNotifier;

const GITHUB_OWNER: &str = "haydonryan";
const GITHUB_REPO: &str = "couchdb-file-sync";

#[derive(serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(serde::Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Parser, Debug)]
#[command(name = "couchdb-file-sync")]
#[command(about = "Filesystem-to-CouchDB sync engine")]
#[command(version)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// CouchDB URL
    #[arg(long, global = true, env = "COUCHDB_FILE_SYNC_DB_URL")]
    db_url: Option<String>,

    /// CouchDB username
    #[arg(long, global = true, env = "COUCHDB_FILE_SYNC_DB_USERNAME")]
    db_user: Option<String>,

    /// CouchDB password
    #[arg(long, global = true, env = "COUCHDB_FILE_SYNC_DB_PASSWORD")]
    db_pass: Option<String>,

    /// CouchDB database name
    #[arg(long, global = true, env = "COUCHDB_FILE_SYNC_DB_NAME")]
    db_name: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a new sync directory
    Init {
        /// Directory to initialize (uses paths from config if not specified)
        path: Option<PathBuf>,

        /// CouchDB URL
        #[arg(long)]
        db_url: Option<String>,

        /// CouchDB database name
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Run a one-time sync
    Sync {
        /// Directory to sync (uses paths from config if not specified)
        path: Option<PathBuf>,

        /// Dry run (don't make changes)
        #[arg(long)]
        dry_run: bool,
    },

    /// Rebuild the remote scope from the local filesystem
    RebuildRemote {
        /// Directory to sync (uses paths from config if not specified)
        path: Option<PathBuf>,
    },

    /// Rebuild the local filesystem from the remote scope
    RebuildLocal {
        /// Directory to sync (uses paths from config if not specified)
        path: Option<PathBuf>,
    },

    /// Run continuous sync daemon
    Daemon {
        /// Directory to sync (uses paths from config if not specified)
        path: Option<PathBuf>,

        /// Poll interval in seconds
        #[arg(short, long, default_value = "60")]
        interval: u64,

        /// Use live sync (filesystem watcher + CouchDB changes feed)
        #[arg(long)]
        live: bool,
    },

    /// List conflicts
    Conflicts {
        /// Directory to check (uses paths from config if not specified)
        path: Option<PathBuf>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Resolve conflicts interactively
    Resolve {
        /// Working directory (uses paths from config if not specified)
        path: Option<PathBuf>,
    },

    /// Show sync status
    Status {
        /// Directory to check (uses paths from config if not specified)
        path: Option<PathBuf>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Install the binary and set up a user-level systemd service
    Install,

    /// Remove the user-level systemd service and installed binary
    Uninstall,

    /// Update couchdb-file-sync to the latest GitHub release
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cli_config = cli.config.clone();

    if cli.verbose > 0 {
        match resolved_config_path(cli_config.clone()) {
            Some((path, source)) => {
                eprintln!("Using config file ({source}): {}", path.display())
            }
            None => eprintln!("No config file found; using defaults and environment overrides"),
        }
    }

    // Load configuration
    let mut config = match AppConfig::load(cli_config.clone()) {
        Ok(c) => c,
        Err(e) => {
            if cli.verbose > 0 {
                info!("Could not load config file: {}", e);
            }
            AppConfig::default()
        }
    };

    // Override config with CLI arguments
    if let Some(url) = cli.db_url {
        config.couchdb.url = url;
    }
    if let Some(user) = cli.db_user {
        config.couchdb.username = Some(user);
    }
    if let Some(pass) = cli.db_pass {
        config.couchdb.password = Some(pass);
    }
    if let Some(name) = cli.db_name {
        config.couchdb.database = name;
    }

    let enable_file_logging = matches!(
        &cli.command,
        Commands::Sync { .. }
            | Commands::RebuildRemote { .. }
            | Commands::RebuildLocal { .. }
            | Commands::Daemon { .. }
    );

    // Initialize logging
    let daemon_mode = matches!(&cli.command, Commands::Daemon { .. });
    init_logging(cli.verbose, &config, enable_file_logging, daemon_mode);
    install_panic_hook(
        config.notifications.enabled,
        config.notifications.slack.webhook_url.clone(),
        resolved_config_path(cli_config).map(|(path, _)| path),
    );

    // Execute command
    match cli.command {
        Commands::Init {
            path,
            db_url,
            db_name,
        } => {
            let cli_path = path.is_some();
            let paths = resolve_paths(path, &config);
            for sync_path in paths {
                let path_configured = if cli_path {
                    config.paths.iter().any(|p| p.local == sync_path.local)
                } else {
                    true
                };
                if cli_path && !path_configured {
                    println!(
                        "Warning: {} is not listed in your config paths.",
                        sync_path.local.display()
                    );
                }
                cli::init(
                    sync_path.local,
                    db_url.clone(),
                    db_name.clone(),
                    path_configured,
                )
                .await?;
            }
        }
        Commands::Sync { path, dry_run } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            for sync_path in paths {
                let mut path_config = config.clone();
                path_config.couchdb.remote_path = sync_path.remote;
                info!(
                    "Syncing: {} -> {}",
                    sync_path.local.display(),
                    path_config.couchdb.remote_path
                );
                cli::sync(sync_path.local, path_config, dry_run).await?;
            }
        }
        Commands::RebuildRemote { path } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            for sync_path in paths {
                let mut path_config = config.clone();
                path_config.couchdb.remote_path = sync_path.remote;
                info!(
                    "Rebuilding remote: {} -> {}",
                    sync_path.local.display(),
                    path_config.couchdb.remote_path
                );
                cli::rebuild_remote(sync_path.local, path_config).await?;
            }
        }
        Commands::RebuildLocal { path } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            for sync_path in paths {
                let mut path_config = config.clone();
                path_config.couchdb.remote_path = sync_path.remote;
                info!(
                    "Rebuilding local: {} <- {}",
                    sync_path.local.display(),
                    path_config.couchdb.remote_path
                );
                cli::rebuild_local(sync_path.local, path_config).await?;
            }
        }
        Commands::Daemon {
            path,
            interval,
            live,
        } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            cli::daemon(paths, config, interval, live).await?;
        }
        Commands::Conflicts { path, json } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            let multi = paths.len() > 1;
            for sync_path in &paths {
                if multi {
                    println!("\n=== {} ===", sync_path.local.display());
                }
                cli::conflicts(sync_path.local.clone(), json).await?;
            }
        }
        Commands::Resolve { path } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            let multi = paths.len() > 1;
            for sync_path in &paths {
                let mut path_config = config.clone();
                path_config.couchdb.remote_path = sync_path.remote.clone();
                if multi {
                    println!("\n=== {} ===", sync_path.local.display());
                }
                cli::resolve(sync_path.local.clone(), path_config).await?;
            }
        }
        Commands::Status { path, json } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
                );
            }
            let multi = paths.len() > 1;
            for sync_path in &paths {
                if multi {
                    println!("\n=== {} ===", sync_path.local.display());
                }
                cli::status(sync_path.local.clone(), json, &config).await?;
            }
        }
        Commands::Install => {
            cli::install_user_service()?;
        }
        Commands::Uninstall => {
            cli::uninstall_user_service()?;
        }
        Commands::Update => {
            update().await?;
        }
    }

    Ok(())
}

fn binary_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

fn target_triplet_and_archive() -> (&'static str, &'static str) {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        ("x86_64-unknown-linux-gnu", "tar.gz")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        ("x86_64-unknown-linux-musl", "tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        ("aarch64-unknown-linux-gnu", "tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        ("x86_64-apple-darwin", "tar.gz")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        ("aarch64-apple-darwin", "tar.gz")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        ("x86_64-pc-windows-msvc", "zip")
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        ("aarch64-pc-windows-msvc", "zip")
    } else {
        panic!("unsupported target platform for self-update");
    }
}

fn parse_release_version(tag_name: &str) -> Result<Version> {
    let normalized = tag_name.strip_prefix('v').unwrap_or(tag_name);
    Version::parse(normalized).with_context(|| format!("Invalid release tag '{}'", tag_name))
}

async fn fetch_latest_release() -> Result<GitHubRelease> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        GITHUB_OWNER, GITHUB_REPO
    );
    Ok(reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "couchdb-file-sync")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GitHubRelease>()
        .await?)
}

async fn download_file(url: &str, dest: &std::path::Path) -> Result<()> {
    let bytes = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, "couchdb-file-sync")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    tokio::fs::write(dest, bytes).await?;
    Ok(())
}

fn extract_release_asset(
    archive_path: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<()> {
    if archive_path.extension().and_then(|s| s.to_str()) == Some("zip") {
        let file = fs::File::open(archive_path)?;
        let mut archive = ZipArchive::new(file)?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();
            if std::path::Path::new(&name)
                .file_name()
                .and_then(|s| s.to_str())
                == Some(binary_name())
            {
                let mut output = fs::File::create(output_path)?;
                io::copy(&mut entry, &mut output)?;
                return Ok(());
            }
        }
    } else {
        let file = fs::File::open(archive_path)?;
        let decompressed = GzDecoder::new(file);
        let mut archive = Archive::new(decompressed);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            if path.file_name().and_then(|s| s.to_str()) == Some(binary_name()) {
                entry.unpack(output_path)?;
                return Ok(());
            }
        }
    }
    anyhow::bail!("Binary not found in release archive");
}

fn install_binary(new_binary: &std::path::Path) -> Result<()> {
    let current_exe =
        std::env::current_exe().context("Failed to resolve current executable path")?;
    let current_dir = current_exe
        .parent()
        .context("Failed to determine executable directory")?;
    let temp_path = current_dir.join(format!("{}.new", binary_name()));
    fs::copy(new_binary, &temp_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&temp_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&temp_path, perms)?;
    }
    fs::rename(&temp_path, &current_exe)?;
    Ok(())
}

async fn update() -> Result<()> {
    if cfg!(target_os = "windows") {
        anyhow::bail!("Self-update is not supported on Windows for this binary.");
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let release = fetch_latest_release().await?;
    let latest = parse_release_version(&release.tag_name)?;
    if latest <= current {
        println!("Already on the latest version ({})", current);
        return Ok(());
    }
    let (target, archive_ext) = target_triplet_and_archive();
    let asset_name = format!("{}-{}.{}", binary_name(), target, archive_ext);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .context("missing release asset")?;
    let dir = tempdir()?;
    let archive_path = dir.path().join(&asset.name);
    let extracted = dir.path().join(binary_name());
    download_file(&asset.browser_download_url, &archive_path).await?;
    extract_release_asset(&archive_path, &extracted)?;
    install_binary(&extracted)?;
    println!("Updated {} from {} to {}", binary_name(), current, latest);
    Ok(())
}

fn resolved_config_path(explicit_path: Option<PathBuf>) -> Option<(PathBuf, &'static str)> {
    if let Some(path) = explicit_path {
        return Some((path, "--config"));
    }

    default_user_config_file_if_exists().map(|path| (path, "user config"))
}

fn default_user_config_file_if_exists() -> Option<PathBuf> {
    let yaml = default_user_config_file()?;
    if yaml.exists() {
        return Some(yaml);
    }

    let yml = yaml.with_extension("yml");
    if yml.exists() {
        return Some(yml);
    }

    None
}

/// Resolve sync paths from CLI argument or config
fn resolve_paths(cli_path: Option<PathBuf>, config: &AppConfig) -> Vec<SyncPath> {
    match cli_path {
        Some(path) => {
            // CLI path specified - prefer the matching configured path mapping.
            if let Some(sync_path) = config
                .paths
                .iter()
                .find(|sync_path| paths_match(&sync_path.local, &path))
            {
                return vec![sync_path.clone()];
            }

            // No configured mapping matched - fall back to the global remote_path.
            vec![SyncPath {
                local: path,
                remote: config.couchdb.remote_path.clone(),
            }]
        }
        None => {
            // No CLI path - use paths from config
            if config.paths.is_empty() {
                // Fallback to current directory with config's remote_path
                vec![SyncPath {
                    local: PathBuf::from("."),
                    remote: config.couchdb.remote_path.clone(),
                }]
            } else {
                config.paths.clone()
            }
        }
    }
}

fn paths_match(left: &std::path::Path, right: &std::path::Path) -> bool {
    if left == right {
        return true;
    }

    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn install_panic_hook(
    notifications_enabled: bool,
    slack_webhook_url: Option<String>,
    config_path: Option<PathBuf>,
) {
    let previous_hook = std::panic::take_hook();
    let slack_webhook_url = Arc::new(slack_webhook_url);
    let config_path = Arc::new(config_path);

    std::panic::set_hook(Box::new(move |panic_info| {
        previous_hook(panic_info);

        if !notifications_enabled {
            return;
        }

        let Some(webhook_url) = slack_webhook_url.as_deref() else {
            return;
        };

        let location = panic_info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown".to_string());
        let config_path = config_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "defaults/environment only".to_string());
        let message = format!(
            ":rotating_light: couchdb-file-sync panic\n\
Timestamp: {}\n\
Location: {}\n\
Config: {}\n\
Payload: {}",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
            location,
            config_path,
            panic_payload(panic_info)
        );

        if let Err(err) = SlackNotifier::notify_text_blocking(webhook_url, &message) {
            eprintln!("Failed to send panic notification to Slack: {err}");
        }
    }));
}

fn panic_payload(panic_info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(message) = panic_info.payload().downcast_ref::<&str>() {
        return (*message).to_string();
    }

    if let Some(message) = panic_info.payload().downcast_ref::<String>() {
        return message.clone();
    }

    "non-string panic payload".to_string()
}

/// Initialize logging based on verbosity or RUST_LOG env var
fn init_logging(verbose: u8, config: &AppConfig, enable_file_logging: bool, daemon_mode: bool) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Prefer RUST_LOG if set, otherwise use verbosity flag
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        let level = match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("couchdb_file_sync={}", level))
    };

    let stdout_layer = tracing_subscriber::fmt::layer().with_filter(filter);

    if enable_file_logging {
        let log_path = config
            .logging
            .file
            .clone()
            .or_else(default_log_file)
            .unwrap_or_else(|| std::path::PathBuf::from("couchdb-file-sync.log"));
        let rotation = if daemon_mode {
            RotationMode::Daily
        } else {
            RotationMode::Never
        };
        let log_writer = AppLogWriter::new(log_path.clone(), rotation, config.logging.rotated_logs);
        let (non_blocking, guard) = match log_writer {
            Ok(writer) => tracing_appender::non_blocking(writer),
            Err(err) => {
                eprintln!("Failed to open log file {}: {}", log_path.display(), err);
                tracing_subscriber::registry().with(stdout_layer).init();
                return;
            }
        };
        Box::leak(Box::new(guard));

        let file_filter = EnvFilter::new("couchdb_file_sync=trace");
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(non_blocking)
            .with_filter(file_filter);

        tracing_subscriber::registry()
            .with(stdout_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry().with(stdout_layer).init();
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, paths_match, resolve_paths};
    use clap::Parser;
    use couchdb_file_sync::config::{AppConfig, SyncPath};
    use std::path::{Path, PathBuf};

    #[test]
    fn cli_path_uses_matching_configured_remote_prefix() {
        let mut config = AppConfig::default();
        config.couchdb.remote_path = "global/".to_string();
        config.paths = vec![SyncPath {
            local: PathBuf::from("/tmp/agents"),
            remote: "Agents".to_string(),
        }];

        let resolved = resolve_paths(Some(PathBuf::from("/tmp/agents")), &config);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].local, PathBuf::from("/tmp/agents"));
        assert_eq!(resolved[0].remote, "Agents");
    }

    #[test]
    fn cli_path_falls_back_to_global_remote_when_unconfigured() {
        let mut config = AppConfig::default();
        config.couchdb.remote_path = "global/".to_string();

        let resolved = resolve_paths(Some(PathBuf::from("/tmp/other")), &config);

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].local, PathBuf::from("/tmp/other"));
        assert_eq!(resolved[0].remote, "global/");
    }

    #[test]
    fn cli_parses_rebuild_remote_subcommand() {
        let cli =
            Cli::try_parse_from(["couchdb-file-sync", "rebuild-remote", "/tmp/docs"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::RebuildRemote {
                path: Some(ref path)
            } if path == &PathBuf::from("/tmp/docs")
        ));
    }

    #[test]
    fn cli_parses_rebuild_local_subcommand() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "rebuild-local", "/tmp/docs"]).unwrap();

        assert!(matches!(
            cli.command,
            Commands::RebuildLocal {
                path: Some(ref path)
            } if path == &PathBuf::from("/tmp/docs")
        ));
    }

    // =========================================================================
    // Tests for paths_match
    // =========================================================================

    #[test]
    fn paths_match_equal_paths() {
        assert!(paths_match(Path::new("/tmp/test"), Path::new("/tmp/test")));
    }

    #[test]
    fn paths_match_different_paths() {
        assert!(!paths_match(
            Path::new("/tmp/test1"),
            Path::new("/tmp/test2")
        ));
    }

    // =========================================================================
    // Tests for resolve_paths edge cases
    // =========================================================================

    #[test]
    fn resolve_paths_no_cli_path_uses_config_paths() {
        let config = AppConfig {
            paths: vec![
                SyncPath {
                    local: PathBuf::from("/path1"),
                    remote: "remote1/".to_string(),
                },
                SyncPath {
                    local: PathBuf::from("/path2"),
                    remote: "remote2/".to_string(),
                },
            ],
            ..Default::default()
        };

        let resolved = resolve_paths(None, &config);
        assert_eq!(resolved.len(), 2);
    }

    #[test]
    fn resolve_paths_no_cli_path_empty_config_uses_current_dir() {
        let config = AppConfig::default();

        let resolved = resolve_paths(None, &config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].local, PathBuf::from("."));
    }

    #[test]
    fn panic_payload_handles_string_payloads() {
        let panic = std::panic::catch_unwind(|| panic!("boom")).unwrap_err();
        let panic = panic.downcast::<&str>().unwrap();
        assert_eq!(*panic, "boom");
    }

    #[test]
    fn panic_payload_handles_owned_string_payloads() {
        let panic =
            std::panic::catch_unwind(|| std::panic::panic_any(String::from("owned"))).unwrap_err();
        let panic = panic.downcast::<String>().unwrap();
        assert_eq!(*panic, "owned".to_string());
    }
}
