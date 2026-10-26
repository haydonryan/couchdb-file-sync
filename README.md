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
| ✅ | **Conflict Notifications** | Telegram and/or Matrix; each opted-in channel is notified once per root and conflict generation |
| ⚠️ | **Flexible Configuration** | CLI args, YAML config, and environment variables |
| ⚠️ | **Conflict File Preservation (optional)** | Keep-both resolution saves a unique, local-only copy (`.filename.remote`, or under `conflicts.conflict_dir` when configured) |
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

### Arch Linux (AUR)

Two AUR packages are maintained under `packaging/aur/`:

- **`couchdb-file-sync`** — builds from source with `cargo build --release`.
- **`couchdb-file-sync-bin`** — installs the prebuilt release binary (faster).

Install either with an AUR helper:

```bash
# From source
yay -S couchdb-file-sync

# Or the prebuilt binary
yay -S couchdb-file-sync-bin
```

Both packages install `couchdb-file-sync` to `/usr/bin`.

### Nix

The flake exposes two packages, both installing the same `couchdb-file-sync`
executable:

- **`app-bin`** — the prebuilt release binary (fast path, no compilation). This
  is the default package.
- **`app-src`** — built from source via the committed `Cargo.lock` (no network
  access at build time).

```bash
# Run the prebuilt release binary (fast path, default)
nix run .#app-bin -- --version

# Run a build-from-source binary
nix run .#app-src -- --version

# Build either package into ./result/bin/couchdb-file-sync
nix build .#app-bin
nix build .#app-src
```

A focused `nix develop` shell (Rust toolchain only — every native dependency is
bundled) is provided as well.

## Development

Enable the repo's pre-commit hook to run formatting, clippy, and tests before each commit:

```bash
git config core.hooksPath .githooks
```

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

Environment variables use the `COUCHDB_FILE_SYNC` prefix with `__` separating
each nesting level (matching the YAML keys below):

```bash
export COUCHDB_FILE_SYNC__COUCHDB__URL="http://localhost:5984"
export COUCHDB_FILE_SYNC__COUCHDB__AUTH__USERNAME="admin"
export COUCHDB_FILE_SYNC__COUCHDB__AUTH__PASSWORD="secret"
export COUCHDB_FILE_SYNC__COUCHDB__DATABASE="couchdb_file_sync_files"
export COUCHDB_FILE_SYNC__SYNC__POLL_INTERVAL="60"
```

### Environment Variable Substitution

Any string value in the YAML config may reference an environment variable with
`${VAR}` syntax (for example `password: "${COUCHDB_PASSWORD}"`). References are
expanded from the environment at load time. A referenced variable that is not
set aborts configuration loading with an explicit error naming the variable —
it never silently reaches the runtime as a literal placeholder.

### YAML Config

```yaml
couchdb:
  url: "http://localhost:5984"
  auth:
    username: "admin"
    password: "${COUCHDB_PASSWORD}"
  database: "couchdb_file_sync_files"

paths:
  - local: "/absolute/or/relative/path"
    remote: "notes/"

sync:
  # Optional default sync root used when no CLI PATH and no `paths` entries
  # are given. Omitted here, so a bare command falls back to the current
  # directory.
  # root_dir: "/absolute/or/relative/path"
  poll_interval: 60
  debounce_ms: 500
  # Live mode: how often (seconds) a bounded reconciliation rescan retries
  # local changes that failed to upload or were dropped when the file watcher
  # overflowed. 0 disables the periodic rescan. Default: 30
  rescan_interval_secs: 30
  # Maximum number of queued changes applied per scheduling wave during a sync.
  # Uploads/downloads are processed batch_size at a time, bounding the queue of
  # work held in memory. Must be >= 1. Default: 100
  batch_size: 100
  # Whether per-file uploads/downloads may run in parallel during a sync.
  # When false, files are applied strictly one at a time (serially). Default: true
  parallel: true
  # Maximum number of per-file uploads/downloads in flight at once when
  # parallel is enabled. Must be >= 1 when parallel is true. Default: 4
  max_parallel: 4
  # Soft-delete tombstones older than this many seconds are pruned so
  # get_all_files/get_changes do not grow unbounded. 0 disables pruning.
  tombstone_retention_secs: 604800
  # Grace period (seconds) protecting in-flight chunk publication during
  # orphan collection. Each cycle reclaims unreferenced chunks left by failed
  # uploads and pruned tombstones, deferring any chunk younger than this window
  # in case a concurrent client is mid-publication. 0 disables collection.
  orphan_chunk_grace_secs: 300

notifications:
  telegram:
    credentials:
      bot_token: "${TELEGRAM_BOT_TOKEN}"
      chat_id: "${TELEGRAM_CHAT_ID}"
  notify_on_conflict: true
  notify_on_sync_error: true

logging:
  level: "info"      # error, warn, info, debug, trace
  format: "pretty"   # pretty, json, compact
  file: "~/.local/state/couchdb-file-sync/couchdb-file-sync.log"  # Optional file output (used by sync/daemon)
  rotation: daily-delete  # never, daily-keep, daily-delete
```

