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
| ✅ | **Authoritative Rebuilds** | Rebuild the remote from local or rebuild the local tree from remote |
| ✅ | **Telegram Notifications** | Get notified when conflicts are detected |
| ⚠️ | **Flexible Configuration** | CLI args, YAML config, and environment variables |
| ⚠️ | **Conflict File Preservation (optional)** | Keep-both resolution saves remote file as `filename.remote` |
| ❌ | **Live Mode** | Optional filesystem watcher + CouchDB changes feed for low-latency sync |
| ⚠️ | **Detailed Sync Logs** | `sync` and `daemon` write detailed logs to `$XDG_STATE_HOME/couchdb-file-sync/couchdb-file-sync.log` by default (override via `logging.file`) |

## Installation

### From Source

```bash
git clone https://gitea.hnrglobal.com/HNR-Global/couchdb-file-sync.git
cd couchdb-file-sync
cargo build --release
```

The binary will be at `target/release/couchdb-file-sync`.

To install it for the current user and create a user-level systemd service:

```bash
./target/release/couchdb-file-sync install
```

This installs the binary to `~/.local/bin/couchdb-file-sync`, writes the config file to `~/.config/couchdb-file-sync/couchdb-file-sync.yaml` if it does not already exist, and enables `~/.config/systemd/user/couchdb-file-sync.service`.

## Development

Enable the repo's pre-commit hook to run formatting, clippy, and tests before each commit:

```bash
git config core.hooksPath .githooks
```

To measure scanner performance, run:

```bash
cargo run --bin scanner-perf --release
```

The helper creates a synthetic dataset, reports one cold scan, one initial warm scan, then repeats the warm scan `1000` times by default and prints min/median/avg/max timings. Override the dataset size or repeat count with `SCANNER_PERF_FILES`, `SCANNER_PERF_BYTES`, and `SCANNER_PERF_ITERATIONS`.

## Quick Start

### 1. Configure CouchDB Connection and Paths

Create `~/.config/couchdb-file-sync/couchdb-file-sync.yaml` (or use environment variables) and define your sync paths.

```bash
mkdir -p ~/.config/couchdb-file-sync
cp /path/to/couchdb-file-sync.yaml.example ~/.config/couchdb-file-sync/couchdb-file-sync.yaml
# Edit ~/.config/couchdb-file-sync/couchdb-file-sync.yaml with your CouchDB credentials and paths
```

Use the `couchdb-file-sync.yaml.example` in the repository as a starting point if you installed from source.

### 2. Initialize Sync Directories

```bash
couchdb-file-sync init
```

This initializes all `paths` defined in your config. You can also pass a single path:

```bash
couchdb-file-sync init ~/my-documents
```

If you pass a path that is not listed in your config, `init` will warn and you should add it to `paths` before syncing.

This creates:
- `.couchdb-file-sync/` directory for state storage
- `.sync-ignore` file for ignore patterns

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
3. YAML config file (`~/.config/couchdb-file-sync/couchdb-file-sync.yaml`)
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

paths:
  - local: "/absolute/or/relative/path"
    remote: "notes/"

sync:
  poll_interval: 60
  debounce_ms: 500

notifications:
  enabled: true
  telegram:
    bot_token: "${TELEGRAM_BOT_TOKEN}"
    chat_id: "${TELEGRAM_CHAT_ID}"
  slack:
    webhook_url: "${SLACK_WEBHOOK_URL}"
  notify_on_conflict: true
  notify_on_sync_error: true

logging:
  level: "info"      # error, warn, info, debug, trace
  format: "pretty"   # pretty, json, compact
  file: "~/.local/state/couchdb-file-sync/couchdb-file-sync.log"  # Optional file output (used by sync/daemon)
  rotated_logs: "delete"  # delete, keep (daemon rotates every 24 hours)
```

## CLI Commands

### `couchdb-file-sync init [PATH]`

Initialize a new sync directory. If `PATH` is omitted, all paths from the config are initialized.
If you pass a path not listed in the config, `init` will warn so you can add it to `paths`.

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

If the process panics, the binary attempts to send a Slack alert through `notifications.slack.webhook_url`. This uses the config already loaded from YAML or environment variables and is separate from the normal sync/conflict Telegram notifications.

For live mode (watcher + changes feed):

```bash
couchdb-file-sync sync
couchdb-file-sync daemon ~/documents --live
```

### `couchdb-file-sync rebuild-remote [PATH]`

Replace the remote scope with the current local filesystem for that path. Every non-ignored local file is uploaded, and remote files that no longer exist locally are deleted.

```bash
couchdb-file-sync rebuild-remote ~/documents
```

### `couchdb-file-sync rebuild-local [PATH]`

Replace the local filesystem with the current remote scope for that path. Existing non-ignored local files are removed first, then live remote files are downloaded.

```bash
couchdb-file-sync rebuild-local ~/documents
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

### `couchdb-file-sync install`

Install the current binary to the standard user location, create a user-level systemd service, and point it at `~/.config/couchdb-file-sync/couchdb-file-sync.yaml`.

```bash
couchdb-file-sync install
```

### `couchdb-file-sync uninstall`

Remove the user-level systemd service and the installed binary from `~/.local/bin`. The config file in `~/.config/couchdb-file-sync/couchdb-file-sync.yaml` is kept.

```bash
couchdb-file-sync uninstall
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
