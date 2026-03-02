# CouchDB File Sync

A Rust-based filesystem-to-CouchDB synchronization engine with bidirectional sync and conflict detection.

> [!IMPORTANT]
> This is very alpha software. Use at your own risk. It is in very early testing. You have been warned!

## Features

| Status | Feature | Description |
| --- | --- | --- |
| ✅ | **Bidirectional Sync** | Changes on either side propagate to the other |
| ✅ | **Conflict Detection** | Automatically detects when files change on both sides |
| ✅ | **Conflict Resolution** | CLI commands to resolve conflicts with multiple strategies |
| ✅ | **Smart Ignore Patterns** | Gitignore-style `.sync-ignore` file support |
| ✅ | **Telegram Notifications** | Get notified when conflicts are detected |
| ⚠️ | **Flexible Configuration** | CLI args, YAML config, and environment variables |
| ⚠️ | **Conflict File Preservation (optional)** | Keep-both resolution saves remote file as `filename.remote` |
| ❌ | **Live Mode** | Optional filesystem watcher + CouchDB changes feed for low-latency sync |
| ⚠️ | **Detailed Sync Logs** | `sync` and `daemon` write detailed logs to `/tmp/couchfs.logs` (override via `logging.file`) |

## Installation

### From Source

```bash
git clone https://gitea.hnrglobal.com/HNR-Global/couchdb-file-sync.git
cd couchdb-file-sync
cargo build --release
```

The binary will be at `target/release/couchdb-file-sync`.

## Quick Start

### 1. Initialize a Sync Directory

```bash
couchdb-file-sync init ~/my-documents
```

This creates:
- `.couchdb-file-sync/` directory for state storage
- `.sync-ignore` file for ignore patterns

### 2. Configure CouchDB Connection

```bash
cd ~/my-documents
cp /path/to/couchdb-file-sync.yaml.example .couchdb-file-sync/couchdb-file-sync.yaml
# Edit .couchdb-file-sync/couchdb-file-sync.yaml with your CouchDB credentials
```

Use the `couchdb-file-sync.yaml.example` in the repository as a starting point if you installed from source.

### 3. Run Initial Sync

```bash
couchdb-file-sync sync
```

### 4. Run Continuous Sync (Daemon Mode)

```bash
couchdb-file-sync daemon --interval 60
```

For low-latency sync using a filesystem watcher and CouchDB changes feed:

```bash
couchdb-file-sync sync
couchdb-file-sync daemon --live
```

## Configuration

Configuration is loaded in this precedence order (highest to lowest):
1. CLI arguments
2. Environment variables (`COUCHDB_FILE_SYNC_*`)
3. YAML config file (`couchdb-file-sync.yaml`)
4. Default values

### Environment Variables

```bash
export COUCHDB_FILE_SYNC_DB_URL="http://localhost:5984"
export COUCHDB_FILE_SYNC_DB_USERNAME="admin"
export COUCHDB_FILE_SYNC_DB_PASSWORD="secret"
export COUCHDB_FILE_SYNC_DB_NAME="couchdb_file_sync_files"
export COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL="60"
```

### YAML Config

```yaml
couchdb:
  url: "http://localhost:5984"
  username: "admin"
  password: "${COUCHDB_PASSWORD}"
  database: "couchdb_file_sync_files"

sync:
  poll_interval: 60
  debounce_ms: 500

notifications:
  enabled: true
  telegram:
    bot_token: "${TELEGRAM_BOT_TOKEN}"
    chat_id: "${TELEGRAM_CHAT_ID}"
  notify_on_conflict: true
  notify_on_sync_error: true

logging:
  level: "info"      # error, warn, info, debug, trace
  format: "pretty"   # pretty, json, compact
  file: "/tmp/couchfs.logs"  # Optional file output (used by sync/daemon)
```

## CLI Commands

### `couchdb-file-sync init [PATH]`

Initialize a new sync directory.

```bash
couchdb-file-sync init ~/documents
```

### `couchdb-file-sync sync [PATH]`

Run a one-time sync.

```bash
couchdb-file-sync sync ~/documents --dry-run
couchdb-file-sync sync ~/documents
```

### `couchdb-file-sync daemon [PATH]`

Run continuous sync daemon.

```bash
couchdb-file-sync daemon ~/documents --interval 60
```

For live mode (watcher + changes feed):

```bash
couchdb-file-sync sync
couchdb-file-sync daemon ~/documents --live
```

### `couchdb-file-sync conflicts [PATH]`

List pending conflicts.

```bash
couchdb-file-sync conflicts
couchdb-file-sync conflicts --json
```

### `couchdb-file-sync resolve <PATH> --strategy <STRATEGY>`

Resolve a conflict.

```bash
# Keep local version
couchdb-file-sync resolve docs/report.pdf --strategy keep-local

# Keep remote version
couchdb-file-sync resolve docs/report.pdf --strategy keep-remote

# Keep both (saves remote as .remote file)
couchdb-file-sync resolve docs/report.pdf --strategy keep-both
```

### `couchdb-file-sync status [PATH]`

Show sync status.

```bash
couchdb-file-sync status
couchdb-file-sync status --json
```

## Ignore Patterns

Create a `.sync-ignore` file in your sync root with gitignore-style patterns:

```gitignore
# Ignore all log files
*.log

# Ignore directories
build/
target/
node_modules/

# Ignore specific files
secret-config.toml

# Negation (un-ignore)
!important.log

# Match at any depth
**/cache/
```

## Conflict Resolution

When a file is modified on both sides, CouchDB File Sync:

1. Detects the conflict
2. Records the conflict in the database
3. Sends Telegram notification (if configured)

Resolve conflicts with:

```bash
couchdb-file-sync conflicts                    # List all conflicts
couchdb-file-sync resolve <path> --strategy <strategy>
```

### Resolution Strategies

- **keep-local**: Upload local version to CouchDB
- **keep-remote**: Download remote version, overwriting local
- **keep-both**: Save remote as `.remote`, keep local as-is

## Project Structure

```
couchdb-file-sync/
├── src/
│   ├── main.rs              # CLI entry point
│   ├── lib.rs               # Library exports
│   ├── config.rs            # Configuration handling
│   ├── couchdb/             # CouchDB client
│   ├── sync/                # Sync engine
│   ├── local/               # Local filesystem operations
│   ├── cli/                 # CLI commands
│   ├── models/              # Data models
│   └── telegram/            # Telegram notifications
├── Cargo.toml
├── couchdb-file-sync.yaml.example
└── README.md
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                   CouchDB File Sync Daemon                   │
├─────────────────────────────────────────────────────────────┤
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ File Watcher │  │ Sync Engine  │  │ CouchDB      │      │
│  │ (notify)     │  │              │  │ Client       │      │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
│         │                 │                  │              │
│  ┌──────▼─────────────────▼──────────────────▼──────┐       │
│  │              State Manager                        │       │
│  │         (SQLite local cache)                      │       │
│  └───────────────────────────────────────────────────┘       │
└─────────────────────────────────────────────────────────────┘
```

## Development

### Running Tests

```bash
cargo test
```

### Building

```bash
cargo build --release
```

## License

MIT

## Author

Nina Těšková <nina.marie.teskova@gmail.com>
