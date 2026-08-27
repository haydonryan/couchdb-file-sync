use anyhow::Result;
use argy::FromArgs;
use std::path::PathBuf;
use tracing::info;

use couchdb_file_sync::cli;
use couchdb_file_sync::config::{AppConfig, SyncPath, default_log_file, default_user_config_file};
use couchdb_file_sync::logging::AppLogWriter;

#[derive(FromArgs, Debug)]
/// filesystem-to-CouchDB sync engine
struct Cli {
    /// path to configuration file
    #[argy(option, short = 'c', global)]
    config: Option<PathBuf>,

    /// enable verbose logging
    #[argy(switch, short = 'v', global)]
    verbose: u8,

    /// `CouchDB` URL
    #[argy(option, global, env = "COUCHDB_FILE_SYNC_DB_URL")]
    db_url: Option<String>,

    /// `CouchDB` username
    #[argy(option, global, env = "COUCHDB_FILE_SYNC_DB_USERNAME")]
    db_user: Option<String>,

    /// `CouchDB` password
    #[argy(option, global, env = "COUCHDB_FILE_SYNC_DB_PASSWORD")]
    db_pass: Option<String>,

    /// `CouchDB` database name
    #[argy(option, global, env = "COUCHDB_FILE_SYNC_DB_NAME")]
    db_name: Option<String>,

    #[argy(subcommand)]
    command: Commands,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand)]
enum Commands {
    Init(InitCommand),
    Sync(SyncCommand),
    RebuildRemote(RebuildRemoteCommand),
    RebuildLocal(RebuildLocalCommand),
    Daemon(DaemonCommand),
    Conflicts(ConflictsCommand),
    Resolve(ResolveCommand),
    Status(StatusCommand),
    Install(InstallCommand),
    Uninstall(UninstallCommand),
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "init")]
/// initialize a new sync directory
struct InitCommand {
    /// directory to initialize (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,

    /// `CouchDB` URL
    #[argy(option)]
    db_url: Option<String>,

    /// `CouchDB` database name
    #[argy(option)]
    db_name: Option<String>,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "sync")]
/// run a one-time sync
struct SyncCommand {
    /// directory to sync (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,

    /// dry run (don't make changes)
    #[argy(switch)]
    dry_run: bool,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "rebuild-remote")]
/// rebuild the remote scope from the local filesystem
struct RebuildRemoteCommand {
    /// directory to sync (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "rebuild-local")]
/// rebuild the local filesystem from the remote scope
struct RebuildLocalCommand {
    /// directory to sync (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "daemon")]
/// run continuous sync daemon
struct DaemonCommand {
    /// directory to sync (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,

    /// poll interval in seconds
    #[argy(option, short = 'i', default = "60")]
    interval: u64,

    /// use live sync (filesystem watcher + `CouchDB` changes feed)
    #[argy(switch)]
    live: bool,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "conflicts")]
/// list conflicts
struct ConflictsCommand {
    /// directory to check (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,

    /// output as JSON
    #[argy(switch)]
    json: bool,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "resolve")]
/// resolve conflicts interactively
struct ResolveCommand {
    /// working directory (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "status")]
/// show sync status
struct StatusCommand {
    /// directory to check (uses paths from config if not specified)
    #[argy(positional)]
    path: Option<PathBuf>,

    /// output as JSON
    #[argy(switch)]
    json: bool,
}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "install")]
/// install the binary and set up a user-level systemd service
struct InstallCommand {}

#[derive(FromArgs, Debug)]
#[argy(subcommand, name = "uninstall")]
/// remove the user-level systemd service and installed binary
struct UninstallCommand {}

#[tokio::main]
async fn main() -> Result<()> {
    run().await
}

async fn run() -> Result<()> {
    let cli: Cli = argy::from_env();

    if cli.verbose > 0 {
        match resolved_config_path(cli.config.clone()) {
            Some((path, source)) => {
                eprintln!("Using config file ({source}): {}", path.display());
            }
            None => eprintln!("No config file found; using defaults and environment overrides"),
        }
    }

    let config = load_config(&cli);

    let enable_file_logging = matches!(
        &cli.command,
        Commands::Sync(_)
            | Commands::RebuildRemote(_)
            | Commands::RebuildLocal(_)
            | Commands::Daemon(_)
    );

    // Initialize logging
    let daemon_mode = matches!(&cli.command, Commands::Daemon(_));
    init_logging(cli.verbose, &config, enable_file_logging, daemon_mode);

    // Execute command
    dispatch_command(cli.command, config).await
}

