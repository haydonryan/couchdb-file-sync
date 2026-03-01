# CouchFS

A Rust-based filesystem-to-CouchDB synchronization engine with bidirectional sync and conflict detection.

## Features

- **Bidirectional Sync**: Changes on either side propagate to the other
- **Conflict Detection**: Automatically detects when files change on both sides
- **Conflict Resolution**: CLI commands to resolve conflicts with multiple strategies
- **Smart Ignore Patterns**: Gitignore-style `.sync-ignore` file support
- **Telegram Notifications**: Get notified when conflicts are detected
- **Flexible Configuration**: CLI args, YAML config, and environment variables
- **Conflict File Preservation**: Remote file saved as `filename.remote` during conflicts
- **Live Mode**: Optional filesystem watcher + CouchDB changes feed for low-latency sync

## Installation

### From Source

```bash
git clone https://gitea.hnrglobal.com/HNR-Global/couchfs.git
cd couchfs
cargo build --release
```

The binary will be at `target/release/couchfs`.

## Quick Start

### 1. Initialize a Sync Directory

```bash
couchfs init ~/my-documents
```

This creates:
- `.couchfs/` directory for state storage
- `.sync-ignore` file for ignore patterns
- `couchfs.yaml.example` configuration template

### 2. Configure CouchDB Connection

```bash
cd ~/my-documents
cp couchfs.yaml.example couchfs.yaml
# Edit couchfs.yaml with your CouchDB credentials
```

### 3. Run Initial Sync

```bash
couchfs sync
```

### 4. Run Continuous Sync (Daemon Mode)

```bash
couchfs daemon --interval 60
```

For low-latency sync using a filesystem watcher and CouchDB changes feed:

```bash
couchfs sync
couchfs daemon --live
```

## Configuration

Configuration is loaded in this precedence order (highest to lowest):
1. CLI arguments
2. Environment variables (`COUCHFS_*`)
3. YAML config file (`couchfs.yaml`)
4. Default values

### Environment Variables

```bash
export COUCHFS_DB_URL="http://localhost:5984"
export COUCHFS_DB_USERNAME="admin"
export COUCHFS_DB_PASSWORD="secret"
export COUCHFS_DB_NAME="couchfs_files"
export COUCHFS_SYNC__POLL_INTERVAL="60"
```

### YAML Config

```yaml
couchdb:
  url: "http://localhost:5984"
  username: "admin"
  password: "${COUCHDB_PASSWORD}"
  database: "couchfs_files"

sync:
  poll_interval: 60
  debounce_ms: 500

notifications:
  enabled: true
  telegram:
    bot_token: "${TELEGRAM_BOT_TOKEN}"
    chat_id: "${TELEGRAM_CHAT_ID}"
  notify_on_conflict: true
```

## CLI Commands

### `couchfs init [PATH]`

Initialize a new sync directory.

```bash
couchfs init ~/documents
```

### `couchfs sync [PATH]`

Run a one-time sync.

```bash
couchfs sync ~/documents --dry-run
couchfs sync ~/documents
```

### `couchfs daemon [PATH]`

Run continuous sync daemon.

```bash
couchfs daemon ~/documents --interval 60
```

For live mode (watcher + changes feed):

```bash
couchfs sync
couchfs daemon ~/documents --live
```

### `couchfs conflicts [PATH]`

List pending conflicts.

```bash
couchfs conflicts
couchfs conflicts --json
```

### `couchfs resolve <PATH> --strategy <STRATEGY>`

Resolve a conflict.

```bash
# Keep local version
couchfs resolve docs/report.pdf --strategy keep-local

# Keep remote version
couchfs resolve docs/report.pdf --strategy keep-remote

# Keep both (saves remote as .remote file)
couchfs resolve docs/report.pdf --strategy keep-both
```

### `couchfs status [PATH]`

Show sync status.

```bash
couchfs status
couchfs status --json
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

When a file is modified on both sides, CouchFS:

1. Detects the conflict
2. Saves the remote version as `filename.remote`
3. Keeps the local version as-is
4. Sends Telegram notification (if configured)
5. Records the conflict in the database

Resolve conflicts with:

```bash
couchfs conflicts                    # List all conflicts
couchfs resolve <path> --strategy <strategy>
```

### Resolution Strategies

- **keep-local**: Upload local version to CouchDB
- **keep-remote**: Download remote version, overwriting local
- **keep-both**: Save remote as `.remote`, keep local as-is

## Project Structure

```
couchfs/
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
├── couchfs.yaml.example
└── README.md
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        CouchFS Daemon                        │
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
