use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

use couchdb_file_sync::cli;
use couchdb_file_sync::config::{default_log_file, default_user_config_file, AppConfig, SyncPath};
use couchdb_file_sync::logging::AppLogWriter;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.verbose > 0 {
        match resolved_config_path(cli.config.clone()) {
            Some((path, source)) => {
                eprintln!("Using config file ({source}): {}", path.display())
            }
            None => eprintln!("No config file found; using defaults and environment overrides"),
        }
    }

    // Load configuration
    let mut config = match AppConfig::load(cli.config) {
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

/// Initialize logging based on verbosity or RUST_LOG env var
fn init_logging(verbose: u8, config: &AppConfig, enable_file_logging: bool, daemon_mode: bool) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;

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
    use super::{
        default_user_config_file_if_exists, paths_match, resolve_paths, resolved_config_path,
    };
    use super::{init_logging, Cli, Commands};
    use clap::Parser;
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
        std::env::remove_var("XDG_CONFIG_HOME");
        f();
        // Restore
        if let Some(ref h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(ref x) = old_xdg {
            std::env::set_var("XDG_CONFIG_HOME", x);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
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
            std::env::set_var("HOME", &fake_home);
            std::env::remove_var("XDG_CONFIG_HOME");

            let result = default_user_config_file_if_exists();
            assert!(
                result.is_none(),
                "expected None for missing config, got {:?}",
                result
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

            std::env::set_var("HOME", tmp.path());
            std::env::remove_var("XDG_CONFIG_HOME");

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

            std::env::set_var("HOME", tmp.path());
            std::env::remove_var("XDG_CONFIG_HOME");

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

            std::env::set_var("HOME", tmp.path());
            std::env::remove_var("XDG_CONFIG_HOME");

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
            std::env::set_var("HOME", &fake_home);
            std::env::remove_var("XDG_CONFIG_HOME");

            let result = resolved_config_path(None);
            assert!(result.is_none());
        });
    }

    // ============================================================
    // init_logging tests
    // ============================================================

    /// Smoke test: init_logging with default settings should not panic.
    /// Note: Only one init_logging test is included because tracing_subscriber::init()
    /// can only be called once per process. Running multiple init_logging tests would
    /// require separate test binaries or using try_init() instead of init().
    #[test]
    fn init_logging_smoke_test_default_verbose() {
        let config = AppConfig::default();
        init_logging(0, &config, false, false);
    }

    // ============================================================

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

    // --- Init subcommand ---

    #[test]
    fn cli_parses_init_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "init"]).unwrap();
        assert!(matches!(cli.command, Commands::Init { path: None, .. }));
    }

    #[test]
    fn cli_parses_init_with_path() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "init", "/my/path"]).unwrap();
        assert!(matches!(cli.command, Commands::Init {
            path: Some(ref p), ..
        } if p == &PathBuf::from("/my/path")));
    }

    #[test]
    fn cli_parses_init_with_path_db_url_db_name() {
        let cli = Cli::try_parse_from([
            "couchdb-file-sync",
            "init",
            "/my/path",
            "--db-url",
            "https://couch.example.com:6984",
            "--db-name",
            "my_database",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::Init {
            path: Some(ref p),
            ref db_url,
            ref db_name,
        } if p == &PathBuf::from("/my/path")
            && db_url.as_deref() == Some("https://couch.example.com:6984")
            && db_name.as_deref() == Some("my_database")));
    }

    #[test]
    fn cli_parses_init_with_only_db_url() {
        let cli = Cli::try_parse_from([
            "couchdb-file-sync",
            "init",
            "--db-url",
            "https://couch.example.com:6984",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::Init {
            path: None,
            ref db_url,
            ..
        } if db_url.as_deref() == Some("https://couch.example.com:6984")));
    }

    // --- Sync subcommand ---

    #[test]
    fn cli_parses_sync_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "sync"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Sync {
                path: None,
                dry_run: false,
            }
        ));
    }

    #[test]
    fn cli_parses_sync_with_path() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "sync", "/data/docs"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync {
            path: Some(ref p), ..
        } if p == &PathBuf::from("/data/docs")));
    }

    #[test]
    fn cli_parses_sync_with_dry_run() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "sync", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync { dry_run: true, .. }));
    }

    #[test]
    fn cli_parses_sync_with_path_and_dry_run() {
        let cli =
            Cli::try_parse_from(["couchdb-file-sync", "sync", "/data/docs", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Commands::Sync {
            path: Some(ref p),
            dry_run: true,
        } if p == &PathBuf::from("/data/docs")));
    }

    // --- Daemon subcommand ---

    #[test]
    fn cli_parses_daemon_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "daemon"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Daemon {
                path: None,
                interval: 60,
                live: false,
            }
        ));
    }

    #[test]
    fn cli_parses_daemon_with_interval() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "daemon", "--interval", "30"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon { interval: 30, .. }));
    }

    #[test]
    fn cli_parses_daemon_with_live_flag() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "daemon", "--live"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon { live: true, .. }));
    }

    #[test]
    fn cli_parses_daemon_with_path_interval_live() {
        let cli = Cli::try_parse_from([
            "couchdb-file-sync",
            "daemon",
            "/my/path",
            "--interval",
            "120",
            "--live",
        ])
        .unwrap();
        assert!(matches!(cli.command, Commands::Daemon {
            path: Some(ref p),
            interval: 120,
            live: true,
        } if p == &PathBuf::from("/my/path")));
    }

    #[test]
    fn cli_parses_daemon_with_short_interval() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "daemon", "-i", "15"]).unwrap();
        assert!(matches!(cli.command, Commands::Daemon { interval: 15, .. }));
    }

    // --- Conflicts subcommand ---

    #[test]
    fn cli_parses_conflicts_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "conflicts"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Conflicts {
                path: None,
                json: false,
            }
        ));
    }

    #[test]
    fn cli_parses_conflicts_with_json() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "conflicts", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Conflicts { json: true, .. }
        ));
    }

    #[test]
    fn cli_parses_conflicts_with_path() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "conflicts", "/data"]).unwrap();
        assert!(matches!(cli.command, Commands::Conflicts {
            path: Some(ref p), ..
        } if p == &PathBuf::from("/data")));
    }

    #[test]
    fn cli_parses_conflicts_with_path_and_json() {
        let cli =
            Cli::try_parse_from(["couchdb-file-sync", "conflicts", "/data", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Conflicts {
            path: Some(ref p),
            json: true,
        } if p == &PathBuf::from("/data")));
    }

    // --- Resolve subcommand ---

    #[test]
    fn cli_parses_resolve_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "resolve"]).unwrap();
        assert!(matches!(cli.command, Commands::Resolve { path: None }));
    }

    #[test]
    fn cli_parses_resolve_with_path() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "resolve", "/data"]).unwrap();
        assert!(matches!(cli.command, Commands::Resolve {
            path: Some(ref p),
        } if p == &PathBuf::from("/data")));
    }

    // --- Status subcommand ---

    #[test]
    fn cli_parses_status_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Status {
                path: None,
                json: false,
            }
        ));
    }

    #[test]
    fn cli_parses_status_with_json() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "status", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Status { json: true, .. }));
    }

    #[test]
    fn cli_parses_status_with_path() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "status", "/data"]).unwrap();
        assert!(matches!(cli.command, Commands::Status {
            path: Some(ref p), ..
        } if p == &PathBuf::from("/data")));
    }

    #[test]
    fn cli_parses_status_with_path_and_json() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "status", "/data", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Status {
            path: Some(ref p),
            json: true,
        } if p == &PathBuf::from("/data")));
    }

    // --- RebuildRemote subcommand ---

    #[test]
    fn cli_parses_rebuild_remote_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "rebuild-remote"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::RebuildRemote { path: None }
        ));
    }

    // --- RebuildLocal subcommand ---

    #[test]
    fn cli_parses_rebuild_local_no_args() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "rebuild-local"]).unwrap();
        assert!(matches!(cli.command, Commands::RebuildLocal { path: None }));
    }

    // --- Install subcommand ---

    #[test]
    fn cli_parses_install() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "install"]).unwrap();
        assert!(matches!(cli.command, Commands::Install));
    }

    // --- Uninstall subcommand ---

    #[test]
    fn cli_parses_uninstall() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "uninstall"]).unwrap();
        assert!(matches!(cli.command, Commands::Uninstall));
    }

    // --- Global args ---

    #[test]
    fn cli_parses_global_verbose_count() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "-v", "status"]).unwrap();
        assert_eq!(cli.verbose, 1);

        let cli = Cli::try_parse_from(["couchdb-file-sync", "-vv", "status"]).unwrap();
        assert_eq!(cli.verbose, 2);

        let cli = Cli::try_parse_from(["couchdb-file-sync", "-vvv", "status"]).unwrap();
        assert_eq!(cli.verbose, 3);
    }

    #[test]
    fn cli_parses_global_db_url() {
        let cli = Cli::try_parse_from([
            "couchdb-file-sync",
            "--db-url",
            "https://example.com:5984",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.db_url.as_deref(), Some("https://example.com:5984"));
    }

    #[test]
    fn cli_parses_global_db_user_and_db_pass() {
        let cli = Cli::try_parse_from([
            "couchdb-file-sync",
            "--db-user",
            "admin",
            "--db-pass",
            "secret",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.db_user.as_deref(), Some("admin"));
        assert_eq!(cli.db_pass.as_deref(), Some("secret"));
    }

    #[test]
    fn cli_parses_global_db_name() {
        let cli =
            Cli::try_parse_from(["couchdb-file-sync", "--db-name", "my_db", "status"]).unwrap();
        assert_eq!(cli.db_name.as_deref(), Some("my_db"));
    }

    #[test]
    fn cli_parses_global_config_path() {
        let cli = Cli::try_parse_from([
            "couchdb-file-sync",
            "--config",
            "/path/to/config.yaml",
            "status",
        ])
        .unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(PathBuf::from("/path/to/config.yaml").as_path())
        );
    }

    #[test]
    fn cli_parses_global_verbose_with_subcommand() {
        let cli = Cli::try_parse_from(["couchdb-file-sync", "-vv", "sync", "--dry-run"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(matches!(cli.command, Commands::Sync { dry_run: true, .. }));
    }

    #[test]
    fn cli_parses_long_verbose_with_subcommand() {
        let cli =
            Cli::try_parse_from(["couchdb-file-sync", "--verbose", "daemon", "--live"]).unwrap();
        assert_eq!(cli.verbose, 1);
        assert!(matches!(cli.command, Commands::Daemon { live: true, .. }));
    }

    #[test]
    fn cli_rejects_invalid_subcommand() {
        let result = Cli::try_parse_from(["couchdb-file-sync", "invalid-cmd"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_rejects_unknown_flag() {
        let result = Cli::try_parse_from(["couchdb-file-sync", "status", "--unknown-flag"]);
        assert!(result.is_err());
    }
}