fn load_config(cli: &Cli) -> AppConfig {
    // Load configuration
    let mut config = match AppConfig::load(cli.config.clone()) {
        Ok(c) => c,
        Err(e) => {
            if cli.verbose > 0 {
                info!("Could not load config file: {}", e);
            }
            AppConfig::default()
        }
    };

    // Override config with CLI arguments
    apply_cli_overrides(&mut config, cli);

    config
}

fn apply_cli_overrides(config: &mut AppConfig, cli: &Cli) {
    if let Some(url) = cli.db_url.clone() {
        config.couchdb.url = url;
    }
    if let (Some(username), Some(password)) = (cli.db_user.as_ref(), cli.db_pass.as_ref()) {
        config.couchdb.auth = Some(couchdb_file_sync::config::CouchDbAuth {
            username: username.clone(),
            password: password.clone(),
        });
    } else if let Some(user) = cli.db_user.as_ref() {
        config.couchdb.auth = Some(couchdb_file_sync::config::CouchDbAuth {
            username: user.clone(),
            password: String::new(),
        });
    } else if let Some(pass) = cli.db_pass.as_ref() {
        config.couchdb.auth = Some(couchdb_file_sync::config::CouchDbAuth {
            username: String::new(),
            password: pass.clone(),
        });
    }
    if let Some(name) = cli.db_name.clone() {
        config.couchdb.database = name;
    }
}

async fn dispatch_command(command: Commands, config: AppConfig) -> Result<()> {
    match command {
        Commands::Init(cmd) => {
            run_init(cmd.path, cmd.db_url.as_ref(), cmd.db_name.as_ref(), &config)?;
        }
        Commands::Sync(cmd) => run_sync(cmd.path, cmd.dry_run, &config).await?,
        Commands::RebuildRemote(cmd) => run_rebuild_remote(cmd.path, &config).await?,
        Commands::RebuildLocal(cmd) => run_rebuild_local(cmd.path, &config).await?,
        Commands::Daemon(cmd) => run_daemon(cmd.path, cmd.interval, cmd.live, config).await?,
        Commands::Conflicts(cmd) => run_conflicts(cmd.path, cmd.json, &config)?,
        Commands::Resolve(cmd) => run_resolve(cmd.path, &config).await?,
        Commands::Status(cmd) => run_status(cmd.path, cmd.json, &config)?,
        Commands::Install(_) => {
            cli::install_user_service()?;
        }
        Commands::Uninstall(_) => {
            cli::uninstall_user_service()?;
        }
    }
    Ok(())
}

fn resolve_paths_or_bail(path: Option<PathBuf>, config: &AppConfig) -> Result<Vec<SyncPath>> {
    let paths = resolve_paths(path, config);
    if paths.is_empty() {
        anyhow::bail!(
            "No sync paths configured. Specify a path or add paths to couchdb-file-sync.yaml"
        );
    }
    Ok(paths)
}

fn run_init(
    path: Option<PathBuf>,
    db_url: Option<&String>,
    db_name: Option<&String>,
    config: &AppConfig,
) -> Result<()> {
    let cli_path = path.is_some();
    let paths = resolve_paths(path, config);
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
            &sync_path.local,
            db_url.cloned(),
            db_name.cloned(),
            path_configured,
        )?;
    }
    Ok(())
}

async fn run_sync(path: Option<PathBuf>, dry_run: bool, config: &AppConfig) -> Result<()> {
    let paths = resolve_paths_or_bail(path, config)?;
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
    Ok(())
}

async fn run_rebuild_remote(path: Option<PathBuf>, config: &AppConfig) -> Result<()> {
    let paths = resolve_paths_or_bail(path, config)?;
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
    Ok(())
}

async fn run_rebuild_local(path: Option<PathBuf>, config: &AppConfig) -> Result<()> {
    let paths = resolve_paths_or_bail(path, config)?;
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
    Ok(())
}

