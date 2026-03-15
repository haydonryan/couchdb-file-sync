use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

use couchdb_file_sync::cli;
use couchdb_file_sync::config::{default_user_config_file, AppConfig, SyncPath};

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
        Commands::Sync { .. } | Commands::Daemon { .. }
    );

    // Initialize logging
    init_logging(cli.verbose, &config, enable_file_logging);

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
            // CLI path specified - use it with the config's remote_path
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

/// Initialize logging based on verbosity or RUST_LOG env var
fn init_logging(verbose: u8, config: &AppConfig, enable_file_logging: bool) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::Layer;

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
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/couchfs.logs"));
        if let Some(parent) = log_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("Failed to create log directory {}: {}", parent.display(), e);
                }
            }
        }
        let file_name = log_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("couchfs.logs");
        let dir = log_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        let file_appender = tracing_appender::rolling::never(dir, file_name);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
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