#### Remote path prefix

The `remote` value on each path (or the `couchdb.remote_path` for a single-path
setup) is the remote document prefix that scope syncs. An **empty value or `"/"`
both mean the whole database** (no prefix); any other value is normalized to a
trailing-slash, leading-slash-free form (`"notes"` and `"/notes/"` both become
`"notes/"`).

Local sync state (file states, conflicts, and the change-feed checkpoint) is
**bound to the validated sync mapping**: the canonical local root, the remote
database name, the remote database UUID, and the normalized prefix. If any of
those change (for example, you point the same directory at a different prefix,
a different database, or a recreated database), the old state is **not** reused.
The affected command errors and asks you to run `couchdb-file-sync
`rebuild-remote` or `rebuild-local` — the sanctioned explicit reset for the new
mapping. The daemon also refuses to start if two configured paths resolve to
the same canonical root.

#### Chunk encoding and LiveSync plain-text interop

File content is stored in `leaf` chunk documents. Every chunk written by this
tool carries an explicit `codec` field (`"base64"`) plus a base64-encoded
`data` string, so arbitrary (non-UTF-8) file bytes survive JSON transport
losslessly and the encoding is never ambiguous on read-back.

Official Obsidian LiveSync databases store text chunks differently: their leaf
`data` is a **raw plain-text string** with no `codec` field, and their metadata
documents use distinct `plain`/`new` types. Because an untagged `data` string
is inherently ambiguous — it could be a v0.5 base64 chunk written by an older
version of this tool, or a raw LiveSync text chunk — the operator picks the
interpretation with the `sync.legacy_chunk_data` config option:

- **`base64`** (default, *migration mode*): untagged `data` strings are treated
  as base64-encoded bytes. Use this when continuing to sync a database written
  by earlier versions of this tool (v0.5 untagged base64 records).
- **`plain_text`** (*LiveSync plain-text import mode*): untagged `data` strings
  are treated as raw text and are **not** base64-reinterpreted. Use this when
  importing/reading text chunks from an official LiveSync database.

New chunks written by this client always carry the explicit `codec`, so they
are read correctly in either mode. Unsupported or corrupt content — invalid
base64 in base64 mode, official LiveSync LZ-compressed chunks (identified by
their marker prefix), or an unknown `codec`/`type` value — fails **per chunk**
rather than silently corrupting the file bytes.

```yaml
sync:
  legacy_chunk_data: plain_text   # or base64 (default)
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

`--dry-run` walks the full sync pipeline (triage, upload/download/delete
detection, and conflict identification) without writing anything to CouchDB,
the local filesystem, or the state database, then prints a summary of what
would be uploaded, downloaded, deleted, and conflicted.

### `couchdb-file-sync daemon [PATH]`

Run continuous sync daemon.

```bash
couchdb-file-sync daemon ~/documents --interval 60
```

`--interval` (or `-i`) is optional. When omitted, the daemon polls at the
configured `sync.poll_interval` (default `60` seconds); an explicit
`--interval` always takes precedence. A zero interval is rejected before the
daemon starts, as are invalid poll intervals.

When `PATH` is omitted, the daemon (and the other commands) sync every
configured `paths` entry. If no `paths` are configured and no `PATH` is given,
the command syncs `sync.root_dir` when set, otherwise the current directory.

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

### `couchdb-file-sync resolve [PATH]`

Resolve pending conflicts **interactively**. For each conflict you choose a
strategy from a prompt; the prompt defaults to the configured
`conflicts.default_strategy` (or keep-local when unset).