async fn run_daemon(
    path: Option<PathBuf>,
    interval: u64,
    live: bool,
    config: AppConfig,
) -> Result<()> {
    let paths = resolve_paths_or_bail(path, &config)?;
    cli::daemon(paths, config, interval, live).await
}

fn run_conflicts(path: Option<PathBuf>, json: bool, config: &AppConfig) -> Result<()> {
    let paths = resolve_paths_or_bail(path, config)?;
    let multi = paths.len() > 1;
    for sync_path in &paths {
        if multi {
            println!("\n=== {} ===", sync_path.local.display());
        }
        cli::conflicts(&sync_path.local, json)?;
    }
    Ok(())
}

async fn run_resolve(path: Option<PathBuf>, config: &AppConfig) -> Result<()> {
    let paths = resolve_paths_or_bail(path, config)?;
    let multi = paths.len() > 1;
    for sync_path in &paths {
        let mut path_config = config.clone();
        path_config.couchdb.remote_path = sync_path.remote.clone();
        if multi {
            println!("\n=== {} ===", sync_path.local.display());
        }
        cli::resolve(sync_path.local.clone(), path_config).await?;
    }
    Ok(())
}

fn run_status(path: Option<PathBuf>, json: bool, config: &AppConfig) -> Result<()> {
    let paths = resolve_paths_or_bail(path, config)?;
    let multi = paths.len() > 1;
    for sync_path in &paths {
        if multi {
            println!("\n=== {} ===", sync_path.local.display());
        }
        cli::status(&sync_path.local, json, config)?;
    }
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

    let alternate = yaml.with_extension("yml");
    if alternate.exists() {
        return Some(alternate);
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

/// Initialize logging based on verbosity or `RUST_LOG` env var
fn init_logging(verbose: u8, config: &AppConfig, enable_file_logging: bool, daemon_mode: bool) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Prefer RUST_LOG if set, otherwise use verbosity flag
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        let level = if verbose >= 2 {
            couchdb_file_sync::config::LogLevel::Trace
        } else if verbose >= 1 {
            couchdb_file_sync::config::LogLevel::Debug
        } else {
            config.logging.level
        };
        EnvFilter::new(format!("couchdb_file_sync={}", level.as_filter_str()))
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
            couchdb_file_sync::config::RotationConfig::DailyKeep
        } else {
            couchdb_file_sync::config::RotationConfig::Never
        };
        let log_writer = AppLogWriter::new(log_path.clone(), rotation);
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
    use super::{Cli, Commands, init_logging};
    use super::{
        apply_cli_overrides, default_user_config_file_if_exists, paths_match, resolve_paths,
        resolved_config_path,
    };
    use argy::FromArgs;
    use couchdb_file_sync::config::{AppConfig, SyncPath};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Global mutex to serialize tests that modify environment variables.
    /// Rust test runner runs tests in parallel within the same binary,
    /// so env-var-dependent tests must be serialized.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper to run an env-dependent test with a saved HOME.
    fn with_saved_home<F>(f: F)
    where
        F: FnOnce(),
    {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // Avoid letting a previous test's XDG_CONFIG_HOME leak through
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        f();
        // Restore
        if let Some(ref h) = old_home {
            unsafe {
                std::env::set_var("HOME", h);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
        if let Some(ref x) = old_xdg {
            unsafe {
                std::env::set_var("XDG_CONFIG_HOME", x);
            }
        } else {
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }
        }
        // guard dropped here, releasing the lock
    }

    // ============================================================
    // resolve_paths tests
    // ============================================================

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
    fn resolve_paths_empty_config_no_cli_path_returns_current_dir() {
        let config = AppConfig::default();
        let resolved = resolve_paths(None, &config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].local, PathBuf::from("."));
        assert_eq!(resolved[0].remote, "");
    }

    #[test]
    fn resolve_paths_empty_config_with_cli_path_uses_cli_path() {
        let config = AppConfig::default();
        let resolved = resolve_paths(Some(PathBuf::from("/custom/path")), &config);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].local, PathBuf::from("/custom/path"));
    }

    #[test]
    fn resolve_paths_uses_configured_paths_when_no_cli_path() {
        let config = AppConfig {
            paths: vec![
                SyncPath {
                    local: PathBuf::from("/home/user/docs"),
                    remote: "docs/".to_string(),
                },
                SyncPath {
                    local: PathBuf::from("/home/user/photos"),
                    remote: "photos/".to_string(),
                },
            ],
            ..Default::default()
        };
        let resolved = resolve_paths(None, &config);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].remote, "docs/");
        assert_eq!(resolved[1].remote, "photos/");
    }

    // ============================================================
    // paths_match tests
    // ============================================================

    #[test]
    fn paths_match_identical_paths() {
        assert!(paths_match(
            PathBuf::from("/tmp/test-path").as_path(),
            PathBuf::from("/tmp/test-path").as_path(),
        ));
    }

    #[test]
    fn paths_match_different_paths_returns_false() {
        assert!(!paths_match(
            PathBuf::from("/tmp/path-a").as_path(),
            PathBuf::from("/tmp/path-b").as_path(),
        ));
    }

    #[test]
    fn paths_match_canonicalized_paths() {
        let tmp = TempDir::new().unwrap();
        let dir_a = tmp.path().join("dir_a");
        let dir_b = tmp.path().join("dir_b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        // Same path should match
        assert!(paths_match(&dir_a, &dir_a));
        // Different paths should not match
        assert!(!paths_match(&dir_a, &dir_b));
    }

    // ============================================================
    // default_user_config_file_if_exists tests
    // ============================================================

    #[test]
    fn default_user_config_file_if_exists_returns_none_for_missing() {
        with_saved_home(|| {
            let tmp = TempDir::new().unwrap();
            let fake_home = tmp.path().join("home");
            std::fs::create_dir_all(&fake_home).unwrap();
            unsafe {
                std::env::set_var("HOME", &fake_home);
            }
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }

            let result = default_user_config_file_if_exists();
            assert!(
                result.is_none(),
                "expected None for missing config, got {result:?}"
            );
        });
    }

    #[test]
    fn default_user_config_file_if_exists_finds_yaml() {
        with_saved_home(|| {
            let tmp = TempDir::new().unwrap();
            let config_dir = tmp.path().join(".config").join("couchdb-file-sync");
            std::fs::create_dir_all(&config_dir).unwrap();
            let yaml_path = config_dir.join("couchdb-file-sync.yaml");
            std::fs::write(&yaml_path, "").unwrap();

            unsafe {
                std::env::set_var("HOME", tmp.path());
            }
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }

            let result = default_user_config_file_if_exists();
            assert!(
                result.is_some(),
                "expected Some for existing yaml, got None"
            );
            assert_eq!(result.unwrap(), yaml_path);
        });
    }

    #[test]
    fn default_user_config_file_if_exists_finds_yml_fallback() {
        with_saved_home(|| {
            let tmp = TempDir::new().unwrap();
            let config_dir = tmp.path().join(".config").join("couchdb-file-sync");
            std::fs::create_dir_all(&config_dir).unwrap();
            let yml_path = config_dir.join("couchdb-file-sync.yml");
            std::fs::write(&yml_path, "").unwrap();

            unsafe {
                std::env::set_var("HOME", tmp.path());
            }
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }

            let result = default_user_config_file_if_exists();
            assert!(result.is_some(), "expected Some for existing yml, got None");
            assert_eq!(result.unwrap(), yml_path);
        });
    }

    #[test]
    fn default_user_config_file_if_exists_prefers_yaml_over_yml() {
        with_saved_home(|| {
            let tmp = TempDir::new().unwrap();
            let config_dir = tmp.path().join(".config").join("couchdb-file-sync");
            std::fs::create_dir_all(&config_dir).unwrap();
            let yaml_path = config_dir.join("couchdb-file-sync.yaml");
            let yml_path = config_dir.join("couchdb-file-sync.yml");
            std::fs::write(&yaml_path, "yaml").unwrap();
            std::fs::write(&yml_path, "yml").unwrap();

            unsafe {
                std::env::set_var("HOME", tmp.path());
            }
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }

            let result = default_user_config_file_if_exists();
            assert!(result.is_some());
            // Should prefer .yaml over .yml
            assert_eq!(result.unwrap(), yaml_path);
        });
    }

    // ============================================================
    // resolved_config_path tests
    // ============================================================

    #[test]
    fn resolved_config_path_returns_explicit_path() {
        let explicit = PathBuf::from("/custom/config.yaml");
        let result = resolved_config_path(Some(explicit.clone()));
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, explicit);
    }

    #[test]
    fn resolved_config_path_returns_none_when_no_file_and_no_explicit() {
        with_saved_home(|| {
            let tmp = TempDir::new().unwrap();
            let fake_home = tmp.path().join("home");
            std::fs::create_dir_all(&fake_home).unwrap();
            unsafe {
                std::env::set_var("HOME", &fake_home);
            }
            unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            }

            let result = resolved_config_path(None);
            assert!(result.is_none());
        });
    }

    // ============================================================
    // init_logging tests
    // ============================================================

    /// Smoke test: `init_logging` with default settings should not panic.
    /// Note: Only one `init_logging` test is included because `tracing_subscriber::init()`
    /// can only be called once per process. Running multiple `init_logging` tests would
    /// require separate test binaries or using `try_init()` instead of `init()`.
    #[test]
    fn init_logging_smoke_test_default_verbose() {
        let config = AppConfig::default();
        init_logging(0, &config, false, false);
    }

    // ============================================================

    #[test]
    fn cli_parses_rebuild_remote_subcommand() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["rebuild-remote", "/tmp/docs"]).unwrap();

        assert!(matches!(cli.command, Commands::RebuildRemote(ref cmd)
            if cmd.path == Some(PathBuf::from("/tmp/docs"))));
    }

    #[test]
    fn cli_parses_rebuild_local_subcommand() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["rebuild-local", "/tmp/docs"]).unwrap();

        assert!(matches!(cli.command, Commands::RebuildLocal(ref cmd)
            if cmd.path == Some(PathBuf::from("/tmp/docs"))));
    }

    // --- Init subcommand ---

    #[test]
    fn cli_parses_init_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["init"]).unwrap();
        assert!(matches!(cli.command, Commands::Init(ref cmd) if cmd.path.is_none()));
    }

    #[test]
    fn cli_parses_init_with_path() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["init", "/my/path"]).unwrap();
        assert!(matches!(cli.command, Commands::Init(ref cmd)
            if cmd.path == Some(PathBuf::from("/my/path"))));
    }
    #[test]
    fn cli_parses_init_with_path_db_url_db_name() {
        // `--db-url`/`--db-name` after `init` are top-level global options,
        // so argy routes them to the Cli rather than the InitCommand.
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &[
                "init",
                "/my/path",
                "--db-url",
                "https://couch.example.com:6984",
                "--db-name",
                "my_database",
            ],
        )
        .unwrap();
        assert!(matches!(cli.command, Commands::Init(ref cmd)
            if cmd.path == Some(PathBuf::from("/my/path"))));
        assert_eq!(
            cli.db_url.as_deref(),
            Some("https://couch.example.com:6984")
        );
        assert_eq!(cli.db_name.as_deref(), Some("my_database"));
    }

    #[test]
    fn cli_parses_init_with_only_db_url() {
        // `--db-url` after `init` is a top-level global option, so argy
        // routes it to the Cli rather than the InitCommand.
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &["init", "--db-url", "https://couch.example.com:6984"],
        )
        .unwrap();
        assert!(matches!(cli.command, Commands::Init(ref cmd) if cmd.path.is_none()));
        assert_eq!(
            cli.db_url.as_deref(),
            Some("https://couch.example.com:6984")
        );
    }

    // --- Sync subcommand ---

    #[test]
    fn cli_parses_sync_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["sync"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync(ref cmd)
            if cmd.path.is_none() && !cmd.dry_run));
    }

    #[test]
    fn cli_parses_sync_with_path() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["sync", "/data/docs"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync(ref cmd)
            if cmd.path == Some(PathBuf::from("/data/docs"))));
    }

    #[test]
    fn cli_parses_sync_with_dry_run() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["sync", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync(ref cmd) if cmd.dry_run));
    }

    #[test]
    fn cli_parses_sync_with_path_and_dry_run() {
        let cli =
            Cli::from_args(&["couchdb-file-sync"], &["sync", "/data/docs", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync(ref cmd)
            if cmd.path == Some(PathBuf::from("/data/docs")) && cmd.dry_run));
    }

    // --- Daemon subcommand ---

    #[test]
    fn cli_parses_daemon_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["daemon"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon(ref cmd)
            if cmd.path.is_none() && cmd.interval == 60 && !cmd.live));
    }

    #[test]
    fn cli_parses_daemon_with_interval() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["daemon", "--interval", "30"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon(ref cmd) if cmd.interval == 30));
    }

    #[test]
    fn cli_parses_daemon_with_live_flag() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["daemon", "--live"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon(ref cmd) if cmd.live));
    }

    #[test]
    fn cli_parses_daemon_with_path_interval_live() {
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &["daemon", "/my/path", "--interval", "120", "--live"],
        )
        .unwrap();
        assert!(matches!(cli.command, Commands::Daemon(ref cmd)
            if cmd.path == Some(PathBuf::from("/my/path")) && cmd.interval == 120 && cmd.live));
    }

    #[test]
    fn cli_parses_daemon_with_short_interval() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["daemon", "-i", "15"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon(ref cmd) if cmd.interval == 15));
    }

    // --- Conflicts subcommand ---

    #[test]
    fn cli_parses_conflicts_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["conflicts"]).unwrap();
        assert!(matches!(cli.command, Commands::Conflicts(ref cmd)
            if cmd.path.is_none() && !cmd.json));
    }

    #[test]
    fn cli_parses_conflicts_with_json() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["conflicts", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Conflicts(ref cmd) if cmd.json));
    }

    #[test]
    fn cli_parses_conflicts_with_path() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["conflicts", "/data"]).unwrap();
        assert!(matches!(cli.command, Commands::Conflicts(ref cmd)
            if cmd.path == Some(PathBuf::from("/data"))));
    }

    #[test]
    fn cli_parses_conflicts_with_path_and_json() {
        let cli =
            Cli::from_args(&["couchdb-file-sync"], &["conflicts", "/data", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Conflicts(ref cmd)
            if cmd.path == Some(PathBuf::from("/data")) && cmd.json));
    }

    // --- Resolve subcommand ---

    #[test]
    fn cli_parses_resolve_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["resolve"]).unwrap();
        assert!(matches!(cli.command, Commands::Resolve(ref cmd) if cmd.path.is_none()));
    }

    #[test]
    fn cli_parses_resolve_with_path() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["resolve", "/data"]).unwrap();
        assert!(matches!(cli.command, Commands::Resolve(ref cmd)
            if cmd.path == Some(PathBuf::from("/data"))));
    }

    // --- Status subcommand ---

    #[test]
    fn cli_parses_status_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status(ref cmd)
            if cmd.path.is_none() && !cmd.json));
    }

    #[test]
    fn cli_parses_status_with_json() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["status", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Status(ref cmd) if cmd.json));
    }

    #[test]
    fn cli_parses_status_with_path() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["status", "/data"]).unwrap();
        assert!(matches!(cli.command, Commands::Status(ref cmd)
            if cmd.path == Some(PathBuf::from("/data"))));
    }

    #[test]
    fn cli_parses_status_with_path_and_json() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["status", "/data", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Status(ref cmd)
            if cmd.path == Some(PathBuf::from("/data")) && cmd.json));
    }

    // --- RebuildRemote subcommand ---

    #[test]
    fn cli_parses_rebuild_remote_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["rebuild-remote"]).unwrap();
        assert!(matches!(cli.command, Commands::RebuildRemote(ref cmd) if cmd.path.is_none()));
    }

    // --- RebuildLocal subcommand ---

    #[test]
    fn cli_parses_rebuild_local_no_args() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["rebuild-local"]).unwrap();
        assert!(matches!(cli.command, Commands::RebuildLocal(ref cmd) if cmd.path.is_none()));
    }

    // --- Install subcommand ---

    #[test]
    fn cli_parses_install() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["install"]).unwrap();
        assert!(matches!(cli.command, Commands::Install(_)));
    }

    // --- Uninstall subcommand ---

    #[test]
    fn cli_parses_uninstall() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["uninstall"]).unwrap();
        assert!(matches!(cli.command, Commands::Uninstall(_)));
    }

    // --- Global args ---

    #[test]
    fn cli_parses_global_verbose_count() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["-v", "status"]).unwrap();
        assert_eq!(cli.verbose, 1);

        let cli = Cli::from_args(&["couchdb-file-sync"], &["-vv", "status"]).unwrap();
        assert_eq!(cli.verbose, 2);

        let cli = Cli::from_args(&["couchdb-file-sync"], &["-vvv", "status"]).unwrap();
        assert_eq!(cli.verbose, 3);
    }

    #[test]
    fn cli_parses_global_db_url() {
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &["--db-url", "https://example.com:5984", "status"],
        )
        .unwrap();
        assert_eq!(cli.db_url.as_deref(), Some("https://example.com:5984"));
    }

    #[test]
    fn cli_parses_global_db_user_and_db_pass() {
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &["--db-user", "admin", "--db-pass", "secret", "status"],
        )
        .unwrap();
        assert_eq!(cli.db_user.as_deref(), Some("admin"));
        assert_eq!(cli.db_pass.as_deref(), Some("secret"));
    }

    #[test]
    fn cli_parses_global_db_name() {
        let cli =
            Cli::from_args(&["couchdb-file-sync"], &["--db-name", "my_db", "status"]).unwrap();
        assert_eq!(cli.db_name.as_deref(), Some("my_db"));
    }

    #[test]
    fn cli_parses_global_config_path() {
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &["--config", "/path/to/config.yaml", "status"],
        )
        .unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(PathBuf::from("/path/to/config.yaml").as_path())
        );
    }

    #[test]
    fn cli_parses_global_verbose_with_subcommand() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["-vv", "sync", "--dry-run"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(matches!(cli.command, Commands::Sync(ref cmd) if cmd.dry_run));
    }
    #[test]
    fn cli_reads_db_env_vars_when_not_provided_on_cli() {
        // argy sources `env`-declared options from the environment when the
        // CLI value is absent.
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("COUCHDB_FILE_SYNC_DB_URL", "https://env.example.com:5984");
            std::env::set_var("COUCHDB_FILE_SYNC_DB_NAME", "envdb");
        }
        let cli = Cli::from_args(&["couchdb-file-sync"], &["status"]).unwrap();
        unsafe {
            std::env::remove_var("COUCHDB_FILE_SYNC_DB_URL");
            std::env::remove_var("COUCHDB_FILE_SYNC_DB_NAME");
        }
        assert_eq!(cli.db_url.as_deref(), Some("https://env.example.com:5984"));
        assert_eq!(cli.db_name.as_deref(), Some("envdb"));
    }

    #[test]
    fn cli_rejects_invalid_subcommand() {
        let result = Cli::from_args(&["couchdb-file-sync"], &["invalid-cmd"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_rejects_unknown_flag() {
        let result = Cli::from_args(&["couchdb-file-sync"], &["status", "--unknown-flag"]);
        assert!(result.is_err());
    }

    #[test]
    fn apply_cli_overrides_sets_url_db_name_and_auth() {
        let cli = Cli::from_args(
            &["couchdb-file-sync"],
            &[
                "--db-url",
                "http://localhost:5984/",
                "--db-user",
                "alice",
                "--db-pass",
                "secret",
                "--db-name",
                "mydb",
                "status",
            ],
        )
        .unwrap();

        let mut config = AppConfig::default();
        apply_cli_overrides(&mut config, &cli);

        assert_eq!(config.couchdb.url, "http://localhost:5984/");
        assert_eq!(config.couchdb.database, "mydb");
        let auth = config.couchdb.auth.as_ref().expect("auth overridden");
        assert_eq!(auth.username, "alice");
        assert_eq!(auth.password, "secret");
    }

    #[test]
    fn apply_cli_overrides_leaves_config_untouched_when_no_flags() {
        let cli = Cli::from_args(&["couchdb-file-sync"], &["status"]).unwrap();
        let mut config = AppConfig::default();
        config.couchdb.url = "original".to_string();

        apply_cli_overrides(&mut config, &cli);

        assert_eq!(config.couchdb.url, "original");
        assert!(config.couchdb.auth.is_none());
    }
}
