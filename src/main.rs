use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

use couchfs::cli;
use couchfs::config::{AppConfig, SyncPath};

#[derive(Parser, Debug)]
#[command(name = "couchfs")]
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
    #[arg(long, global = true, env = "COUCHFS_DB_URL")]
    db_url: Option<String>,

    /// CouchDB username
    #[arg(long, global = true, env = "COUCHFS_DB_USERNAME")]
    db_user: Option<String>,

    /// CouchDB password
    #[arg(long, global = true, env = "COUCHFS_DB_PASSWORD")]
    db_pass: Option<String>,

    /// CouchDB database name
    #[arg(long, global = true, env = "COUCHFS_DB_NAME")]
    db_name: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a new sync directory
    Init {
        /// Directory to initialize (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    init_logging(cli.verbose);

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

    // Execute command
    match cli.command {
        Commands::Init {
            path,
            db_url,
            db_name,
        } => {
            cli::init(path, db_url, db_name).await?;
        }
        Commands::Sync { path, dry_run } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchfs.yaml"
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
                    "No sync paths configured. Specify a path or add paths to couchfs.yaml"
                );
            }
            cli::daemon(paths, config, interval, live).await?;
        }
        Commands::Conflicts { path, json } => {
            let paths = resolve_paths(path, &config);
            if paths.is_empty() {
                anyhow::bail!(
                    "No sync paths configured. Specify a path or add paths to couchfs.yaml"
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
                    "No sync paths configured. Specify a path or add paths to couchfs.yaml"
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
                    "No sync paths configured. Specify a path or add paths to couchfs.yaml"
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
    }

    Ok(())
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
fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;

    // Prefer RUST_LOG if set, otherwise use verbosity flag
    let filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        let level = match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("couchfs={}", level))
    };

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
