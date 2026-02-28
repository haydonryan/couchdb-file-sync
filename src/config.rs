use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A sync path pair mapping local directory to remote path
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyncPath {
    /// Local directory path
    pub local: PathBuf,
    /// Remote path prefix in CouchDB (e.g., "notes/" or "obsidian/")
    #[serde(default)]
    pub remote: String,
}

/// Application configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AppConfig {
    #[serde(default)]
    pub couchdb: CouchDbConfig,
    #[serde(default)]
    pub sync: SyncConfig,
    /// Multiple sync path pairs (local -> remote)
    #[serde(default)]
    pub paths: Vec<SyncPath>,
    #[serde(default)]
    pub ignore: IgnoreConfig,
    #[serde(default)]
    pub conflicts: ConflictConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl AppConfig {
    /// Load configuration from file and environment
    pub fn load(config_path: Option<PathBuf>) -> Result<Self> {
        let mut config_builder = config::Config::builder();

        // Add config file if specified or found
        if let Some(path) = config_path {
            config_builder = config_builder.add_source(config::File::from(path));
        } else if let Some(path) = Self::find_config_file() {
            config_builder = config_builder.add_source(config::File::from(path));
        }

        // Add environment variables with COUCHFS_ prefix
        config_builder = config_builder.add_source(
            config::Environment::with_prefix("COUCHFS")
                .separator("__")
                .try_parsing(true),
        );

        // Build and deserialize
        let config = config_builder.build()?;
        let app_config: AppConfig = config.try_deserialize()?;

        Ok(app_config)
    }

    /// Find config file in current directory or parent directories
    fn find_config_file() -> Option<PathBuf> {
        let filenames = [
            "couchfs.yaml",
            "couchfs.yml",
            ".couchfs.yaml",
            ".couchfs.yml",
            ".couchfs/couchfs.yaml",
        ];

        let mut current_dir = std::env::current_dir().ok()?;

        loop {
            for filename in &filenames {
                let path = current_dir.join(filename);
                if path.exists() {
                    return Some(path);
                }
            }

            // Go up one directory
            match current_dir.parent() {
                Some(parent) => current_dir = parent.to_path_buf(),
                None => break,
            }
        }

        None
    }
}

/// CouchDB connection configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CouchDbConfig {
    #[serde(default = "default_db_url")]
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_db_name")]
    pub database: String,
    /// Remote path to sync (e.g., "notes/" or "obsidian/"). Empty means sync all.
    #[serde(default)]
    pub remote_path: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_retry")]
    pub retry_attempts: u32,
}

impl Default for CouchDbConfig {
    fn default() -> Self {
        Self {
            url: default_db_url(),
            username: None,
            password: None,
            database: default_db_name(),
            remote_path: String::new(),
            timeout_seconds: default_timeout(),
            retry_attempts: default_retry(),
        }
    }
}

/// Sync behavior configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SyncConfig {
    pub root_dir: Option<PathBuf>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval: u64,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    #[serde(default = "default_parallel")]
    pub parallel: bool,
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            root_dir: None,
            poll_interval: default_poll_interval(),
            debounce_ms: default_debounce_ms(),
            batch_size: default_batch_size(),
            max_file_size: default_max_file_size(),
            parallel: default_parallel(),
            max_parallel: default_max_parallel(),
        }
    }
}

/// Ignore patterns configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IgnoreConfig {
    pub patterns: Vec<String>,
    pub ignore_files: Vec<String>,
}

/// Conflict resolution configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConflictConfig {
    #[serde(default = "default_conflict_strategy")]
    pub default_strategy: String,
    #[serde(default)]
    pub auto_resolve: bool,
    pub conflict_dir: Option<PathBuf>,
}

impl Default for ConflictConfig {
    fn default() -> Self {
        Self {
            default_strategy: default_conflict_strategy(),
            auto_resolve: false,
            conflict_dir: None,
        }
    }
}

/// Notification configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NotificationConfig {
    pub enabled: bool,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub notify_on_conflict: bool,
    #[serde(default)]
    pub notify_on_sync_error: bool,
    #[serde(default)]
    pub notify_summary: bool,
}

/// Telegram notification configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
}

/// Logging configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_format")]
    pub format: String,
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            file: None,
        }
    }
}

// Default value functions
fn default_db_url() -> String {
    "http://localhost:5984".to_string()
}

fn default_db_name() -> String {
    "couchfs_files".to_string()
}

fn default_timeout() -> u64 {
    30
}

fn default_retry() -> u32 {
    3
}

fn default_poll_interval() -> u64 {
    60
}

fn default_debounce_ms() -> u64 {
    500
}

fn default_batch_size() -> usize {
    100
}

fn default_max_file_size() -> u64 {
    1024 * 1024 * 1024 // 1GB
}

fn default_parallel() -> bool {
    true
}

fn default_max_parallel() -> usize {
    4
}

fn default_conflict_strategy() -> String {
    "keep-both".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "pretty".to_string()
}
