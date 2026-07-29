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
        if let Some(path) = config_path {
            config_builder = config_builder.add_source(config::File::from(path));
        } else if let Some(path) = Self::find_config_file() {
            config_builder = config_builder.add_source(config::File::from(path));
        }

        // Add environment variables with COUCHDB_FILE_SYNC_ prefix
        config_builder = config_builder.add_source(
            config::Environment::with_prefix("COUCHDB_FILE_SYNC")
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
        default_user_config_candidates()
            .into_iter()
            .find(|path| path.exists())
    }
}

pub fn default_user_config_dir() -> Option<PathBuf> {
    let home_dir = std::env::var_os("HOME").map(PathBuf::from)?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir.join(".config"));
    Some(config_home.join("couchdb-file-sync"))
}

pub fn default_user_config_file() -> Option<PathBuf> {
    default_user_config_dir().map(|dir| dir.join("couchdb-file-sync.yaml"))
}

pub fn default_user_state_dir() -> Option<PathBuf> {
    let home_dir = std::env::var_os("HOME").map(PathBuf::from)?;
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir.join(".local").join("state"));
    Some(state_home.join("couchdb-file-sync"))
}

pub fn default_log_file() -> Option<PathBuf> {
    default_user_state_dir().map(|dir| dir.join("couchdb-file-sync.log"))
}

fn default_user_config_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(config_dir) = default_user_config_dir() {
        paths.push(config_dir.join("couchdb-file-sync.yaml"));
        paths.push(config_dir.join("couchdb-file-sync.yml"));
    }
    paths
}

// ─── Enums ─────────────────────────────────────────────────────────────────

/// Log level enum replacing arbitrary String
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Return the tracing-compatible filter string
    pub fn as_filter_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// Log format enum replacing arbitrary String
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
    Compact,
}

/// Conflict resolution strategy enum replacing arbitrary String
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictStrategy {
    #[default]
    KeepBoth,
    KeepLocal,
    KeepRemote,
    Skip,
}

impl ConflictStrategy {
    /// Return the serialized form (matches serde rename)
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictStrategy::KeepBoth => "keep-both",
            ConflictStrategy::KeepLocal => "keep-local",
            ConflictStrategy::KeepRemote => "keep-remote",
            ConflictStrategy::Skip => "skip",
        }
    }
}

/// Matrix message type enum replacing arbitrary String
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum MatrixMessageType {
    #[default]
    #[serde(rename = "m.notice")]
    Notice,
    #[serde(rename = "m.text")]
    Text,
    #[serde(rename = "m.emote")]
    Emote,
}

impl MatrixMessageType {
    /// Return the Matrix wire format string
    pub fn as_str(&self) -> &'static str {
        match self {
            MatrixMessageType::Notice => "m.notice",
            MatrixMessageType::Text => "m.text",
            MatrixMessageType::Emote => "m.emote",
        }
    }
}

/// Rotation config combining mode and policy into a single enum
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum RotationConfig {
    /// No log rotation
    #[default]
    Never,
    /// Rotate daily and keep old files
    DailyKeep,
    /// Rotate daily and delete old files after one cycle
    DailyDelete,
}

// ─── Credential structs ────────────────────────────────────────────────────

/// Unified Telegram credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramCredentials {
    pub bot_token: String,
    pub chat_id: String,
}

/// Unified Matrix credentials
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixCredentials {
    pub homeserver_url: String,
    pub access_token: String,
    pub room_id: String,
}

/// Unified CouchDB authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouchDbAuth {
    pub username: String,
    pub password: String,
}

// ─── Config structs ────────────────────────────────────────────────────────

/// CouchDB connection configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CouchDbConfig {
    #[serde(default = "default_db_url")]
    pub url: String,
    #[serde(default)]
    pub auth: Option<CouchDbAuth>,
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
            auth: None,
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
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConflictConfig {
    #[serde(default)]
    pub default_strategy: ConflictStrategy,
    #[serde(default)]
    pub auto_resolve: bool,
    pub conflict_dir: Option<PathBuf>,
}

