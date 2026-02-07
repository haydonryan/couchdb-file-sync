use crate::config::AppConfig;
use crate::couchdb::CouchDb;
use crate::local::LocalDb;
use crate::models::{IgnoreMatcher, ResolutionStrategy};
use crate::sync::{SyncEngine, SyncReport};
use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{error, info};

/// Initialize a new sync directory
pub async fn init(path: PathBuf, _db_url: Option<String>, _db_name: Option<String>) -> Result<()> {
    info!("Initializing CouchFS directory: {}", path.display());

    // Create directory if it doesn't exist
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }

    // Create .couchfs subdirectory for state
    let couchfs_dir = path.join(".couchfs");
    std::fs::create_dir_all(&couchfs_dir)?;

    // Create couchfs.yaml.example in .couchfs directory
    let config_example = couchfs_dir.join("couchfs.yaml.example");
    std::fs::write(&config_example, include_str!("../../couchfs.yaml.example"))?;

    // Create .sync-ignore if it doesn't exist
    let sync_ignore = path.join(".sync-ignore");
    if !sync_ignore.exists() {
        let default_ignore = r#"# CouchFS ignore patterns
# Add files/directories to ignore (gitignore-style syntax)

# CouchFS internal files
.couchfs/
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
"#;
        std::fs::write(&sync_ignore, default_ignore)?;
    }

    // Create initial database
    let db_path = couchfs_dir.join("state.db");
    let local_db = LocalDb::open(&db_path)?;
    drop(local_db);

    println!("✓ Initialized CouchFS in {}", path.display());
    println!("  State database: {}", db_path.display());
    println!("  Config example: {}", config_example.display());
    println!("  Ignore file: {}", sync_ignore.display());
    println!();
    println!("Next steps:");
    println!("  1. Copy .couchfs/couchfs.yaml.example to .couchfs/couchfs.yaml");
    println!("  2. Edit .couchfs/couchfs.yaml with your CouchDB credentials");
    println!("  3. Run: couchfs sync {}", path.display());

    Ok(())
}

/// Run a one-time sync
pub async fn sync(path: PathBuf, config: AppConfig, dry_run: bool) -> Result<SyncReport> {
    info!("Running sync in: {}", path.display());

    // Load ignore patterns
    let _ignore_matcher = load_ignore_patterns(&path);

    // Open local database
    let db_path = path.join(".couchfs/state.db");
    let local_db = LocalDb::open(&db_path)?;

    // Connect to CouchDB
    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    if dry_run {
        println!("Dry run mode - no changes will be made");
        // TODO: Implement dry-run logic
        return Ok(SyncReport::default());
    }

    // Run sync
    let mut engine = SyncEngine::new(couchdb, local_db, path);
    let report = engine.sync().await?;

    print_sync_report(&report);

    Ok(report)
}

/// Run continuous sync daemon
pub async fn daemon(path: PathBuf, config: AppConfig, interval: u64) -> Result<()> {
    info!("Starting CouchFS daemon in: {}", path.display());
    println!("CouchFS daemon started (interval: {}s)", interval);
    println!("Press Ctrl+C to stop");

    // TODO: Implement file watcher + periodic sync
    // For now, just do periodic sync
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(interval));

    loop {
        interval.tick().await;

        if let Err(e) = sync(path.clone(), config.clone(), false).await {
            error!("Sync error: {}", e);
        }
    }
}

/// List conflicts
pub async fn conflicts(path: PathBuf, json: bool) -> Result<()> {
    let db_path = path.join(".couchfs/state.db");
    let local_db = LocalDb::open(&db_path)?;

    let conflicts = local_db.get_conflicts()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&conflicts)?);
    } else {
        if conflicts.is_empty() {
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
            println!("Resolve with:");
            println!("  couchfs resolve <path> --strategy keep-local");
            println!("  couchfs resolve <path> --strategy keep-remote");
            println!("  couchfs resolve <path> --strategy keep-both");
        }
    }

    Ok(())
}

/// Resolve a conflict
pub async fn resolve(
    path: PathBuf,
    conflict_path: PathBuf,
    strategy: ResolutionStrategy,
    config: AppConfig,
) -> Result<()> {
    info!(
        "Resolving conflict: {:?} with strategy: {}",
        conflict_path, strategy
    );

    let db_path = path.join(".couchfs/state.db");
    let local_db = LocalDb::open(&db_path)?;

    let couchdb = CouchDb::new(
        &config.couchdb.url,
        config.couchdb.username.as_deref(),
        config.couchdb.password.as_deref(),
        &config.couchdb.database,
        &config.couchdb.remote_path,
    )
    .await?;

    let mut engine = SyncEngine::new(couchdb, local_db, path);

    let conflict_path_str = conflict_path.to_string_lossy().to_string();
    engine
        .resolve_conflict(&conflict_path_str, strategy)
        .await?;

    println!(
        "✓ Resolved conflict: {} (strategy: {})",
        conflict_path.display(),
        strategy
    );

    Ok(())
}

/// Show sync status
pub async fn status(path: PathBuf, json: bool, _config: &AppConfig) -> Result<()> {
    let db_path = path.join(".couchfs/state.db");
    let local_db = LocalDb::open(&db_path)?;

    let file_states = local_db.get_all_file_states()?;
    let conflicts = local_db.get_conflicts()?;
    let checkpoint = local_db.get_checkpoint()?;

    // Count files by walking directory
    let file_count = walkdir::WalkDir::new(&path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count();

    if json {
        let status = serde_json::json!({
            "sync_directory": path,
            "tracked_files": file_states.len(),
            "local_files": file_count,
            "pending_conflicts": conflicts.len(),
            "last_sync": checkpoint.map(|(_, ts)| ts),
        });
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("CouchFS Status");
        println!("==============");
        println!("Sync directory: {}", path.display());
        println!("Local files:    {}", file_count);
        println!("Tracked files:  {}", file_states.len());
        println!("Pending conflicts: {}", conflicts.len());

        if let Some((seq, ts)) = checkpoint {
            println!("Last sync:      {}", ts.format("%Y-%m-%d %H:%M:%S UTC"));
            println!("Last sequence:  {}", seq);
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

/// Helper to print sync report
fn print_sync_report(report: &SyncReport) {
    println!();
    println!("Sync complete ✓");
    println!("  Uploaded:  {}", report.uploaded);
    println!("  Downloaded: {}", report.downloaded);
    println!("  Deleted (local): {}", report.deleted_local);
    println!("  Deleted (remote): {}", report.deleted_remote);

    if report.conflicts > 0 {
        println!("  Conflicts: {} ⚠️", report.conflicts);
        println!();
        println!("Run 'couchfs conflicts' to see details");
    }

    if !report.errors.is_empty() {
        println!();
        println!("Errors:");
        for error in &report.errors {
            println!("  • {}", error);
        }
    }
}