```bash
couchdb-file-sync resolve
couchdb-file-sync resolve docs/report.pdf   # only resolve conflicts under this path
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

Exclusions can come from several sources. The effective policy is the union
of all configured sources, applied in precedence order (later sources override
earlier ones, so a `!` negation in a later source can re-include a path an
earlier source ignored):

1. **`ignore.patterns`** — inline gitignore-style patterns in the YAML config.
2. **`ignore.ignore_files`** — each listed file, read in order from the sync
   root.
3. **`.sync-ignore`** — always applied last (highest precedence), unless it is
   already listed in `ignore.ignore_files`.

When `ignore.ignore_files` is not configured, `.sync-ignore` alone is used, so
existing setups behave as before.

The policy fails closed: a malformed pattern in any source, or a configured
`ignore.ignore_files` entry that is missing or unreadable, aborts the sync
command with an error instead of silently degrading to "ignore nothing". The
default `.sync-ignore` file is optional (a missing one is fine), but any
explicitly configured ignore file must exist and be readable.

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

In live (daemon) mode, editing an ignore file reloads the policy and triggers a
reconciliation rescan, so rule changes take effect without restarting the
daemon. A reload that fails (e.g. a malformed edit) keeps the previous policy
and logs an error rather than exposing files.

## Conflict Resolution

When a file is modified on both sides — or one side is edited while the
other side deletes it — CouchDB File Sync:

1. Detects the conflict (including local-delete/remote-edit and
   remote-delete/local-edit races)
2. Records the conflict in the database
3. Sends Telegram notification (if configured)

Deletion ordering uses the monotonic CouchDB revision when available, so a
genuinely-new deletion is never discarded solely due to cross-host clock skew.

Resolve conflicts with:

```bash
couchdb-file-sync conflicts    # List all conflicts
couchdb-file-sync resolve      # Resolve conflicts interactively (per-conflict prompt)
```

### Resolution Strategies

- **keep-local**: Upload local version to CouchDB. If the local file was
  deleted, the deletion is propagated to the remote instead.
- **keep-remote**: Download remote version, overwriting local. If the remote
  was deleted (tombstone), the local file is deleted to match — it does not
  resurrect the tombstone's stale content.
- **keep-both**: Save the remote version as a unique, local-only copy while
  keeping the local file as-is. By default the copy is written dot-prefixed
  next to the original (`.filename.remote`, `.filename.remote.1`, …). If
  `conflicts.conflict_dir` is configured, copies instead go under
  `<sync-root>/<conflict_dir>/`, mirroring the original's subdirectory
  (`.conflicts/docs/report.pdf.remote`, …). The preservation directory is
  always excluded from sync, so copies never reenter a cycle accidentally; the
  copy never clobbers an existing backup and never marks itself as a synced
  download. A remote tombstone has no content to keep, so the local file is
  left untouched.

The configured `conflicts.default_strategy` (keep-local, keep-remote,
keep-both, or skip) preselects the interactive prompt's default. The
`conflicts.auto_resolve` option is **not supported**: the application does not
perform automatic conflict resolution, so a config that enables it is rejected
before any sync runs rather than silently ignoring it.

Resolution is snapshot-aware: destructive strategies never overwrite a version
that changed since the conflict was previewed. **keep-local** refuses (and
refreshes the conflict) if the remote advanced past the previewed revision, and
**keep-remote** refuses if the local file was edited after preview — so a newer,
unseen version is never silently clobbered. Re-run `couchdb-file-sync
conflicts` to re-review a refreshed conflict.

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

### Chunked content storage

File content is not stored as a single CouchDB document. `CouchDB` enforces a
per-document transport limit (the default `max_document_size` is 8 MiB) and
rejects any single document whose *encoded* JSON exceeds it with
`413 document_too_large` — a 9 MiB upload fails even though the file-size limit
is far larger. To stay within that bound:

- Content is split into ordered chunks of at most **2 MiB** of raw bytes each
  (`MAX_CHUNK_DATA_BYTES` in `src/couchdb/db.rs`).
- Each chunk's `data` is base64-encoded on the wire (4 bytes per 3 raw bytes,
  ~33% expansion), so a 2 MiB chunk encodes to roughly 2.8 MiB of JSON —
  comfortably inside the 8 MiB document limit with headroom for the surrounding
  `_id`/`type`/`created_at` fields.
- A file's `children` field holds its chunk IDs in order, so a file is
  reconstructed byte-for-byte by concatenating the chunks in `children` order.
- An empty file stores **no** chunks (an empty `children` list). If a chunk save
  fails partway through an upload, the error is propagated and no partial chunk
  set is recorded as the file's `children`; the orphan-written leading chunks
  are reclaimed by orphan chunk collection after `orphan_chunk_grace_secs`.

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