/// Notification configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NotificationConfig {
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub notify_on_conflict: bool,
    #[serde(default)]
    pub notify_on_sync_error: bool,
}

/// Telegram notification configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub credentials: Option<TelegramCredentials>,
}

/// Matrix notification configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MatrixConfig {
    #[serde(default)]
    pub credentials: Option<MatrixCredentials>,
    #[serde(default)]
    pub message_type: MatrixMessageType,
}

/// Logging configuration
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: LogLevel,
    #[serde(default)]
    pub format: LogFormat,
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub rotation: RotationConfig,
}

// Default value functions (kept for backward compat with serde defaults)
fn default_db_url() -> String {
    "http://localhost:5984".to_string()
}

fn default_db_name() -> String {
    "couchdb_file_sync_files".to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn default_user_config_dir_uses_xdg_config_home_when_set() {
        let dir = resolve_user_config_dir(
            Some(Path::new("/home/tester")),
            Some(Path::new("/tmp/xdg-config")),
        )
        .unwrap();

        assert_eq!(dir, PathBuf::from("/tmp/xdg-config/couchdb-file-sync"));
    }

    #[test]
    fn default_user_config_dir_falls_back_to_home_config() {
        let dir = resolve_user_config_dir(Some(Path::new("/home/tester")), None).unwrap();

        assert_eq!(dir, PathBuf::from("/home/tester/.config/couchdb-file-sync"));
    }

    #[test]
    fn default_user_config_dir_requires_home() {
        assert!(resolve_user_config_dir(None, Some(Path::new("/tmp/xdg-config"))).is_none());
    }

    fn resolve_user_config_dir(
        home_dir: Option<&Path>,
        xdg_config_home: Option<&Path>,
    ) -> Option<PathBuf> {
        let home_dir = home_dir?.to_path_buf();
        let config_home = xdg_config_home
            .map(Path::to_path_buf)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| home_dir.join(".config"));
        Some(config_home.join("couchdb-file-sync"))
    }

    // ---- Enum tests ----

    #[test]
    fn test_log_level_default() {
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn test_log_level_filter_str() {
        assert_eq!(LogLevel::Trace.as_filter_str(), "trace");
        assert_eq!(LogLevel::Debug.as_filter_str(), "debug");
        assert_eq!(LogLevel::Info.as_filter_str(), "info");
        assert_eq!(LogLevel::Warn.as_filter_str(), "warn");
        assert_eq!(LogLevel::Error.as_filter_str(), "error");
    }

    #[test]
    fn test_log_format_default() {
        assert_eq!(LogFormat::default(), LogFormat::Pretty);
    }

    #[test]
    fn test_conflict_strategy_default() {
        assert_eq!(ConflictStrategy::default(), ConflictStrategy::KeepBoth);
    }

    #[test]
    fn test_conflict_strategy_as_str() {
        assert_eq!(ConflictStrategy::KeepBoth.as_str(), "keep-both");
        assert_eq!(ConflictStrategy::KeepLocal.as_str(), "keep-local");
        assert_eq!(ConflictStrategy::KeepRemote.as_str(), "keep-remote");
        assert_eq!(ConflictStrategy::Skip.as_str(), "skip");
    }

    #[test]
    fn test_matrix_message_type_default() {
        assert_eq!(MatrixMessageType::default(), MatrixMessageType::Notice);
    }

    #[test]
    fn test_matrix_message_type_as_str() {
        assert_eq!(MatrixMessageType::Notice.as_str(), "m.notice");
        assert_eq!(MatrixMessageType::Text.as_str(), "m.text");
        assert_eq!(MatrixMessageType::Emote.as_str(), "m.emote");
    }

    #[test]
    fn test_rotation_config_default() {
        assert_eq!(RotationConfig::default(), RotationConfig::Never);
    }

    #[test]
    fn test_rotation_config_serde_round_trip() {
        let variants = [
            RotationConfig::Never,
            RotationConfig::DailyKeep,
            RotationConfig::DailyDelete,
        ];
        for variant in &variants {
            let yaml = serde_yaml::to_string(variant).unwrap();
            let deserialized: RotationConfig = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(*variant, deserialized);
        }
    }

    #[test]
    fn test_log_level_serde_lowercase() {
        let yaml = "debug\n";
        let level: LogLevel = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(level, LogLevel::Debug);
    }

    #[test]
    fn test_conflict_strategy_serde_kebab() {
        let yaml = "keep-local\n";
        let strategy: ConflictStrategy = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(strategy, ConflictStrategy::KeepLocal);
    }

    #[test]
    fn test_matrix_message_type_serde() {
        let yaml = "m.text\n";
        let msg_type: MatrixMessageType = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(msg_type, MatrixMessageType::Text);
    }

    // ---- Credential struct tests ----

    #[test]
    fn test_telegram_credentials() {
        let creds = TelegramCredentials {
            bot_token: "123:abc".into(),
            chat_id: "-456".into(),
        };
        let yaml = serde_yaml::to_string(&creds).unwrap();
        let deserialized: TelegramCredentials = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.bot_token, "123:abc");
        assert_eq!(deserialized.chat_id, "-456");
    }

    #[test]
    fn test_matrix_credentials() {
        let creds = MatrixCredentials {
            homeserver_url: "https://matrix.example.com".into(),
            access_token: "syt_token".into(),
            room_id: "!room:example.com".into(),
        };
        let yaml = serde_yaml::to_string(&creds).unwrap();
        let deserialized: MatrixCredentials = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.homeserver_url, "https://matrix.example.com");
        assert_eq!(deserialized.access_token, "syt_token");
        assert_eq!(deserialized.room_id, "!room:example.com");
    }

    #[test]
    fn test_couchdb_auth() {
        let auth = CouchDbAuth {
            username: "admin".into(),
            password: "secret".into(),
        };
        let yaml = serde_yaml::to_string(&auth).unwrap();
        let deserialized: CouchDbAuth = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.username, "admin");
        assert_eq!(deserialized.password, "secret");
    }

    // ---- AppConfig default tests ----

    #[test]
    fn test_app_config_default_has_sensible_values() {
        let config = AppConfig::default();

        // CouchDB defaults
        assert_eq!(config.couchdb.url, "http://localhost:5984");
        assert_eq!(config.couchdb.database, "couchdb_file_sync_files");
        assert_eq!(config.couchdb.timeout_seconds, 30);
        assert_eq!(config.couchdb.retry_attempts, 3);
        assert!(config.couchdb.auth.is_none());
        assert_eq!(config.couchdb.remote_path, "");

        // Sync defaults
        assert!(config.sync.root_dir.is_none());
        assert_eq!(config.sync.poll_interval, 60);
        assert_eq!(config.sync.debounce_ms, 500);
        assert_eq!(config.sync.batch_size, 100);
        assert_eq!(config.sync.max_file_size, 1024 * 1024 * 1024);
        assert!(config.sync.parallel);
        assert_eq!(config.sync.max_parallel, 4);

        // Empty by default
        assert!(config.paths.is_empty());
        assert!(config.ignore.patterns.is_empty());
        assert!(config.ignore.ignore_files.is_empty());

        // Conflict defaults
        assert_eq!(
            config.conflicts.default_strategy,
            ConflictStrategy::KeepBoth
        );
        assert!(!config.conflicts.auto_resolve);
        assert!(config.conflicts.conflict_dir.is_none());

        // Logging defaults
        assert_eq!(config.logging.level, LogLevel::Info);
        assert_eq!(config.logging.format, LogFormat::Pretty);
        assert!(config.logging.file.is_none());
        assert_eq!(config.logging.rotation, RotationConfig::Never);

        // Notifications defaults
        assert!(config.notifications.telegram.credentials.is_none());
        assert!(config.notifications.matrix.credentials.is_none());
        assert!(!config.notifications.notify_on_conflict);
        assert!(!config.notifications.notify_on_sync_error);
    }

    #[test]
    fn test_couchdb_config_default_uses_provided_defaults() {
        let couch = CouchDbConfig::default();
        assert_eq!(couch.url, "http://localhost:5984");
        assert_eq!(couch.database, "couchdb_file_sync_files");
        assert_eq!(couch.timeout_seconds, 30);
        assert_eq!(couch.retry_attempts, 3);
    }

    #[test]
    fn test_sync_config_default_values() {
        let sync = SyncConfig::default();
        assert!(sync.root_dir.is_none());
        assert_eq!(sync.poll_interval, 60);
        assert_eq!(sync.debounce_ms, 500);
        assert_eq!(sync.batch_size, 100);
        assert_eq!(sync.max_file_size, 1024 * 1024 * 1024);
        assert!(sync.parallel);
        assert_eq!(sync.max_parallel, 4);
    }

    #[test]
    fn test_conflict_config_default() {
        let conflict = ConflictConfig::default();
        assert_eq!(conflict.default_strategy, ConflictStrategy::KeepBoth);
        assert!(!conflict.auto_resolve);
        assert!(conflict.conflict_dir.is_none());
    }

    #[test]
    fn test_logging_config_default() {
        let log = LoggingConfig::default();
        assert_eq!(log.level, LogLevel::Info);
        assert_eq!(log.format, LogFormat::Pretty);
        assert!(log.file.is_none());
        assert_eq!(log.rotation, RotationConfig::Never);
    }

    // ---- SyncPath tests ----

    #[test]
    fn test_sync_path_construction() {
        let sync_path = SyncPath {
            local: PathBuf::from("/home/user/docs"),
            remote: "notes/".to_string(),
        };
        assert_eq!(sync_path.local, PathBuf::from("/home/user/docs"));
        assert_eq!(sync_path.remote, "notes/");
    }

    #[test]
    fn test_sync_path_default_remote() {
        // When remote is not serialized, it should default to empty string
        let yaml = r#"
local: /home/user/docs
"#;
        let sync_path: SyncPath = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(sync_path.local, PathBuf::from("/home/user/docs"));
        assert_eq!(
            sync_path.remote, "",
            "remote should default to empty string"
        );
    }

    #[test]
    fn test_sync_path_round_trip() {
        let original = SyncPath {
            local: PathBuf::from("/data/photos"),
            remote: "photos/".to_string(),
        };
        let yaml = serde_yaml::to_string(&original).unwrap();
        let deserialized: SyncPath = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(original.local, deserialized.local);
        assert_eq!(original.remote, deserialized.remote);
    }

    // ---- AppConfig::load tests ----

    #[test]
    fn test_app_config_load_returns_defaults_when_no_file() {
        // When no config file is given, load should succeed.
        let result = AppConfig::load(None);
        assert!(
            result.is_ok(),
            "load should succeed without config file: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_app_config_load_with_missing_file_path_fails_gracefully() {
        // The config crate's File::from() requires the file to exist.
        // load() will return an error when given a nonexistent path.
        let result = AppConfig::load(Some(PathBuf::from(
            "/definitely/does/not/exist/config.yaml",
        )));
        assert!(
            result.is_err(),
            "load with nonexistent file path should return Err"
        );
        // But calling with None should work (no file, just env)
        let result = AppConfig::load(None);
        assert!(result.is_ok(), "load without file path should succeed");
    }

    #[test]
    fn test_app_config_env_overrides() {
        // Save all potentially conflicting env vars
        let old_url = std::env::var_os("COUCHDB_FILE_SYNC__COUCHDB__URL");
        let old_interval = std::env::var_os("COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL");
        let old_log = std::env::var_os("COUCHDB_FILE_SYNC__LOGGING__LEVEL");

        // Clear interfering env vars
        std::env::remove_var("COUCHDB_FILE_SYNC__COUCHDB__URL");
        std::env::remove_var("COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL");
        std::env::remove_var("COUCHDB_FILE_SYNC__LOGGING__LEVEL");

        // --- Test 1: Env vars override defaults ---
        std::env::set_var(
            "COUCHDB_FILE_SYNC__COUCHDB__URL",
            "https://couch.example.com:6984",
        );
        std::env::set_var("COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL", "120");
        std::env::set_var("COUCHDB_FILE_SYNC__LOGGING__LEVEL", "debug");

        let config = AppConfig::load(None).unwrap();
        assert_eq!(config.couchdb.url, "https://couch.example.com:6984");
        assert_eq!(config.sync.poll_interval, 120);
        assert_eq!(config.logging.level, LogLevel::Debug);

        // --- Test 2: Env var overrides config file ---
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test-config.yaml");
        std::fs::write(
            &config_path,
            r#"
couchdb:
  url: "http://file-value:5984"
  database: "from_file"
sync:
  poll_interval: 99
logging:
  level: "warn"
"#,
        )
        .unwrap();

        // Env var overrides file value
        std::env::set_var("COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL", "200");

        let config = AppConfig::load(Some(config_path)).unwrap();

        // File value should be present for non-overridden keys
        assert_eq!(config.couchdb.url, "https://couch.example.com:6984");
        assert_eq!(config.couchdb.database, "from_file");
        assert_eq!(config.logging.level, LogLevel::Debug);

        // Env overrides file for poll_interval
        assert_eq!(config.sync.poll_interval, 200);

        // --- Restore original env vars ---
        if let Some(v) = old_url {
            std::env::set_var("COUCHDB_FILE_SYNC__COUCHDB__URL", v);
        } else {
            std::env::remove_var("COUCHDB_FILE_SYNC__COUCHDB__URL");
        }
        if let Some(v) = old_interval {
            std::env::set_var("COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL", v);
        } else {
            std::env::remove_var("COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL");
        }
        if let Some(v) = old_log {
            std::env::set_var("COUCHDB_FILE_SYNC__LOGGING__LEVEL", v);
        } else {
            std::env::remove_var("COUCHDB_FILE_SYNC__LOGGING__LEVEL");
        }
    }

    // ---- CouchDB auth deserialization ----

    #[test]
    fn test_couchdb_config_with_auth() {
        let yaml = r#"
url: "https://couch.example.com:6984"
auth:
  username: admin
  password: secret
database: my_db
"#;
        let config: CouchDbConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.url, "https://couch.example.com:6984");
        assert!(config.auth.is_some());
        let auth = config.auth.unwrap();
        assert_eq!(auth.username, "admin");
        assert_eq!(auth.password, "secret");
        assert_eq!(config.database, "my_db");
    }

    #[test]
    fn test_couchdb_config_without_auth() {
        let yaml = r#"
url: "https://couch.example.com:6984"
database: my_db
"#;
        let config: CouchDbConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.url, "https://couch.example.com:6984");
        assert!(config.auth.is_none());
        assert_eq!(config.database, "my_db");
    }

    // ---- Telegram config deserialization ----

    #[test]
    fn test_telegram_config_with_credentials() {
        let yaml = r#"
credentials:
  bot_token: "123:abc"
  chat_id: "-456"
"#;
        let config: TelegramConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.credentials.is_some());
        let creds = config.credentials.unwrap();
        assert_eq!(creds.bot_token, "123:abc");
        assert_eq!(creds.chat_id, "-456");
    }

    #[test]
    fn test_telegram_config_without_credentials() {
        let config = TelegramConfig::default();
        assert!(config.credentials.is_none());
    }

    // ---- Matrix config deserialization ----

    #[test]
    fn test_matrix_config_with_credentials() {
        let yaml = r#"
credentials:
  homeserver_url: "https://matrix.example.com"
  access_token: "syt_token"
  room_id: "!room:example.com"
message_type: "m.text"
"#;
        let config: MatrixConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.credentials.is_some());
        let creds = config.credentials.unwrap();
        assert_eq!(creds.homeserver_url, "https://matrix.example.com");
        assert_eq!(creds.access_token, "syt_token");
        assert_eq!(creds.room_id, "!room:example.com");
        assert_eq!(config.message_type, MatrixMessageType::Text);
    }

    #[test]
    fn test_matrix_config_default_message_type() {
        let yaml = r#"
credentials:
  homeserver_url: "https://matrix.example.com"
  access_token: "syt_token"
  room_id: "!room:example.com"
"#;
        let config: MatrixConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.message_type, MatrixMessageType::Notice);
    }
}
