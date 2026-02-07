use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

use couchfs::cli;
use couchfs::config::AppConfig;
use couchfs::models::ResolutionStrategy;

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
        /// Directory to sync (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Dry run (don't make changes)
        #[arg(long)]
        dry_run: bool,
    },

    /// Run continuous sync daemon
    Daemon {
        /// Directory to sync (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Poll interval in seconds
        #[arg(short, long, default_value = "60")]
        interval: u64,
    },

    /// List conflicts
    Conflicts {
        /// Directory to check (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Resolve a conflict
    Resolve {
        /// Path to the conflicted file
        path: PathBuf,

        /// Resolution strategy
        #[arg(short, long)]
        strategy: ResolutionStrategy,

        /// Working directory (default: current directory)
        #[arg(default_value = ".")]
        work_dir: PathBuf,
    },

    /// Show sync status
    Status {
        /// Directory to check (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,

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
            cli::sync(path, config, dry_run).await?;
        }
        Commands::Daemon { path, interval } => {
            cli::daemon(path, config, interval).await?;
        }
        Commands::Conflicts { path, json } => {
            cli::conflicts(path, json).await?;
        }
        Commands::Resolve {
            path,
            strategy,
            work_dir,
        } => {
            cli::resolve(work_dir, path, strategy, config).await?;
        }
        Commands::Status { path, json } => {
            cli::status(path, json, &config).await?;
        }
    }

    Ok(())
}

/// Initialize logging based on verbosity
fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };

    tracing_subscriber::fmt()
        .with_env_filter(format!("couchfs={}", level))
        .init();
}
