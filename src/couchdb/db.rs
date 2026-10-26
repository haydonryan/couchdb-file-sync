use crate::models::{Change, ChunkDoc, DatabaseName, FileDoc, RemotePath, TimestampMillis};
use anyhow::Result;
use couch_rs::Client;
use couch_rs::database::Database;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;
use tracing::{debug, warn};

/// Base delay in milliseconds for retry backoff; doubles after each retry.
/// Only used in non-test builds; tests use a fixed tiny delay (`test_backoff`).
#[cfg(not(test))]
const RETRY_BASE_BACKOFF_MS: u64 = 250;

/// Characters allowed unencoded when percent-encoding a `CouchDB` document id.
///
/// A document id is opaque to the transport: even a filesystem-path id such as
/// `notes/foo.md` must be encoded as `notes%2Ffoo.md` so `CouchDB` treats it as
/// one literal document id rather than a path with separators. Encoding every
/// reserved character (including `/`, `.`, `%`, `#`, `?`, `&`, `+`, `=`) keeps
/// an untrusted id from escaping the selected database via `../` dot segments,
/// being truncated at `#`/`?`, or injecting query/fragment/URL parameters. Only
/// RFC 3986 unreserved characters (`A-Za-z0-9`, `-`, `_`, `~`) are left as-is;
/// the server percent-decodes the rest, so a legitimate id like `notes/foo.md`
/// still round-trips exactly to the same document.
const DOC_ID_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'~');

/// True if `id` is a chunk document id (generated as `h:<hex><hex>`).
///
/// A chunk id is exactly `h:` followed by a non-empty run of hex digits (the
/// timestamp and random suffix produced by `generate_chunk_id`). A tracked
/// file whose path merely *begins* with `h:` (e.g. `h:report.md`) is NOT a
/// chunk: hard-deleting it must surface as a file deletion, never be discarded
/// as chunk cleanup.
#[must_use]
fn is_chunk_id(id: &str) -> bool {
    let Some(hex_part) = id.strip_prefix("h:") else {
        return false;
    };
    !hex_part.is_empty() && hex_part.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True if `id` is a `CouchDB` system document (a design doc `_design/...` or
/// a local doc `_local/...`). These are server bookkeeping records, never user
/// files, so they must be excluded from the change stream before any
/// document-kind decision.
#[must_use]
fn is_system_doc_id(id: &str) -> bool {
    id.starts_with("_design/") || id.starts_with("_local/")
}

/// Result of one orphan-chunk collection pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkCollectionResult {
    /// Total chunk documents examined during the pass.
    pub scanned: usize,
    /// Unreachable chunks hard-deleted during the pass.
    pub collected: usize,
    /// Unreachable chunks left in place (still within the in-flight grace
    /// period, missing a revision, deferred to respect the per-pass bound, or
    /// whose delete failed).
    pub deferred: usize,
}

/// Transport headroom (ms) reserved between the long-poll server wait and the
/// request client timeout.
///
/// The reqwest client used for `_changes` requests carries a timeout of
/// exactly `timeout_seconds`. A long-poll that waits the full `timeout_seconds`
/// on the server side returns right at the client timeout, so the response
/// fails to decode (or times out) once transport latency is added. Budgeting
/// the long-poll wait below `timeout_seconds` by this margin leaves room for
/// the response to complete and decode before the client gives up.
const LONGPOLL_TRANSPORT_HEADROOM_MS: u64 = 1000;

/// Maximum number of raw content bytes stored in a single chunk document.
///
/// `CouchDB` enforces a per-document transport limit (the default
/// `max_document_size` is 8 MiB), rejecting any single document whose *encoded*
/// JSON exceeds it with `413 document_too_large` — a 9 MiB upload fails even
/// though the file-size limit is far larger. Chunk `data` is base64-encoded on
/// the wire (4 bytes per 3 raw bytes, ~33% expansion), so a raw bound of 2 MiB
/// encodes to roughly 2.8 MiB of JSON — comfortably inside the 8 MiB document
/// limit with headroom for the surrounding `_id`/`type`/`created_at` fields.
/// Files larger than this are split into multiple ordered chunks.
const MAX_CHUNK_DATA_BYTES: usize = 2 * 1024 * 1024;

/// Which `_changes` feed to request.
///
/// `Normal` is nonblocking: `CouchDB` returns immediately with the current
/// set of changes and `last_seq`. It is used for periodic sync so an idle
/// probe succeeds promptly instead of waiting out the full request timeout.
/// `LongPoll` holds the connection open until a change arrives or the
/// server-side `timeout` elapses; it is used by the live feed and is budgeted
/// to leave transport headroom below the request client timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangesFeed {
    Normal,
    LongPoll,
}

/// `CouchDB` client wrapper
pub struct CouchDb {
    client: Client,
    db: Database,
    http_client: HttpClient,
    base_db_url: String,
    db_name: DatabaseName,
    auth: Option<(String, String)>,
    /// Remote path prefix to sync (e.g., "notes/" or "obsidian/")
    remote_path: RemotePath,
    /// Request timeout in seconds applied to all `CouchDB` HTTP operations.
    timeout_seconds: u64,
    /// Maximum total attempts (including the initial one) for a `CouchDB`
    /// operation before giving up on transient connectivity failures.
    retry_attempts: u32,
    /// Whether the client was allowed to create the remote database on
    /// connect. When `false` (dry-run), the remote DB is never created; a
    /// missing database surfaces as an explicit error instead of a write.
    create_db: bool,
    /// How to interpret legacy chunk `data` strings that carry no explicit
    /// codec (see [`LegacyChunkData`]).
    legacy_chunk_data: crate::models::LegacyChunkData,
    /// Test-only canned responses used to exercise the sync pipeline without a
    /// live `CouchDB` server. Only present in test builds.
    #[cfg(test)]
    test_state: Option<CannedCouch>,
    /// Test-only counter of `CouchDB` write attempts (save/delete/chunk ops).
    /// Only present in test builds.
    #[cfg(test)]
    test_write_calls: std::sync::atomic::AtomicUsize,
    /// Test-only override for the retry backoff delay. Only present in test
    /// builds so retry unit tests run quickly without real sleeps.
    #[cfg(test)]
    test_backoff: Duration,
}

/// Test-only canned responses for the `CouchDB` client.
///
/// When `test_state` is set, read methods return this data instead of hitting
/// the network and write methods record a call instead of mutating anything.
#[cfg(test)]
#[derive(Default, Clone)]
pub struct CannedCouch {
    /// Remote changes returned by `get_changes`.
    pub changes: Vec<crate::models::Change>,
    /// Last sequence value returned by `get_changes`.
    pub last_seq: String,
    /// Metadata returned by `fetch_metadata`, keyed by remote path.
    pub metadata: std::collections::HashMap<String, crate::models::FileDoc>,
    /// All in-scope remote files returned by `get_all_files` (used by the
    /// authoritative rebuild paths). Filtered by the remote-path scope the same
    /// way the live `get_all_files` filters its listing.
    pub all_files: Vec<crate::models::FileDoc>,
    /// File content returned by `get_file_content`, keyed by remote path.
    pub contents: std::collections::HashMap<String, Vec<u8>>,
    /// Paths for which `get_file_content` should return a fetch error instead
    /// of content, simulating a failed network/auth/server content download.
    pub content_errors: std::collections::HashSet<String>,
    /// Remote paths (doc ids) for which `save_file` should return an error,
    /// simulating a failed network/auth/server content upload.
    pub save_errors: std::collections::HashSet<String>,
    /// Remote paths (doc ids) for which the FIRST `save_file` call returns an
    /// error, simulating a transient upload failure. The path is removed after
    /// the failure, so a subsequent retry (e.g. reconciliation) succeeds.
    /// Interior mutability lets the retry succeed against the same shared
    /// canned state.
    pub save_errors_once: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Remote paths (doc ids) for which the FIRST `delete_file` call returns an
    /// error, simulating a transient delete failure. The path is removed after
    /// the failure so a subsequent retry (reconciliation) succeeds.
    pub delete_errors_once: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// When set, each `save_file`/`get_file_content` call records itself in the
    /// probe and sleeps for `batch_delay`, letting tests observe how many
    /// per-file operations are in flight at once.
    pub probe: Option<std::sync::Arc<ConcurrencyProbe>>,
    /// Optional artificial per-operation delay to force overlap in
    /// bounded-concurrency tests. Only consulted when `probe` is set.
    pub batch_delay: Option<std::time::Duration>,
}

/// Test-only tracker of the maximum number of concurrent in-flight canned
/// `CouchDB` operations, used to assert that the sync apply loops respect the
/// bounded-concurrency limit.
#[cfg(test)]
#[derive(Default)]
pub struct ConcurrencyProbe {
    current: std::sync::atomic::AtomicUsize,
    max: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl ConcurrencyProbe {
    fn enter(&self) {
        let now = self
            .current
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
    }

    fn leave(&self) {
        self.current
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Highest number of concurrent operations observed.
    pub fn max_concurrent(&self) -> usize {
        self.max.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Entry from a `CouchDB` changes feed
#[derive(Debug, Clone)]
pub struct ChangeFeedEntry {
    pub change: Change,
    pub seq: String,
}

#[derive(Debug, Deserialize)]
struct ChangesResponse<T> {
    results: Vec<ChangeRow<T>>,
    last_seq: Value,
}

#[derive(Debug, Deserialize)]
struct ChangeRow<T> {
    id: String,
    seq: Value,
    deleted: Option<bool>,
    doc: Option<T>,
    /// `changes` carries the revision(s) of the update. For a hard-deleted row
    /// (no body) this is the only place the deletion's revision is available.
    #[serde(default)]
    changes: Vec<ChangeRev>,
}

/// A single `{ "rev": "…" }` entry from a `_changes` row's `changes` array.
#[derive(Debug, Deserialize)]
struct ChangeRev {
    rev: String,
}

fn seq_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

/// Classifies a transport/HTTP failure for the shared bounded retry policy.
///
/// Transient failures — connection errors, timeouts, and `CouchDB` 429/5xx
/// responses — are retried up to `retry_attempts`; permanent errors (auth,
/// schema, conflict, parse failures) fail immediately. Classification is driven
/// by structured status codes and typed errors rather than by parsing `Display`
/// strings, so a filename or error body that merely contains a status number
/// (e.g. `notes/404.md` next to a connection error) is never misread as that
/// HTTP status.
trait RetryClassify {
    fn is_transient(&self) -> bool;
}

impl RetryClassify for couch_rs::error::CouchError {
    fn is_transient(&self) -> bool {
        // `couch_rs` maps transport failures (timeouts, refused/reset
        // connections) onto a synthetic 501 with no real HTTP response, so any
        // 5xx (including 500 and 501) or 429 is retried.
        self.status().is_some_and(|status| {
            status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        })
    }
}

impl RetryClassify for reqwest::Error {
    fn is_transient(&self) -> bool {
        // Timeouts and connection-level failures are transport errors.
        if self.is_timeout() || self.is_connect() {
            return true;
        }
        self.status().is_some_and(|status| {
            status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        })
    }
}

impl RetryClassify for anyhow::Error {
    fn is_transient(&self) -> bool {
        // Walk the source chain looking for a classified error. `couch_rs` and
        // reqwest errors are stored (not stringified) inside `anyhow::Error`,
        // so this recovers the structured status/transport kind even after the
        // error has been converted into the crate-wide `anyhow` error type.
        let mut cursor: Option<&(dyn std::error::Error + 'static)> = Some(self.as_ref());
        while let Some(err) = cursor {
            if let Some(req) = err.downcast_ref::<reqwest::Error>()
                && req.is_transient()
            {
                return true;
            }
            if let Some(couch) = err.downcast_ref::<couch_rs::error::CouchError>()
                && couch.is_transient()
            {
                return true;
            }
            cursor = err.source();
        }
        false
    }
}

impl CouchDb {
    /// Create a new `CouchDB` client.
    ///
    /// `timeout_seconds` is applied as the request timeout for every `CouchDB`
    /// HTTP operation (both the direct reqwest client and the `couch_rs`
    /// client). `retry_attempts` caps the total number of attempts made for an
    /// operation when it fails with a transient connectivity error (see
    /// [`Self::retry_transient`]); the operation is attempted at most
    /// `retry_attempts` times in total, with exponential backoff between
    /// attempts.
    ///
    /// # Errors
    ///
    /// Returns an error if the `couch_rs` client cannot be created or the
    /// connection to `CouchDB` cannot be established.
    pub async fn new(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        db_name: &str,
        remote_path: &str,
        timeout_seconds: u64,
        retry_attempts: u32,
    ) -> Result<Self> {
        Self::connect(
            url,
            username,
            password,
            db_name,
            remote_path,
            timeout_seconds,
            retry_attempts,
            true,
        )
        .await
    }

    /// Create a read-only `CouchDB` client that never creates a missing remote
    /// database.
    ///
    /// The database handle is built structurally (no `PUT` to create a missing
    /// DB). A read against an absent database therefore 404s and is surfaced as
    /// an explicit "remote database does not exist" error — dry-run sync uses
    /// this so it walks the pipeline against an existing baseline and never
    /// writes.
    ///
    /// # Errors
    ///
    /// Returns an error if the `couch_rs` client or reqwest client cannot be
    /// created.
    pub async fn new_read_only(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        db_name: &str,
        remote_path: &str,
        timeout_seconds: u64,
        retry_attempts: u32,
    ) -> Result<Self> {
        Self::connect(
            url,
            username,
            password,
            db_name,
            remote_path,
            timeout_seconds,
            retry_attempts,
            false,
        )
        .await
    }

    /// Shared connection builder. When `create_db` is true the remote database
    /// is created on connect (via `couch_rs` `Client::db`); when false the
    /// handle is built structurally so a missing database is never created.
    #[allow(clippy::too_many_arguments)] // same connection settings as `new`/`new_read_only`
    async fn connect(
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        db_name: &str,
        remote_path: &str,
        timeout_seconds: u64,
        retry_attempts: u32,
        create_db: bool,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_seconds);
        let client = match (username, password) {
            (Some(u), Some(p)) => {
                Client::new_with_timeout(url, Some(u), Some(p), Some(timeout_seconds))?
            }
            _ => Client::new_with_timeout(url, None, None, Some(timeout_seconds))?,
        };

        let db = if create_db {
            // Get or create database (couch_rs `Client::db` creates a missing DB).
            client.db(db_name).await?
        } else {
            // Structural handle only: never create the remote database.
            couch_rs::database::Database::new(db_name.to_string(), client.clone())
        };

        let auth = match (username, password) {
            (Some(u), Some(p)) => Some((u.to_string(), p.to_string())),
            _ => None,
        };

        let base = url.trim_end_matches('/');
        let base_db_url = if base.ends_with(&format!("/{db_name}")) {
            base.to_string()
        } else {
            format!("{base}/{db_name}")
        };

        Ok(Self {
            client,
            db,
            http_client: HttpClient::builder().timeout(timeout).build()?,
            base_db_url,
            db_name: DatabaseName::new(db_name.to_string()),
            auth,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds,
            retry_attempts,
            create_db,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            #[cfg(test)]
            test_state: None,
            #[cfg(test)]
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_backoff: Duration::from_millis(1),
        })
    }

    /// Set how to interpret legacy chunk `data` strings that carry no explicit
    /// codec (see [`LegacyChunkData`]). Defaults to the v0.5 `base64` migration
    /// mode.
    #[must_use]
    pub const fn with_legacy_chunk_data(mut self, legacy: crate::models::LegacyChunkData) -> Self {
        self.legacy_chunk_data = legacy;
        self
    }

    async fn get_update_seq(&self) -> Result<String> {
        let info = self
            .retry_transient(|| self.client.get_info(&self.db_name))
            .await?;
        Ok(info.update_seq)
    }

    /// Fetch the remote database UUID.
    ///
    /// The UUID changes whenever the database is deleted and recreated, so it
    /// is part of the sync identity: reusing stored local state for a recreated
    /// database (with the same name and prefix) would silently reuse stale
    /// file states and checkpoints.
    ///
    /// # Errors
    ///
    /// Returns an error if the database info request fails.
    pub async fn get_db_uuid(&self) -> Result<String> {
        let body = self
            .retry_transient::<_, _, serde_json::Value, anyhow::Error>(|| {
                let mut request = self.http_client.get(&self.base_db_url);
                if let Some((username, password)) = &self.auth {
                    request = request.basic_auth(username, Some(password));
                }
                async move {
                    let response = request.send().await?;
                    // A read-only client must not silently proceed against a
                    // missing database: surface the absence explicitly instead
                    // of ever issuing a write to create it.
                    if !self.create_db && response.status() == reqwest::StatusCode::NOT_FOUND {
                        return Err(anyhow::anyhow!(
                            "remote database '{db}' does not exist (read-only/dry-run sync \
                             never creates databases), so there is nothing to sync against",
                            db = self.db_name,
                        ));
                    }
                    response
                        .error_for_status()?
                        .json::<serde_json::Value>()
                        .await
                        .map_err(Into::into)
                }
            })
            .await?;
        Ok(body
            .get("uuid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Create a `CouchDb` instance for testing without connecting to a real server.
    /// Only fields needed for path conversion and sync metadata are populated.
    ///
    /// # Panics
    ///
    /// Panics if the structural `couch_rs` client cannot be created, and panics
    /// if any method requiring actual `CouchDB` access is called.
    #[cfg(test)]
    #[must_use]
    pub fn for_test(remote_path: &str) -> Self {
        let remote_path = RemotePath::new(remote_path);
        // Create a client and database handle without connecting.
        // `Database::new` and `Client::new_no_auth` are purely structural;
        // actual HTTP requests will fail at runtime.
        let client = couch_rs::Client::new_no_auth("http://localhost:1")
            .expect("failed to create couch_rs client for test");
        let db = couch_rs::database::Database::new("unittest".to_string(), client.clone());
        Self {
            client,
            db,
            http_client: reqwest::Client::new(),
            base_db_url: "http://localhost:1/unittest".to_string(),
            db_name: DatabaseName::new("unittest"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            #[cfg(test)]
            test_state: None,
            #[cfg(test)]
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Create a `CouchDb` instance for testing backed by canned data.
    ///
    /// Read methods (`get_changes`, `fetch_metadata`, `get_file_content`)
    /// return the canned values, and write methods (`save_file`,
    /// `delete_file`, `upload_file_content`, `delete_chunks`) increment the
    /// write-call counter and do nothing, so tests can assert that a dry-run
    /// never issues any remote writes.
    #[cfg(test)]
    #[must_use]
    pub fn for_test_with_canned(remote_path: &str, canned: CannedCouch) -> Self {
        let mut client = Self::for_test(remote_path);
        client.test_state = Some(canned);
        client
    }

    /// Create a `CouchDb` instance for testing that points its structural
    /// `couch_rs` client and reqwest client at a caller-supplied base URL
    /// (typically an in-process fake `CouchDB` server), with the given
    /// remote-path scope and no canned state. Lets tests exercise the real HTTP
    /// path against a stateful fixture (e.g. verifying shared chunks survive an
    /// overwrite) without a live `CouchDB` server.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_test_at(addr: &str, remote_path: &str) -> Self {
        let client = couch_rs::Client::new_no_auth(addr).expect("fake couch_rs client");
        let db = couch_rs::database::Database::new("test_db".to_string(), client.clone());
        Self {
            client,
            db,
            http_client: reqwest::Client::new(),
            base_db_url: format!("{addr}/test_db"),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Number of write attempts issued against this test client.
    #[cfg(test)]
    #[must_use]
    pub fn test_write_calls(&self) -> usize {
        self.test_write_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Fetch changes from `CouchDB` using the `_changes` feed.
    ///
    /// `feed` selects a nonblocking `normal` feed or a blocking `longpoll`
    /// feed. For `LongPoll`, `timeout_ms` is the full wait budget and the
    /// server-side `timeout` is budgeted to leave
    /// [`LONGPOLL_TRANSPORT_HEADROOM_MS`] of headroom below the request client
    /// timeout so the response can complete and decode before the client gives
    /// up.
    ///
    /// # Errors
    ///
    /// Returns an error if the changes feed request fails or the update
    /// sequence cannot be fetched.
    pub async fn get_changes_feed(
        &self,
        since: &str,
        feed: ChangesFeed,
        timeout_ms: u64,
    ) -> Result<(Vec<ChangeFeedEntry>, String)> {
        let url = format!("{}/_changes", self.base_db_url);

        let feed_name = match feed {
            ChangesFeed::Normal => "normal",
            ChangesFeed::LongPoll => "longpoll",
        };

        // Server-side long-poll wait, budgeted to leave transport headroom
        // below the request client timeout (`timeout_seconds`). A tiny floor
        // keeps the long-poll a valid short wait even when the configured
        // timeout leaves little room for the margin.
        let server_timeout: Option<String> = match feed {
            ChangesFeed::Normal => None,
            ChangesFeed::LongPoll => Some(
                timeout_ms
                    .saturating_sub(LONGPOLL_TRANSPORT_HEADROOM_MS)
                    .max(1)
                    .to_string(),
            ),
        };

        // Decode the feed as raw JSON envelopes first so an unrelated or
        // unsupported document can never fail deserialization of the whole
        // response. Each in-scope row is then inspected by id and document
        // kind, and only validated file records are decoded into `FileDoc`.
        let body = self
            .retry_transient::<_, _, ChangesResponse<Value>, anyhow::Error>(|| {
                let mut request = self.http_client.get(&url).query(&[
                    ("since", since),
                    ("include_docs", "true"),
                    ("feed", feed_name),
                ]);
                if let Some(t) = &server_timeout {
                    request = request.query(&[("timeout", t.as_str())]);
                }

                if let Some((username, password)) = &self.auth {
                    request = request.basic_auth(username, Some(password));
                }

                async move {
                    let response = request.send().await?.error_for_status()?;
                    response
                        .json::<ChangesResponse<Value>>()
                        .await
                        .map_err(Into::into)
                }
            })
            .await?;

        let mut entries = Vec::new();
        for row in body.results {
            if let Some(entry) = self.change_from_row(&row)? {
                entries.push(entry);
            }
        }

        Ok((entries, seq_to_string(&body.last_seq)))
    }

    /// Classify a single raw `_changes` row into a [`ChangeFeedEntry`], or
    /// `Ok(None)` when the row is out of scope or is not a file record.
    ///
    /// Raw JSON envelopes are inspected here rather than deserializing the
    /// whole feed as `FileDoc`, so an unrelated or unsupported document can
    /// never fail the feed. Out-of-scope ids and non-file documents (chunk
    /// `leaf` docs, design/system docs, unknown `type` values) are dropped;
    /// only validated in-scope file records are decoded into `FileDoc`.
    fn change_from_row(&self, row: &ChangeRow<Value>) -> Result<Option<ChangeFeedEntry>> {
        // Out-of-scope ids are dropped before any document-kind decision so
        // unrelated remote records never surface or break the feed.
        if !self.is_path_allowed(&row.id) {
            return Ok(None);
        }

        if row.deleted.unwrap_or(false) {
            // Hard-deleted rows carry no body, so the document kind is
            // unavailable. Chunk documents (ids like `h:<hex><hex>`) are
            // hard-deleted whenever a file is overwritten; they must not
            // surface as file deletions. Only a `h:` id whose remainder is
            // pure hex is a chunk — a tracked file that merely begins with
            // `h:` is still a real file deletion.
            if is_chunk_id(&row.id) {
                return Ok(None);
            }
            return Ok(Some(ChangeFeedEntry {
                change: Change::remote_deleted(
                    row.id.clone(),
                    None,
                    row.changes.first().map(|c| c.rev.clone()),
                ),
                seq: seq_to_string(&row.seq),
            }));
        }

        let Some(doc) = row.doc.as_ref() else {
            return Ok(None);
        };

        // Design/local system documents are never user files.
        if is_system_doc_id(&row.id) {
            return Ok(None);
        }

        // Only `plain` documents — or documents with no `type`, which defaults
        // to `plain` — are file records. Chunk (`leaf`) documents and any
        // unknown/unsupported `type` value are not files: skip them rather
        // than letting one unrelated record fail the feed.
        let is_file_doc = match doc.get("type") {
            None => true,
            Some(Value::String(t)) => t == "plain",
            Some(_) => false,
        };
        if !is_file_doc {
            return Ok(None);
        }

        // In-scope, file-typed record: decode the applicable schema. A
        // malformed body here is an actionable error, never a silent drop —
        // returning Err leaves the checkpoint unadvanced so the malformed file
        // cannot be lost.
        let file_doc: FileDoc = serde_json::from_value(doc.clone()).map_err(|e| {
            anyhow::anyhow!(
                "Malformed in-scope file document `{}` in changes feed: {e}",
                row.id
            )
        })?;

        let entry = if file_doc.deleted {
            ChangeFeedEntry {
                change: Change::remote_deleted(
                    file_doc.id.clone(),
                    Some(file_doc.delete_time()),
                    file_doc.rev,
                ),
                seq: seq_to_string(&row.seq),
            }
        } else {
            let mtime = file_doc.modified_at();
            let rev = file_doc.rev.clone().unwrap_or_default();
            ChangeFeedEntry {
                change: Change::remote_modified(
                    file_doc.id,
                    String::new(),
                    file_doc.size,
                    mtime,
                    rev,
                ),
                seq: seq_to_string(&row.seq),
            }
        };
        Ok(Some(entry))
    }

    /// Check if a path is within the configured remote path
    /// Check if a path is within the configured remote path
    #[must_use]
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if self.remote_path.is_empty() {
            true
        } else {
            path.starts_with(self.remote_path.as_str())
                || path == self.remote_path.as_str().trim_end_matches('/')
        }
    }

    /// Get the normalized remote path prefix used for this sync scope.
    #[must_use]
    pub fn remote_prefix(&self) -> &str {
        &self.remote_path
    }

    /// Configured request timeout in seconds applied to `CouchDB` operations.
    #[must_use]
    pub const fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    /// Maximum total attempts (including the initial one) made for a `CouchDB`
    /// operation when it fails with a transient connectivity error.
    #[must_use]
    pub const fn retry_attempts(&self) -> u32 {
        self.retry_attempts
    }

    /// Run `op`, retrying transient connectivity failures with exponential
    /// backoff.
    ///
    /// The operation is attempted at most [`Self::retry_attempts`] times in
    /// total (the initial attempt plus retries; never fewer than one). Only
    /// failures classified as transient by [`RetryClassify`] are retried;
    /// persistent errors are returned immediately.
    async fn retry_transient<F, Fut, T, E>(&self, op: F) -> std::result::Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, E>>,
        E: RetryClassify + std::fmt::Display,
    {
        let mut attempts: u32 = 0;
        let total = self.retry_attempts.max(1);
        let mut op = op;
        loop {
            match op().await {
                Ok(value) => return Ok(value),
                Err(e) if e.is_transient() && attempts + 1 < total => {
                    attempts += 1;
                    let backoff = self.backoff(attempts);
                    warn!(
                        "CouchDB transient failure (attempt {}/{}): {}; retrying in {:?}",
                        attempts, total, e, backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Delay to wait before the next retry attempt (pathological: real sleeps
    /// in production, a tiny fixed delay in test builds to keep tests fast).
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    const fn backoff(&self, attempts: u32) -> Duration {
        #[cfg(test)]
        {
            let _ = attempts;
            self.test_backoff
        }
        #[cfg(not(test))]
        {
            let shift = if attempts >= 7 { 6 } else { attempts - 1 };
            Duration::from_millis(RETRY_BASE_BACKOFF_MS.saturating_mul(1u64 << shift))
        }
    }

    /// Get the full remote path for a local file
    #[must_use]
    pub fn get_remote_path(&self, local_path: &str) -> String {
        if self.remote_path.is_empty() {
            local_path.to_string()
        } else {
            // Combine remote path prefix with local path
            format!("{}{}", self.remote_path, local_path)
        }
    }

    /// Get the local path from a remote path (strips the remote prefix)
    #[must_use]
    pub fn get_local_path(&self, remote_path: &str) -> String {
        if self.remote_path.is_empty() {
            remote_path.to_string()
        } else {
            // Strip the remote path prefix
            remote_path
                .strip_prefix(self.remote_path.as_str())
                .unwrap_or(remote_path)
                .to_string()
        }
    }

    /// Get a document by ID
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails, or `Ok(None)` if the
    /// document does not exist.
    pub async fn get_file(&self, path: &str) -> Result<Option<FileDoc>> {
        // Check if path is within allowed remote path
        if !self.is_path_allowed(path) {
            return Ok(None);
        }

        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok(state.metadata.get(path).cloned());
        }

        match self.retry_transient(|| self.db.get(path)).await {
            Ok(doc) => Ok(Some(doc)),
            Err(e) => {
                // Only an actual HTTP 404 means absence. Structured status
                // detection (rather than substring matching on the error text)
                // means a filename or message that merely contains "404" or
                // "Not Found" is never mistaken for a missing document.
                if e.is_not_found() {
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Save a document
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails.
    pub async fn save_file(&self, doc: &mut FileDoc) -> Result<()> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            if state.save_errors.contains(&doc.id) {
                anyhow::bail!("simulated save failure for {}", doc.id);
            }
            // A one-shot transient failure: fail this call and clear the path
            // so the next call (reconciliation retry) succeeds.
            if let Ok(mut once) = state.save_errors_once.lock()
                && once.remove(&doc.id)
            {
                anyhow::bail!(
                    "simulated transient save failure for {doc_id}",
                    doc_id = doc.id
                );
            }
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(probe) = &state.probe {
                probe.enter();
                if let Some(delay) = state.batch_delay {
                    tokio::time::sleep(delay).await;
                }
                probe.leave();
            }
            return Ok(());
        }

        debug!("Saving file to CouchDB: {}", doc.id);

        // `doc` is borrowed mutably across the save, which cannot be returned
        // from a lazy closure, so apply the retry contract inline here: at most
        // `retry_attempts` total attempts, retrying only transient failures.
        let mut attempts: u32 = 0;
        let total = self.retry_attempts.max(1);
        loop {
            match self.db.save(doc).await {
                Ok(_details) => break,
                Err(e) if e.is_transient() && attempts + 1 < total => {
                    attempts += 1;
                    let backoff = self.backoff(attempts);
                    warn!(
                        "CouchDB transient failure while saving {} (attempt {}/{}): {}; retrying in {:?}",
                        doc.id, attempts, total, e, backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Delete a document
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails.
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            // A one-shot transient failure: fail this call and clear the path
            // so the next call (reconciliation retry) succeeds.
            if let Ok(mut once) = state.delete_errors_once.lock()
                && once.remove(path)
            {
                anyhow::bail!("simulated transient delete failure for {path}");
            }
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }

        if let Some(mut doc) = self.get_file(path).await? {
            doc.deleted = true;
            // Stamp the tombstone with an authoritative deletion timestamp so
            // age-based pruning and remote-delete arbitration use the *delete*
            // time rather than the deleted file's possibly-preserved stale
            // mtime. The mtime is stamped too for older/external clients that
            // only understand it.
            let now = crate::models::TimestampMillis::now();
            doc.deleted_at = now;
            doc.mtime = now;
            self.save_file(&mut doc).await?;
            debug!("Marked file as deleted in CouchDB: {}", path);
        }
        Ok(())
    }

    /// Get all documents (files only - not chunks, including deleted)
    /// Filtered by the configured remote path
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails.
    pub async fn get_all_files(&self) -> Result<Vec<FileDoc>> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok(state
                .all_files
                .iter()
                .filter(|&d| d.is_file() && self.is_path_allowed(&d.id))
                .cloned()
                .collect());
        }
        let collection = self
            .retry_transient(|| self.db.get_all::<FileDoc>())
            .await?;
        Ok(collection
            .rows
            .into_iter()
            .filter(|d| d.is_file() && self.is_path_allowed(&d.id))
            .collect())
    }

    /// Get changes since the last checkpoint.
    ///
    /// Returns remote files within the configured remote path. On the first
    /// run (no checkpoint) this bootstraps the existing in-scope file set so a
    /// fresh client materializes pre-existing remote files. On subsequent runs
    /// it resumes the since-based `_changes` feed so only changes after the
    /// checkpoint are returned. The returned sequence is the scope-appropriate
    /// resume point: the feed's `last_seq` for both incremental runs and the
    /// bootstrap run (whose gap-free snapshot replays the feed from a
    /// pre-listing cursor).
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` changes request or the update
    /// sequence fetch fails.
    pub async fn get_changes(&self, since: Option<&str>) -> Result<(Vec<Change>, String)> {
        debug!("get_changes called with since = {:?}", since);

        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok((state.changes.clone(), state.last_seq.clone()));
        }

        // First run (no checkpoint): bootstrap the existing in-scope file set so
        // a fresh client materializes pre-existing remote files, then checkpoint
        // the current DB sequence so later syncs are incremental.
        let Some(since) = since else {
            // Build a gap-free first-sync snapshot. Capture the DB's update_seq
            // as a *pre-snapshot* cursor BEFORE listing the existing files, then
            // replay the `_changes` feed from that cursor. Any document
            // created/updated/deleted after the snapshot is therefore either in
            // the listing (it predates the listing read) or arrives via the
            // replayed feed. Checkpointing the feed's `last_seq` (always ≥ the
            // snapshot cursor) can never advance past a change we failed to
            // materialize, so concurrent remote edits are not skipped.
            let snapshot_seq = self.get_update_seq().await?;
            let all_files = self.get_all_files().await?;
            debug!(
                "Bootstrap (no checkpoint): {} existing files in scope",
                all_files.len()
            );
            let listing_changes: Vec<Change> = all_files
                .into_iter()
                .map(|doc| {
                    let mtime = doc.modified_at();
                    let rev = doc.rev.clone();
                    if doc.deleted {
                        Change::remote_deleted(doc.id.clone(), Some(doc.delete_time()), rev)
                    } else {
                        Change::remote_modified(
                            doc.id,
                            String::new(),
                            doc.size,
                            mtime,
                            rev.unwrap_or_default(),
                        )
                    }
                })
                .collect();

            // Replay everything that changed after the pre-snapshot cursor so a
            // create/update/delete racing the listing is not checkpointed over.
            let (feed_entries, seq) = self
                .get_changes_feed(
                    &snapshot_seq,
                    ChangesFeed::Normal,
                    self.timeout_seconds.saturating_mul(1000),
                )
                .await?;
            let feed_changes: Vec<Change> =
                feed_entries.into_iter().map(|entry| entry.change).collect();

            // Merge the replayed feed over the listing snapshot. The feed is read
            // last, so its per-path state is the most recent — including deletions
            // that the earlier listing would not reflect — while stable files that
            // never changed after the snapshot appear only in the listing.
            let mut changes = feed_changes;
            let mut covered: HashSet<String> =
                changes.iter().map(|c| c.path().to_string()).collect();
            for change in listing_changes {
                if covered.insert(change.path().to_string()) {
                    changes.push(change);
                }
            }
            return Ok((changes, seq));
        };

        // Incremental: resume the since-based `_changes` feed so we only pull
        // changes that occurred after the checkpoint. A nonblocking `normal`
        // feed returns immediately with the current changes and `last_seq`, so
        // an idle periodic cycle succeeds promptly instead of waiting out the
        // full request timeout. The feed's `last_seq` is the scope-appropriate
        // resume point for the next cycle.
        let (entries, seq) = self
            .get_changes_feed(
                since,
                ChangesFeed::Normal,
                self.timeout_seconds.saturating_mul(1000),
            )
            .await?;
        let changes: Vec<Change> = entries.into_iter().map(|entry| entry.change).collect();
        debug!(
            "Returning {} changes since checkpoint {}",
            changes.len(),
            since
        );
        Ok((changes, seq))
    }

    /// Fetch remote file metadata (without downloading chunks)
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` request fails, or `Ok(None)` if the
    /// document does not exist.
    pub async fn fetch_metadata(&self, path: &str) -> Result<Option<FileDoc>> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            return Ok(state.metadata.get(path).cloned());
        }

        // Check if path is within allowed remote path
        if !self.is_path_allowed(path) {
            return Ok(None);
        }

        debug!("[FETCH METADATA] Fetching metadata for: {}", path);

        match self.retry_transient(|| self.db.get::<FileDoc>(path)).await {
            Ok(doc) => {
                debug!("[FETCH METADATA] Retrieved metadata:");
                debug!("  path: {}", doc.path);
                debug!("  size: {} bytes", doc.size);
                debug!("  mtime: {} ms", doc.mtime);
                debug!("  ctime: {} ms", doc.ctime);
                debug!("  rev: {:?}", doc.rev);
                debug!("  chunks: {}", doc.children.len());
                Ok(Some(doc))
            }
            Err(e) => {
                // Only an actual HTTP 404 means absence (structured status,
                // never a substring match on the error text).
                if e.is_not_found() {
                    debug!("[FETCH METADATA] Not found: {}", path);
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Test connection to `CouchDB`
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub async fn ping(&self) -> Result<bool> {
        // Get all files (limit 1) to test connection
        match self.retry_transient(|| self.db.get_all::<FileDoc>()).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Get a chunk document by ID
    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkDoc>> {
        // Encode the chunk id as a single literal URL path segment so it can
        // never escape the selected database via `../`, be truncated at `#`/`?`,
        // or inject query/fragment parameters.
        let encoded_id = utf8_percent_encode(chunk_id, DOC_ID_ENCODE_SET);
        let url = format!("{}/{encoded_id}", self.base_db_url);

        // Status validation lives *inside* the retryable operation so a
        // transient 429/5xx chunk fetch is retried; only a genuine 404 maps to
        // "no such chunk" (Ok(None)). Permanent statuses (auth, schema, …) are
        // returned as errors without retrying.
        self.retry_transient::<_, _, Option<ChunkDoc>, anyhow::Error>(|| {
            let mut request = self.http_client.get(&url);
            if let Some((username, password)) = &self.auth {
                request = request.basic_auth(username, Some(password));
            }
            async move {
                let response = request.send().await?;
                let status = response.status();
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    return Err(couch_rs::error::CouchError::new(
                        format!("Failed to fetch chunk {chunk_id}: {status} - {body}"),
                        status,
                    )
                    .into());
                }
                let chunk: ChunkDoc = response.json().await?;
                Ok(Some(chunk))
            }
        })
        .await
    }

    /// Get file content by fetching and combining all chunks.
    ///
    /// Fetches the file's metadata, then reassembles content from that
    /// snapshot. Callers that already hold a metadata snapshot should prefer
    /// [`Self::get_file_content_for_doc`] so the download is validated against
    /// the exact revision/children/size they selected, avoiding a mixed-revision
    /// download if metadata changes between two separate fetches.
    ///
    /// # Errors
    ///
    /// Returns an error if the file document or any chunk cannot be fetched, or
    /// if the content does not match the declared shape/length.
    pub async fn get_file_content(&self, path: &str) -> Result<Vec<u8>> {
        let Some(doc) = self.get_file(path).await? else {
            anyhow::bail!("File not found: {path}");
        };
        self.get_file_content_for_doc(&doc).await
    }

    /// Reassemble and validate file content from an already-selected metadata
    /// snapshot.
    ///
    /// This is the single consistent snapshot a download is validated against:
    /// it uses the caller's `FileDoc` (child list, declared size) rather than
    /// re-fetching metadata, so a concurrent metadata change cannot mix two
    /// revisions' chunk lists into one download.
    ///
    /// Validation performed before returning:
    /// - A nonempty file (declared `size > 0`) must carry at least one chunk;
    ///   an empty child list is only valid for a genuinely zero-length file.
    /// - Every listed chunk must exist and be a `Leaf` (chunk) document per the
    ///   format contract; a wrong-type record is rejected, not concatenated.
    /// - Duplicate chunk IDs are rejected (they would double-count bytes).
    /// - The combined byte length must equal the declared `size` exactly.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be fetched or the assembled content
    /// violates the declared shape/length.
    pub async fn get_file_content_for_doc(&self, doc: &FileDoc) -> Result<Vec<u8>> {
        #[cfg(test)]
        if let Some(state) = &self.test_state {
            if state.content_errors.contains(&doc.id) {
                anyhow::bail!("simulated content fetch failure for {}", doc.id);
            }
            if let Some(probe) = &state.probe {
                probe.enter();
                if let Some(delay) = state.batch_delay {
                    tokio::time::sleep(delay).await;
                }
                probe.leave();
            }
            return Ok(state.contents.get(&doc.id).cloned().unwrap_or_default());
        }

        if doc.children.is_empty() {
            // A declared-nonempty file with no chunks was never published (or
            // its content was lost). Returning empty bytes would record a
            // corrupt download as a valid zero-length file, so fail instead.
            if doc.size > 0 {
                anyhow::bail!(
                    "File {} declares size {} but has no chunks; refusing to return empty content",
                    doc.id,
                    doc.size
                );
            }
            debug!("File {} has no chunks, returning empty content", doc.id);
            return Ok(Vec::new());
        }

        // Fetch each chunk and combine the data.
        let mut content = Vec::with_capacity(usize::try_from(doc.size).unwrap_or(0));
        let mut seen = HashSet::with_capacity(doc.children.len());
        for chunk_id in &doc.children {
            // A duplicate chunk ID would double-count bytes and break the
            // length invariant; reject it rather than assembling a corrupt file.
            if !seen.insert(chunk_id) {
                anyhow::bail!(
                    "Duplicate chunk {chunk_id} listed for file {}; refusing to return corrupt content",
                    doc.id
                );
            }
            let Some(chunk) = self.get_chunk(chunk_id).await? else {
                // A missing chunk is a partial/corrupt download: fail it rather
                // than silently truncating the file and recording it as synced.
                anyhow::bail!(
                    "Chunk {chunk_id} not found for file {}; refusing to return partial content",
                    doc.id
                );
            };
            // The server must return the exact document we asked for. A
            // mismatched identity (e.g. a different doc reached through a
            // rewritten/encoded id) is not this file's content and must be
            // rejected rather than concatenated.
            if chunk.id != *chunk_id {
                anyhow::bail!(
                    "Chunk {chunk_id} for file {} returned unexpected identity {:?}; refusing to return content",
                    doc.id,
                    chunk.id
                );
            }
            // The format contract reserves `Leaf` for chunk records. A
            // wrong-type record (e.g. a nested file doc) is not content and must
            // be rejected rather than concatenated as bytes.
            if chunk.doc_type != crate::models::DocType::Leaf {
                anyhow::bail!(
                    "Chunk {chunk_id} for file {} has non-leaf type {:?}; refusing to return content",
                    doc.id,
                    chunk.doc_type
                );
            }
            content.extend_from_slice(&chunk.content_bytes(self.legacy_chunk_data)?);
        }

        // The combined bytes must match the declared size exactly. A mismatch
        // means the child list or a chunk changed underneath us (mixed
        // revision) or the metadata was inconsistent; fail rather than record
        // a corrupt download.
        let len = u64::try_from(content.len()).unwrap_or(u64::MAX);
        if len != doc.size {
            anyhow::bail!(
                "Content size mismatch for {}: expected {} bytes, downloaded {}; refusing partial/corrupt content",
                doc.id,
                doc.size,
                len
            );
        }

        Ok(content)
    }

    /// Generate a unique chunk ID
    fn generate_chunk_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Generate a base36-like ID similar to Obsidian LiveSync
        format!("h:{:x}{:x}", timestamp, rand::random::<u32>())
    }

    /// Save a chunk document to `CouchDB`
    async fn save_chunk(&self, chunk: &ChunkDoc) -> Result<()> {
        // Encode the chunk id as a single literal URL path segment (see
        // `get_chunk`) so a crafted id cannot escape the selected database or
        // inject query/fragment parameters.
        let encoded_id = utf8_percent_encode(&chunk.id, DOC_ID_ENCODE_SET);
        let url = format!("{}/{encoded_id}", self.base_db_url);

        // Status validation lives *inside* the retryable operation. A chunk
        // PUT is idempotent on its unique id, so a transient 429/5xx — or a
        // write that committed but whose acknowledgement was lost (a timeout or
        // dropped connection) — is safely retried without duplicating content:
        // re-sending the same chunk id/body just overwrites the same document.
        self.retry_transient::<_, _, (), anyhow::Error>(|| {
            let mut request = self.http_client.put(&url);
            if let Some((username, password)) = &self.auth {
                request = request.basic_auth(username, Some(password));
            }

            async move {
                let response = request
                    .header("Content-Type", "application/json")
                    .json(chunk)
                    .send()
                    .await?;
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    return Err(couch_rs::error::CouchError::new(
                        format!("Failed to save chunk {}: {status} - {body}", chunk.id),
                        status,
                    )
                    .into());
                }
                Ok(())
            }
        })
        .await?;

        debug!("Saved chunk: {}", chunk.id);
        Ok(())
    }

    /// Upload file content as ordered bounded chunks and return the chunk IDs.
    ///
    /// The content is split into chunks of at most [`MAX_CHUNK_DATA_BYTES`]
    /// raw bytes each, saved in order, and the returned chunk IDs are ordered
    /// so that `get_file_content` (which concatenates `children` in order)
    /// reconstructs the original bytes byte-for-byte. An empty file yields no
    /// chunks (an empty `children` list), which `get_file_content` maps to
    /// empty content.
    ///
    /// If a chunk save fails partway through (an interrupted upload), the error
    /// is propagated and no partial chunk set is returned: the caller only ever
    /// records a file's `children` on success, so an interrupted upload never
    /// leaves a file referencing missing chunks. Chunks written before the
    /// failure carry no referencing metadata document and are reclaimed by
    /// orphan chunk collection after the publication grace window.
    ///
    /// # Errors
    ///
    /// Returns an error if a chunk cannot be saved to `CouchDB`.
    pub async fn upload_file_content(&self, content: Vec<u8>) -> Result<Vec<String>> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(Vec::new());
        }

        // An empty file is represented by an empty `children` list rather than
        // an empty chunk, keeping `get_file_content`'s empty-children mapping
        // authoritative.
        if content.is_empty() {
            return Ok(Vec::new());
        }

        let mut chunk_ids = Vec::new();
        for chunk_data in content.chunks(MAX_CHUNK_DATA_BYTES) {
            let chunk_id = Self::generate_chunk_id();
            let chunk = ChunkDoc::new_encoded(chunk_id.clone(), chunk_data.to_vec());

            self.save_chunk(&chunk).await?;
            chunk_ids.push(chunk_id);
        }

        Ok(chunk_ids)
    }

    /// Delete old chunks that are no longer referenced
    ///
    /// # Errors
    ///
    /// Returns an error if a chunk cannot be deleted from `CouchDB`.
    pub async fn delete_chunks(&self, chunk_ids: &[String]) -> Result<()> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }

        for chunk_id in chunk_ids {
            let Some(chunk) = self.get_chunk(chunk_id).await? else {
                continue;
            };
            // Only hard-delete the exact leaf chunk we asked for. A mismatched
            // identity or a non-leaf record reached through a crafted id must
            // never be deleted.
            if chunk.id != *chunk_id || chunk.doc_type != crate::models::DocType::Leaf {
                warn!(
                    "Refusing to delete {chunk_id}: server returned {:?} of type {:?}",
                    chunk.id, chunk.doc_type
                );
                continue;
            }
            if let Some(rev) = chunk.rev {
                self.delete_doc(chunk_id, &rev).await?;
            }
        }
        Ok(())
    }

    /// Permanently delete a document by id and current revision.
    ///
    /// # Errors
    ///
    /// Returns an error if the `CouchDB` delete request fails.
    async fn delete_doc(&self, id: &str, rev: &str) -> Result<()> {
        // The id may be a filesystem path containing URL-reserved characters
        // (space, `#`, `?`, `%`, ...). Encode it as a single literal URL path
        // segment (see `DOC_ID_ENCODE_SET`) so the DELETE targets the correct
        // document instead of being truncated, mangled, or able to traverse
        // `../` out of the selected database.
        let encoded_id = utf8_percent_encode(id, DOC_ID_ENCODE_SET);
        let url = format!("{}/{encoded_id}", self.base_db_url);

        // `delete_doc` previously bypassed the shared retry policy entirely. It
        // now applies the same bounded retry with status validation inside the
        // retryable operation: transient 429/5xx and lost-acknowledgement
        // failures retry up to the configured limit; permanent errors (auth,
        // conflict, schema) fail immediately. A retried DELETE is idempotent on
        // the pinned revision.
        self.retry_transient::<_, _, (), anyhow::Error>(|| {
            let mut request = self.http_client.delete(&url);
            if let Some((username, password)) = &self.auth {
                request = request.basic_auth(username, Some(password));
            }
            // Send the revision as a structured query parameter so it is
            // percent-encoded and can never inject extra query parameters or a
            // fragment into the request.
            request = request.query(&[("rev", rev)]);
            async move {
                let response = request.send().await?;
                let status = response.status();
                if status.is_success() {
                    Ok(())
                } else {
                    let body = response.text().await.unwrap_or_default();
                    warn!("Failed to delete {}: {status} - {body}", id);
                    Err(couch_rs::error::CouchError::new(
                        format!("Failed to delete {id}: {status} - {body}"),
                        status,
                    )
                    .into())
                }
            }
        })
        .await?;
        debug!("Deleted document: {}", id);
        Ok(())
    }

    /// Permanently remove soft-delete tombstones that have outlived the
    /// retention window.
    ///
    /// A tombstone is a file document with `deleted: true` that exists so other
    /// clients observe and propagate the deletion. Keeping them forever makes
    /// `get_all_files`/`get_changes` grow unbounded, so once a tombstone has
    /// been around longer than `retention` (its authoritative delete time is
    /// older than the cutoff) it is considered obsolete and hard-deleted. This is
    /// best-effort cleanup: an individual tombstone that cannot be deleted is
    /// logged and skipped rather than aborting the sync.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial document fetch fails.
    pub async fn prune_tombstones(&self, retention: Duration) -> Result<usize> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(0);
        }

        let all_files = self.get_all_files().await?;
        let now_ms = crate::models::TimestampMillis::now().as_u64();
        let cutoff = crate::models::TimestampMillis::new(
            now_ms.saturating_sub(u64::try_from(retention.as_millis()).unwrap_or(u64::MAX)),
        );
        let candidates = obsolete_tombstones(&all_files, cutoff);

        let mut pruned = 0;
        for doc in candidates {
            let Some(rev) = &doc.rev else {
                warn!("Skipping tombstone {} without a revision", doc.id);
                continue;
            };
            match self.delete_doc(&doc.id, rev).await {
                Ok(()) => {
                    pruned += 1;
                    debug!("Pruned obsolete tombstone: {}", doc.id);
                }
                Err(e) => warn!("Failed to prune tombstone {}: {e}", doc.id),
            }
        }
        Ok(pruned)
    }

    /// Ensure a Mango index on the `type` field exists so chunk enumeration
    /// via `_find` is available. `Idempotent`: `CouchDB` returns success with
    /// `result: "exists"` when the index is already present.
    ///
    /// # Errors
    ///
    /// Returns an error if the `_index` request fails.
    async fn ensure_chunk_type_index(&self) -> Result<()> {
        let fields = couch_rs::types::index::IndexFields::new(vec![
            couch_rs::types::find::SortSpec::Simple("type".to_string()),
        ]);
        self.db
            .insert_index("chunk-type-index", fields, None, None)
            .await?;
        Ok(())
    }

    /// Collect orphan (unreachable) chunk documents, reclaiming content left
    /// behind by failed metadata publications and pruned tombstones.
    ///
    /// A chunk is an orphan when its id is not referenced by any file document
    /// (live file *or* not-yet-pruned tombstone). Two failure modes produce
    /// such orphans:
    ///
    /// - A failed publication: `upload_file_content` saves new chunks *before*
    ///   `save_file` publishes the metadata, so a failed/failed-to-commit save
    ///   leaves the freshly uploaded chunks with no referencing document.
    /// - Tombstone pruning: `prune_tombstones` hard-deletes obsolete delete
    ///   tombstones (metadata only), orphaning the chunks they referenced.
    ///
    /// Collection is **bounded**: at most `batch_size` orphans are hard-deleted
    /// per pass; any remainder is left in place for a later pass. It is
    /// **resumable**: unreferenced chunks persist until collected, so an
    /// interrupted pass continues on the next cycle. To protect a concurrent
    /// in-flight publication — a chunk just uploaded by another client whose
    /// metadata save has not landed yet — an unreferenced chunk younger than
    /// `grace` is deferred rather than collected. Every delete targets the
    /// specific revision observed during enumeration, so a chunk that changed
    /// (or became referenced) since is never removed.
    ///
    /// # Errors
    ///
    /// Returns an error if the index setup, file-document fetch, or chunk
    /// enumeration fails. Individual delete failures are logged and counted as
    /// deferred rather than aborting the pass.
    pub async fn collect_orphan_chunks(
        &self,
        batch_size: u64,
        grace: Duration,
    ) -> Result<ChunkCollectionResult> {
        #[cfg(test)]
        if self.test_state.is_some() {
            self.test_write_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(ChunkCollectionResult::default());
        }

        self.ensure_chunk_type_index().await?;

        let batch = usize::try_from(batch_size.max(1)).unwrap_or(1000);

        let now_ms = crate::models::TimestampMillis::now().as_u64();
        let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
        // Only collect chunks created strictly before the grace window opens.
        // created_at == 0 (older/externally-created chunks) is treated as old:
        // its age is unknown but it is unreferenced, and deferring it forever
        // would never reclaim pre-existing orphans.
        let cutoff = now_ms.saturating_sub(grace_ms);

        // Referenced chunk ids from every file document (live files and
        // not-yet-pruned tombstones). Chunks referenced by a surviving
        // tombstone are "recoverable" and must survive.
        let all_files = self.get_all_files().await?;
        let referenced: HashSet<&str> = all_files
            .iter()
            .flat_map(|d| d.children.iter().map(String::as_str))
            .collect();

        let mut result = ChunkCollectionResult::default();
        let mut bookmark: Option<String> = None;
        loop {
            let mut query =
                couch_rs::types::find::FindQuery::new(serde_json::json!({ "type": "leaf" }))
                    .fields(vec![
                        "_id".to_string(),
                        "_rev".to_string(),
                        "created_at".to_string(),
                    ])
                    .limit(u64::try_from(batch).unwrap_or(1000))
                    .stable(true);
            if let Some(b) = &bookmark {
                query = query.bookmark(b);
            }
            let collection = self.db.find::<ChunkDoc>(&query).await?;

            for chunk in &collection.rows {
                result.scanned += 1;
                if referenced.contains(chunk.id.as_str()) {
                    continue;
                }
                let created = chunk.created_at.as_u64();
                if created != 0 && created > cutoff {
                    // Still inside the in-flight publication grace window.
                    result.deferred += 1;
                    continue;
                }
                if result.collected >= batch {
                    // Per-pass bound reached; leave the rest for a later pass.
                    result.deferred += 1;
                    continue;
                }
                let Some(rev) = chunk.rev.as_deref() else {
                    warn!("Skipping orphan chunk {} without a revision", chunk.id);
                    result.deferred += 1;
                    continue;
                };
                match self.delete_doc(&chunk.id, rev).await {
                    Ok(()) => result.collected += 1,
                    Err(e) => {
                        warn!("Failed to collect orphan chunk {}: {e}", chunk.id);
                        result.deferred += 1;
                    }
                }
            }

            match collection.bookmark {
                Some(next) if next != bookmark.as_deref().unwrap_or("") => bookmark = Some(next),
                _ => break,
            }
        }

        Ok(result)
    }
}

/// Select the soft-delete tombstones that are old enough to be pruned.
///
/// A tombstone is obsolete when it is a file doc flagged `deleted` whose
/// deletion-stamped mtime predates the cutoff. Chunks and live files are never
/// candidates.
#[must_use]
fn obsolete_tombstones(docs: &[FileDoc], cutoff: TimestampMillis) -> Vec<&FileDoc> {
    docs.iter()
        .filter(|d| {
            d.is_file()
                && d.deleted
                && u64::try_from(d.delete_time().timestamp_millis()).unwrap_or(u64::MAX)
                    < cutoff.as_u64()
        })
        .collect()
}

/// Helper to create `CouchDB` URL from components
#[must_use]
pub fn build_couch_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal error type implementing [`RetryClassify`] so the retry-loop
    /// mechanics tests can exercise transient vs. permanent classification
    /// without a real transport error.
    #[derive(Debug, PartialEq)]
    enum TestRetryError {
        Transient,
        Permanent,
    }

    impl std::fmt::Display for TestRetryError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{self:?}")
        }
    }

    impl RetryClassify for TestRetryError {
        fn is_transient(&self) -> bool {
            matches!(self, Self::Transient)
        }
    }

    /// Helper to create a `CouchDb` instance for testing without a real `CouchDB`.
    /// The methods under test (`is_path_allowed`, `get_remote_path`, `get_local_path`)
    /// only depend on `self.remote_path`, so we use dummy `couch_rs` values.
    fn test_couchdb(remote_path: &str) -> CouchDb {
        let client = Client::new_no_auth("http://localhost:15984").unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: "http://localhost:15984/test_db".to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    // -----------------------------------------------------------------------
    // Task 10051: is_path_allowed
    // -----------------------------------------------------------------------

    #[test]
    fn is_path_allowed_empty_prefix_permits_all() {
        let db = test_couchdb("");
        assert!(db.is_path_allowed("any/path/file.md"));
        assert!(db.is_path_allowed("deeply/nested/path/document.txt"));
    }

    #[test]
    fn is_path_allowed_trailing_slash_prefix() {
        let db = test_couchdb("notes/");
        assert!(db.is_path_allowed("notes/file.md"));
        assert!(db.is_path_allowed("notes/subdir/doc.txt"));
        assert!(!db.is_path_allowed("other/file.md"));
        assert!(!db.is_path_allowed("journal/entry.md"));
    }

    #[test]
    fn is_path_allows_exact_prefix_match_without_trailing_slash() {
        // A path equal to the prefix (minus trailing slash) should also be allowed
        let db = test_couchdb("notes/");
        assert!(db.is_path_allowed("notes"));
    }

    #[test]
    fn is_path_allowed_rejects_outside_prefix() {
        let db = test_couchdb("notes/");
        assert!(!db.is_path_allowed("Notes/file.md"));
        assert!(!db.is_path_allowed("note"));
        assert!(!db.is_path_allowed("notes_extra/file.md"));
    }

    #[test]
    fn is_path_allowed_root_prefix_normalized_to_empty() {
        let db = test_couchdb("");
        assert!(db.is_path_allowed("any/path.md"));
    }

    #[test]
    fn is_path_allowed_nested_paths() {
        let db = test_couchdb("obsidian/");
        assert!(db.is_path_allowed("obsidian/vault/notes/file.md"));
        assert!(db.is_path_allowed("obsidian/vault/deep/nested/doc.txt"));
    }

    // -----------------------------------------------------------------------
    // Task 10052: get_remote_path and get_local_path round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn get_remote_path_without_prefix_passthrough() {
        let db = test_couchdb("");
        assert_eq!(db.get_remote_path("file.md"), "file.md");
        assert_eq!(db.get_remote_path("subdir/doc.txt"), "subdir/doc.txt");
    }

    #[test]
    fn get_remote_path_with_prefix() {
        let db = test_couchdb("notes/");
        assert_eq!(db.get_remote_path("file.md"), "notes/file.md");
        assert_eq!(db.get_remote_path("subdir/doc.txt"), "notes/subdir/doc.txt");
    }

    #[test]
    fn get_local_path_without_prefix_passthrough() {
        let db = test_couchdb("");
        assert_eq!(db.get_local_path("file.md"), "file.md");
        assert_eq!(db.get_local_path("subdir/doc.txt"), "subdir/doc.txt");
    }

    #[test]
    fn get_local_path_strips_prefix() {
        let db = test_couchdb("notes/");
        assert_eq!(db.get_local_path("notes/file.md"), "file.md");
        assert_eq!(db.get_local_path("notes/subdir/doc.txt"), "subdir/doc.txt");
    }

    #[test]
    fn get_local_path_unknown_prefix_returns_whole() {
        let db = test_couchdb("notes/");
        // When the remote path doesn't start with the prefix, return as-is
        assert_eq!(db.get_local_path("other/file.md"), "other/file.md");
    }

    #[test]
    fn remote_local_path_round_trip_without_prefix() {
        let db = test_couchdb("");
        let local = "some/file.md";
        let remote = db.get_remote_path(local);
        assert_eq!(db.get_local_path(&remote), local);
    }

    #[test]
    fn remote_local_path_round_trip_with_prefix() {
        let db = test_couchdb("obsidian/");
        let local = "vault/note.md";
        let remote = db.get_remote_path(local);
        assert_eq!(remote, "obsidian/vault/note.md");
        assert_eq!(db.get_local_path(&remote), local);
    }

    // -----------------------------------------------------------------------
    // Task 10053: build_couch_url, seq_to_string, 404 handling in get_file
    // -----------------------------------------------------------------------

    #[test]
    fn build_couch_url_constructs_http_url() {
        assert_eq!(build_couch_url("localhost", 5984), "http://localhost:5984");
    }

    #[test]
    fn build_couch_url_with_ip_and_non_default_port() {
        assert_eq!(build_couch_url("127.0.0.1", 443), "http://127.0.0.1:443");
    }

    #[test]
    fn seq_to_string_from_string() {
        let value = Value::String("12345-abc".to_string());
        assert_eq!(seq_to_string(&value), "12345-abc");
    }

    #[test]
    fn seq_to_string_from_number() {
        let value = Value::Number(serde_json::Number::from(67890));
        assert_eq!(seq_to_string(&value), "67890");
    }

    #[test]
    fn seq_to_string_from_null() {
        let value = Value::Null;
        assert_eq!(seq_to_string(&value), "null");
    }

    #[test]
    fn seq_to_string_from_array() {
        let value = Value::Array(vec![
            Value::String("seq1".to_string()),
            Value::String("seq2".to_string()),
        ]);
        // For non-string values, serde_json::to_string is used
        assert_eq!(seq_to_string(&value), "[\"seq1\",\"seq2\"]");
    }

    /// Test that `get_file` returns Ok(None) for paths outside the allowed prefix
    #[tokio::test]
    async fn get_file_returns_none_for_disallowed_path() {
        let db = test_couchdb("notes/");
        let result = db.get_file("other/outside.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    /// Test that `fetch_metadata` returns Ok(None) for paths outside the allowed prefix
    #[tokio::test]
    async fn fetch_metadata_returns_none_for_disallowed_path() {
        let db = test_couchdb("notes/");
        let result = db.fetch_metadata("other/outside.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_file_disallowed_path_returns_ok_none() {
        let db = test_couchdb("data/");
        let result = db.get_file("config/secrets.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn fetch_metadata_disallowed_path_returns_ok_none() {
        let db = test_couchdb("data/");
        let result = db.fetch_metadata("config/secrets.md").await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // Task 10831/10832/10833: timeout_seconds and retry_attempts wiring
    // -----------------------------------------------------------------------

    /// Build a test client with explicit timeout/retry settings.
    fn test_couchdb_with_settings(timeout_seconds: u64, retry_attempts: u32) -> CouchDb {
        let mut db = test_couchdb("");
        db.timeout_seconds = timeout_seconds;
        db.retry_attempts = retry_attempts;
        db
    }

    #[test]
    fn configured_timeout_and_retry_are_threaded_into_client() {
        let db = test_couchdb_with_settings(42, 5);
        assert_eq!(db.timeout_seconds(), 42);
        assert_eq!(db.retry_attempts(), 5);
    }

    #[tokio::test]
    async fn retry_transient_injects_n_failures_and_retries_up_to_retry_attempts() {
        // Verify behavior: N = retry_attempts - 1 transient failures are
        // tolerated, and the operation succeeds on the last allowed attempt.
        let db = test_couchdb_with_settings(30, 3);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, u32, TestRetryError>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(TestRetryError::Transient)
                    } else {
                        Ok(42u32)
                    }
                }
            })
            .await;

        assert_eq!(result, Ok(42));
        // 2 transient failures + 1 success == retry_attempts total attempts.
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "client should retry up to retry_attempts total attempts"
        );
    }

    #[tokio::test]
    async fn retry_transient_gives_up_after_retry_attempts_transient_failures() {
        let db = test_couchdb_with_settings(30, 2);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, (), TestRetryError>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = n;
                    Err(TestRetryError::Transient)
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "client must give up after retry_attempts total attempts"
        );
    }

    #[tokio::test]
    async fn retry_transient_does_not_retry_non_transient_errors() {
        let db = test_couchdb_with_settings(30, 5);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, (), TestRetryError>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = n;
                    Err(TestRetryError::Permanent)
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "non-transient errors must not be retried"
        );
    }

    #[tokio::test]
    async fn retry_transient_always_performs_at_least_one_attempt() {
        let db = test_couchdb_with_settings(30, 0);
        let calls = std::sync::atomic::AtomicUsize::new(0);

        let result = db
            .retry_transient::<_, _, (), TestRetryError>(|| {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let _ = n;
                    Err(TestRetryError::Transient)
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "zero retry_attempts is clamped to a single attempt"
        );
    }

    #[test]
    fn couch_error_classifies_by_structured_status() {
        // 5xx (including the previously-missed 500), 501 (couch_rs's synthetic
        // transport mapping), and 429 are transient.
        for status in [
            reqwest::StatusCode::INTERNAL_SERVER_ERROR, // 500
            reqwest::StatusCode::BAD_GATEWAY,           // 502
            reqwest::StatusCode::SERVICE_UNAVAILABLE,   // 503
            reqwest::StatusCode::GATEWAY_TIMEOUT,       // 504
            reqwest::StatusCode::NOT_IMPLEMENTED,       // 501
            reqwest::StatusCode::TOO_MANY_REQUESTS,     // 429
        ] {
            assert!(
                couch_rs::error::CouchError::new("boom".to_string(), status).is_transient(),
                "{status} should be transient"
            );
        }
        // Permanent auth/schema/conflict errors fail immediately (no retry).
        for status in [
            reqwest::StatusCode::BAD_REQUEST,  // 400
            reqwest::StatusCode::UNAUTHORIZED, // 401
            reqwest::StatusCode::FORBIDDEN,    // 403
            reqwest::StatusCode::NOT_FOUND,    // 404
            reqwest::StatusCode::CONFLICT,     // 409
        ] {
            assert!(
                !couch_rs::error::CouchError::new("boom".to_string(), status).is_transient(),
                "{status} should NOT be transient"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Task 2895: surface delete_chunks HTTP failures
    // -----------------------------------------------------------------------

    /// Build a `CouchDb` instance pointed at a caller-supplied base URL, with no
    /// canned state, so `delete_chunks` issues real HTTP requests to the fake
    /// server in these tests.
    fn couchdb_at(base_db_url: &str) -> CouchDb {
        let client = Client::new_no_auth("http://localhost:15984").unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: base_db_url.to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(String::new()),
            timeout_seconds: 30,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Spawn an in-process fake `CouchDB` server backed by a real hyper HTTP
    /// server that serves one chunk document on GET and answers DELETE with
    /// `delete_status`. Using hyper means HTTP/1.1 framing, keep-alive and
    /// connection reuse are handled correctly by construction, so the tests are
    /// deterministic and do not race on macOS the way a hand-rolled raw-TCP
    /// server did. Returns the base database URL, a counter of accepted TCP
    /// connections, the server thread handle, and a shutdown flag.
    fn spawn_fake_couch(
        delete_status: u16,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        // Bind a std listener first so we obtain the ephemeral port without
        // needing an async context, then hand the socket to a dedicated tokio
        // runtime that runs the hyper accept loop.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake couch");
        let addr = listener.local_addr().expect("fake couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let conn_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_thread = shutdown.clone();
        let conn_count_thread = conn_count.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake couch tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    // Poll for connections with a short timeout so the loop can
                    // observe the shutdown flag and exit promptly.
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            conn_count_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let status = delete_status;
                            let service = hyper::service::service_fn(move |req| {
                                handle_fake_couch_req(req, status)
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {} // no connection yet; re-check shutdown
                    }
                }
            });
        });
        (base_db_url, conn_count, handle, shutdown)
    }

    /// Serve one HTTP request: respond to GET with a chunk document and to
    /// DELETE with the configured status. hyper handles keep-alive and
    /// connection reuse, so the server tolerates reqwest reusing the pooled
    /// connection across the GET and DELETE that `delete_chunks` issues.
    // `async` is required: hyper's `service_fn` must return a future resolving
    // to `Result`, so the (test-only) fake server keeps this signature even
    // though the body contains no awaits.
    #[allow(clippy::unused_async)]
    async fn handle_fake_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        delete_status: u16,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        match parts.method {
            hyper::Method::GET => {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                    br#"{"_id":"chunk1","_rev":"1-abc","data":"aGVsbG8=","type":"leaf"}"#,
                ));
            }
            hyper::Method::DELETE if delete_status == 200 => {
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from_static(br#"{"ok":true}"#));
            }
            hyper::Method::DELETE => {
                *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                    br#"{"error":"simulated delete failure"}"#,
                ));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn delete_chunks_surfaces_http_delete_failure() {
        let (base_db_url, _conn_count, server, shutdown) = spawn_fake_couch(500);
        let db = couchdb_at(&base_db_url);
        let chunk_ids = vec!["chunk1".to_string()];
        let result = db.delete_chunks(&chunk_ids).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");
        assert!(
            result.is_err(),
            "failed chunk delete must be surfaced, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_chunks_successful_delete_returns_ok() {
        let (base_db_url, _conn_count, server, shutdown) = spawn_fake_couch(200);
        let db = couchdb_at(&base_db_url);
        let chunk_ids = vec!["chunk1".to_string()];
        let result = db.delete_chunks(&chunk_ids).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");
        assert!(
            result.is_ok(),
            "successful chunk delete must return Ok, got: {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Task 6372: document/child id traversal + reserved-character safety
    // -----------------------------------------------------------------------

    /// Normalize a raw request path the way a reverse proxy resolves dot
    /// segments: collapse `.` and pop the last segment for `..`. `%2F` is NOT
    /// decoded before this, matching how nginx/CouchDB route on the raw path —
    /// so `..%2Fvictim` is a single literal segment and never resolves to the
    /// parent database.
    fn normalize_raw_path(path: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for seg in path.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                s => out.push(s),
            }
        }
        out
    }

    /// Percent-decode the final path segment to recover the real document id,
    /// mimicking how `CouchDB` decodes `%2F`, `%3A`, `%2E` etc. back into the
    /// doc id when it looks a document up.
    fn decoded_last_segment(path: &str) -> String {
        let seg = path.split('/').next_back().unwrap_or("");
        percent_encoding::percent_decode_str(seg)
            .decode_utf8_lossy()
            .into_owned()
    }

    /// Spawn an in-process fake `CouchDB` server exposing two logical
    /// databases: `test_db` (the selected database) and `victim` (a database
    /// the client must never address). The server normalizes dot segments in
    /// the raw request path like a real reverse proxy, sets `victim_accessed`
    /// if any request resolves to the victim database, and records every raw
    /// request target. Returns the base database URL, the victim-accessed
    /// flag, the recorded request paths, the server thread handle, and the
    /// shutdown flag.
    #[allow(clippy::type_complexity)]
    fn spawn_traversal_couch() -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind traversal couch");
        let addr = listener.local_addr().expect("traversal couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let victim_accessed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let shutdown_thread = shutdown.clone();
        let victim_thread = victim_accessed.clone();
        let paths_thread = paths.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("traversal couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set traversal couch nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("traversal couch tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let victim = victim_thread.clone();
                            let paths = paths_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_traversal_req(req, victim.clone(), paths.clone())
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, victim_accessed, paths, handle, shutdown)
    }

    /// Serve one request on the two-database fake. Sets `victim` if the raw
    /// path (after dot-segment normalization) resolves to the `victim`
    /// database, records the raw request target, and answers the selected
    /// database's ordinary `chunk1` GET with a leaf chunk, everything else
    /// with 404, PUT with 201, DELETE with 200.
    #[allow(clippy::unused_async)]
    async fn handle_traversal_req(
        req: hyper::Request<hyper::body::Incoming>,
        victim: std::sync::Arc<std::sync::atomic::AtomicBool>,
        paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let path = parts.uri.path().to_string();
        paths.lock().expect("record path").push(path.clone());
        let segments = normalize_raw_path(&path);
        // Any request that resolves to the victim database is a successful
        // escape from the selected database.
        if segments.first().is_some_and(|s| *s == "victim") {
            victim.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let under_test_db = segments.first().is_some_and(|s| *s == "test_db");
        match parts.method {
            hyper::Method::GET if under_test_db && path.ends_with("/chunk1") => {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                    br#"{"_id":"chunk1","_rev":"1-abc","data":"aGVsbG8=","type":"leaf"}"#,
                ));
            }
            hyper::Method::GET if under_test_db => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
            hyper::Method::DELETE => {
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from_static(br#"{"ok":true}"#));
            }
            hyper::Method::PUT => {
                *response.status_mut() = hyper::StatusCode::CREATED;
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[test]
    fn legitimate_doc_id_encodes_slash_and_round_trips() {
        // A filesystem-path id keeps its exact identity: `notes/foo.md` is
        // sent as literal segments `notes%2Ffoo%2Emd`, which CouchDB decodes
        // back to the same document id. No path traversal is possible because
        // the encoded slash and dots are not routing separators.
        assert_eq!(
            utf8_percent_encode("notes/foo.md", DOC_ID_ENCODE_SET).to_string(),
            "notes%2Ffoo%2Emd"
        );
    }

    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn traversal_and_reserved_ids_cannot_escape_selected_database() {
        let (base_db_url, victim_accessed, paths, server, shutdown) = spawn_traversal_couch();
        let db = couchdb_at(&base_db_url);

        // 1. Direct delete_doc with a `../` traversal id: must stay inside
        //    `test_db`, never reach `victim`, and succeed against the 404-free
        //    DELETE handler.
        let result = db.delete_doc("../victim/target", "1-abc").await;
        assert!(
            result.is_ok(),
            "traversal delete_doc must be Ok: {result:?}"
        );
        assert!(
            !victim_accessed.load(std::sync::atomic::Ordering::SeqCst),
            "delete_doc `../victim/target` escaped to the victim database"
        );

        // 2. get_chunk with a `../` traversal id: must not escape the selected
        //    database (returns None from the 404, never touches `victim`).
        let fetched = db.get_chunk("../victim/target").await;
        assert!(
            fetched.is_ok(),
            "traversal get_chunk must be Ok: {fetched:?}"
        );
        assert!(
            !victim_accessed.load(std::sync::atomic::Ordering::SeqCst),
            "get_chunk `../victim/target` escaped to the victim database"
        );

        // 3. save_chunk with reserved characters (`# ? % &` and a space): the
        //    `#`/`?` must be percent-encoded, not truncated or turned into a
        //    fragment/query, and the victim database must stay untouched.
        let reserved_id = "foo#bar?baz%qux& plus";
        let chunk = ChunkDoc::new_encoded(reserved_id.to_string(), b"data".to_vec());
        let result = db.save_chunk(&chunk).await;
        assert!(
            result.is_ok(),
            "reserved-id save_chunk must be Ok: {result:?}"
        );
        assert!(
            !victim_accessed.load(std::sync::atomic::Ordering::SeqCst),
            "reserved-id save_chunk escaped to the victim database"
        );
        {
            let saved_paths = paths.lock().expect("read saved paths");
            let put_path = saved_paths
                .iter()
                .find(|p| p.starts_with("/test_db/foo%23"))
                .expect("save_chunk must encode `#` as %23");
            // `#`, `?` and `%` must be percent-encoded in the request path —
            // never truncated, turned into a fragment/query, or left as a raw
            // reserved char.
            assert!(
                put_path.contains("%23") && put_path.contains("%3F") && put_path.contains("%25"),
                "reserved chars must be percent-encoded in the request path: {put_path}"
            );
            assert!(
                !put_path.contains('#') && !put_path.contains('?'),
                "no literal reserved chars may appear in the request path: {put_path}"
            );
        }

        // 4. get_chunk with a reserved-character id: must not escape and must
        //    not be truncated at `#`/`?`.
        let fetched = db.get_chunk(reserved_id).await;
        assert!(
            fetched.is_ok(),
            "reserved-id get_chunk must be Ok: {fetched:?}"
        );
        assert!(
            !victim_accessed.load(std::sync::atomic::Ordering::SeqCst),
            "reserved-id get_chunk escaped to the victim database"
        );

        // 5. delete_chunks with a traversal child id: get_chunk returns None
        //    (stays in `test_db`), so nothing is deleted and `victim` is never
        //    addressed.
        let result = db.delete_chunks(&["../victim/target".to_string()]).await;
        assert!(
            result.is_ok(),
            "traversal delete_chunks must be Ok: {result:?}"
        );
        assert!(
            !victim_accessed.load(std::sync::atomic::Ordering::SeqCst),
            "delete_chunks `../victim/target` escaped to the victim database"
        );

        // 6. Ordinary chunk operations still pass end-to-end (fetch + delete).
        let result = db.delete_chunks(&["chunk1".to_string()]).await;
        assert!(
            result.is_ok(),
            "ordinary delete_chunks must be Ok: {result:?}"
        );
        assert!(
            !victim_accessed.load(std::sync::atomic::Ordering::SeqCst),
            "ordinary chunk operations must not touch the victim database"
        );

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("traversal couch server join");
    }
    // -----------------------------------------------------------------------
    // Story 6371: structured HTTP errors + consistent bounded retry semantics
    // -----------------------------------------------------------------------

    /// One scripted response for [`spawn_scripted_couch`]. Each request pops
    /// the next step off the queue; an exhausted script defaults to `200`.
    enum ScriptedStep {
        /// Respond immediately with `status` and body.
        Respond { status: u16, body: String },
        /// Hold the connection open for `delay_ms` (longer than the client's
        /// request timeout) before responding, so reqwest times out — a
        /// deterministic "write committed but acknowledgement lost" / transient
        /// transport failure.
        StallThen {
            delay_ms: u64,
            status: u16,
            body: String,
        },
    }

    /// Spawn an in-process scripted fake `CouchDB` server that plays back a
    /// queue of [`ScriptedStep`] responses (one per request), lets tests count
    /// the total requests issued, and records every PUT/POST/DELETE target so
    /// tests can assert a retried write did not duplicate a document. Returns
    /// the base DB URL, the bare server address (for the `couch_rs` client),
    /// the script queue, the request counter, the write map, the server thread
    /// handle, and a shutdown flag.
    #[allow(clippy::type_complexity)]
    fn spawn_scripted_couch(
        steps: Vec<ScriptedStep>,
    ) -> (
        String,
        String,
        std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ScriptedStep>>>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted couch");
        let addr = listener.local_addr().expect("scripted couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let script = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
            steps,
        )));
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writes = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let (shutdown_thread, script_thread, requests_thread, writes_thread) = (
            shutdown.clone(),
            script.clone(),
            requests.clone(),
            writes.clone(),
        );
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scripted couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set scripted couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("scripted couch listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let script = script_thread.clone();
                            let requests = requests_thread.clone();
                            let writes = writes_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let script = script.clone();
                                let requests = requests.clone();
                                let writes = writes.clone();
                                async move {
                                    handle_scripted_req(req, &script, &requests, &writes).await
                                }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (
            base_db_url,
            format!("http://{addr}"),
            script,
            requests,
            writes,
            handle,
            shutdown,
        )
    }

    /// Serve one scripted request: record the request count, persist any
    /// write target (by method+path, so a retried write overwrites and never
    /// duplicates), then play back the next script step.
    #[allow(clippy::type_complexity)]
    async fn handle_scripted_req(
        req: hyper::Request<hyper::body::Incoming>,
        script: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ScriptedStep>>>,
        requests: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        writes: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (parts, body) = req.into_parts();

        // Persist write targets so tests can assert a retried write landed at
        // most once per document (keyed by method + path).
        if matches!(
            parts.method,
            hyper::Method::PUT | hyper::Method::POST | hyper::Method::DELETE
        ) {
            let collected = http_body_util::BodyExt::collect(body).await;
            let body_bytes = collected.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
            if let Ok(mut w) = writes.lock() {
                w.insert(
                    format!("{:?} {}", parts.method, parts.uri),
                    String::from_utf8_lossy(&body_bytes).to_string(),
                );
            }
        }

        let step = script.lock().map_or(None, |mut s| s.pop_front());
        let (status, body_text, delay_ms) = match step {
            Some(ScriptedStep::Respond { status, body }) => (status, body, 0),
            Some(ScriptedStep::StallThen {
                delay_ms,
                status,
                body,
            }) => (status, body, delay_ms),
            None => (200, String::new(), 0),
        };
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        let mut response = hyper::Response::new(http_body_util::Full::new(
            hyper::body::Bytes::from(body_text),
        ));
        *response.status_mut() =
            hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR);
        Ok(response)
    }

    /// A valid chunk document body served by the scripted fake couch.
    fn scripted_chunk_body(chunk_id: &str) -> String {
        format!(
            r#"{{"_id":"{chunk_id}","_rev":"1-abc","data":"aGVsbG8=","type":"leaf","created_at":0}}"#
        )
    }

    /// A valid file document body (with an explicit `_id`/`_rev`) served by the
    /// scripted fake couch to the `couch_rs` client.
    fn scripted_file_body(id: &str) -> String {
        format!(
            r#"{{"_id":"{id}","_rev":"1-abc","children":[],"path":"{id}","type":"plain","size":0}}"#
        )
    }

    /// A `CouchDb` whose `couch_rs` client points at the scripted server's bare
    /// address (so `get_file`/`fetch_metadata` hit it) with a real request
    /// timeout on the direct reqwest client (so timeout retries are testable).
    fn scripted_couchdb(base_db_url: &str, bare_addr: &str, timeout_seconds: u64) -> CouchDb {
        scripted_couchdb_with_retries(base_db_url, bare_addr, timeout_seconds, 3)
    }

    /// Like [`scripted_couchdb`], with a caller-chosen retry limit.
    fn scripted_couchdb_with_retries(
        base_db_url: &str,
        bare_addr: &str,
        timeout_seconds: u64,
        retry_attempts: u32,
    ) -> CouchDb {
        let client = Client::new_no_auth(bare_addr).unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(timeout_seconds))
                .build()
                .expect("build scripted http client"),
            base_db_url: base_db_url.to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(String::new()),
            timeout_seconds,
            retry_attempts,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn get_chunk_retries_500_503_429_then_succeeds() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![
                ScriptedStep::Respond {
                    status: 500,
                    body: String::new(),
                },
                ScriptedStep::Respond {
                    status: 503,
                    body: String::new(),
                },
                ScriptedStep::Respond {
                    status: 429,
                    body: String::new(),
                },
                ScriptedStep::Respond {
                    status: 200,
                    body: scripted_chunk_body("chunk1"),
                },
            ]);
        let db = scripted_couchdb_with_retries(&base_db_url, &bare_addr, 30, 4);
        let result = db.get_chunk("chunk1").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            matches!(result, Ok(Some(_))),
            "transient 500/503/429 must retry to success: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "three transient failures then success == 4 attempts"
        );
    }

    #[tokio::test]
    async fn get_chunk_does_not_retry_permanent_403() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![ScriptedStep::Respond {
                status: 403,
                body: String::new(),
            }]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.get_chunk("chunk1").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            result.is_err(),
            "permanent 403 must fail immediately: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "permanent auth errors must not be retried"
        );
    }

    #[tokio::test]
    async fn get_chunk_404_returns_none() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![ScriptedStep::Respond {
                status: 404,
                body: String::new(),
            }]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.get_chunk("chunk1").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            matches!(result, Ok(None)),
            "a genuine 404 must mean absence: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "404 is permanent and must not be retried"
        );
    }

    #[tokio::test]
    async fn get_chunk_retries_on_timeout() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![
                ScriptedStep::StallThen {
                    delay_ms: 1500,
                    status: 200,
                    body: scripted_chunk_body("chunk1"),
                },
                ScriptedStep::Respond {
                    status: 200,
                    body: scripted_chunk_body("chunk1"),
                },
            ]);
        // 1s request timeout: the first attempt stalls past it and times out,
        // the retry succeeds.
        let db = scripted_couchdb(&base_db_url, &bare_addr, 1);
        let result = db.get_chunk("chunk1").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            matches!(result, Ok(Some(_))),
            "a timeout must be retried to success: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one timeout then success == 2 attempts"
        );
    }

    #[tokio::test]
    async fn get_file_404_means_absence_only() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![ScriptedStep::Respond {
                status: 404,
                body: String::new(),
            }]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.get_file("notes/missing.md").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            matches!(result, Ok(None)),
            "an actual 404 must mean absence: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "404 is permanent and must not be retried"
        );
    }

    #[tokio::test]
    async fn get_file_transient_error_containing_404_in_path_is_not_absence() {
        // Regression: the old substring check treated any error whose text
        // contained "404" (e.g. a connection error mentioning the path
        // "notes/404.md") as a missing document. A structured 500 must surface
        // as an error (after retries), never as Ok(None).
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![
                ScriptedStep::Respond {
                    status: 500,
                    body: "boom for notes/404.md".to_string(),
                },
                ScriptedStep::Respond {
                    status: 500,
                    body: "boom for notes/404.md".to_string(),
                },
                ScriptedStep::Respond {
                    status: 500,
                    body: "boom for notes/404.md".to_string(),
                },
            ]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.get_file("notes/404.md").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            result.is_err(),
            "a 500 mentioning a 404 in the path must error, not look absent: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "a 500 (transient) must retry up to retry_attempts"
        );
    }

    #[tokio::test]
    async fn get_file_present_doc_with_404_in_id_returns_some() {
        let (base_db_url, bare_addr, _script, _requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![ScriptedStep::Respond {
                status: 200,
                body: scripted_file_body("notes/404.md"),
            }]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.get_file("notes/404.md").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            matches!(result, Ok(Some(_))),
            "a present doc whose id merely contains '404' must not be treated as absent: {result:?}"
        );
    }

    #[tokio::test]
    async fn fetch_metadata_404_returns_none() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![ScriptedStep::Respond {
                status: 404,
                body: String::new(),
            }]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.fetch_metadata("notes/missing.md").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            matches!(result, Ok(None)),
            "an actual 404 must mean absence: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "404 is permanent and must not be retried"
        );
    }

    #[tokio::test]
    async fn save_chunk_retries_on_503_then_succeeds() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![
                ScriptedStep::Respond {
                    status: 503,
                    body: String::new(),
                },
                ScriptedStep::Respond {
                    status: 201,
                    body: "{\"ok\":true}".to_string(),
                },
            ]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let chunk = ChunkDoc::new_encoded("h:chunk1".to_string(), b"hello".to_vec());
        let result = db.save_chunk(&chunk).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            result.is_ok(),
            "transient 503 on save must retry to success: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one transient failure then success == 2 attempts"
        );
    }

    #[tokio::test]
    async fn save_chunk_lost_response_retried_without_duplication() {
        // Success-with-lost-response: the first PUT commits but its
        // acknowledgement is lost (the server stalls past the client timeout,
        // so the client sees a transport error). The retry re-PUTs the same
        // chunk id/body, which is idempotent: the server ends with exactly one
        // stored chunk document, never a duplicate.
        let (base_db_url, bare_addr, _script, requests, writes, server, shutdown) =
            spawn_scripted_couch(vec![
                ScriptedStep::StallThen {
                    delay_ms: 1500,
                    status: 201,
                    body: "{\"ok\":true}".to_string(),
                },
                ScriptedStep::Respond {
                    status: 201,
                    body: "{\"ok\":true}".to_string(),
                },
            ]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 1);
        let chunk = ChunkDoc::new_encoded("h:chunk1".to_string(), b"hello".to_vec());
        let result = db.save_chunk(&chunk).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            result.is_ok(),
            "a lost acknowledgement on save must be retried to success: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one lost-ack then success == 2 attempts"
        );
        let put_count = {
            let stored = writes.lock().expect("writes lock");
            stored
                .iter()
                .filter(|(k, _)| k.starts_with("PUT ") && k.contains("h%3Achunk1"))
                .count()
        };
        assert_eq!(
            put_count, 1,
            "an idempotent retry must leave exactly one chunk document"
        );
    }

    #[tokio::test]
    async fn delete_doc_retries_on_500_then_succeeds() {
        let (base_db_url, bare_addr, _script, requests, _writes, server, shutdown) =
            spawn_scripted_couch(vec![
                ScriptedStep::Respond {
                    status: 500,
                    body: String::new(),
                },
                ScriptedStep::Respond {
                    status: 200,
                    body: "{\"ok\":true}".to_string(),
                },
            ]);
        let db = scripted_couchdb(&base_db_url, &bare_addr, 30);
        let result = db.delete_doc("notes/gone.md", "1-abc").await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("scripted couch join");
        assert!(
            result.is_ok(),
            "a transient 500 on delete must retry to success: {result:?}"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "delete_doc must now apply bounded retry (1 failure + 1 success)"
        );
    }

    // -----------------------------------------------------------------------
    // get_changes: since-based incremental feed + first-run bootstrap
    // -----------------------------------------------------------------------

    /// Build a live (non-deleted) `FileDoc` for the fake `_changes`/`_all_docs`
    /// server, tagged with the given mtime so changes are distinguishable.
    fn changes_file_doc(id: &str, mtime_ms: u64) -> FileDoc {
        FileDoc {
            id: id.to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: id.to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(mtime_ms),
            deleted_at: TimestampMillis::default(),
            size: 1,
            doc_type: crate::models::DocType::Plain,
            deleted: false,
            hash: String::new(),
        }
    }

    /// Build a `CouchDb` whose *both* `couch_rs` client (used by
    /// `get_all_files`/`get_update_seq`) and reqwest client (used by
    /// `get_changes_feed`) point at `addr`, with the given remote-path scope.
    fn couchdb_at_full(addr: &str, remote_path: &str) -> CouchDb {
        let client = Client::new_no_auth(addr).unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: format!("{addr}/test_db"),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds: 30,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Like [`couchdb_at_full`], but with a real request client timeout so
    /// tests can exercise the headroom between the long-poll server wait and
    /// the client timeout (the fix for the idle-poll timeout bug).
    fn couchdb_at_with_timeout(addr: &str, remote_path: &str, timeout_seconds: u64) -> CouchDb {
        let client = Client::new_no_auth(addr).unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::builder()
                .timeout(Duration::from_secs(timeout_seconds))
                .build()
                .expect("build http client for test"),
            base_db_url: format!("{addr}/test_db"),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(remote_path.to_string()),
            timeout_seconds,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Spawn an in-process fake `CouchDB` server backed by a real hyper HTTP
    /// server that serves the three endpoints `get_changes` touches:
    ///
    /// - `POST /test_db/_all_docs` (via `couch_rs`), returning `docs` — the
    ///   bootstrap path for a first run with no checkpoint.
    /// - `GET /test_db/_changes` (via reqwest), returning change rows for
    ///   `docs` with `1`-based sequences, filtered to sequences strictly
    ///   greater than the `since` query parameter — the incremental path.
    /// - `GET /test_db` (via `couch_rs`), returning a `DbInfo` whose
    ///   `update_seq` equals the number of `docs`, so bootstrap can checkpoint
    ///   a scope-appropriate sequence.
    ///
    /// Returns the server base URL, the server thread handle, and a shutdown
    /// flag.
    fn spawn_fake_changes_couch(
        docs: Vec<FileDoc>,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake changes couch");
        let addr = listener.local_addr().expect("fake changes addr");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let docs_thread = docs;
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake changes runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake changes nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("fake changes tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let docs = docs_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_fake_changes_couch_req(req, docs.clone())
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {}
                    }
                }
            });
        });
        (format!("http://{addr}"), handle, shutdown)
    }

    /// Serve the three `CouchDB` endpoints used by `get_changes` (see
    /// [`spawn_fake_changes_couch`]). hyper handles HTTP/1.1 framing and
    /// keep-alive, so reqwest and `couch_rs` reuse connections deterministically.
    #[allow(clippy::unused_async)]
    async fn handle_fake_changes_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        docs: Vec<FileDoc>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let method = parts.method.clone();
        let path = parts.uri.path().to_string();
        match (method, path.as_str()) {
            // `couch_rs` percent-encodes the underscore in the db name
            // (`/test%5Fdb`), while our own reqwest path does not, so match on
            // the endpoint suffix rather than the exact path.
            (hyper::Method::POST, p) if p.ends_with("_all_docs") => {
                let rows: Vec<serde_json::Value> = docs
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "key": d.id,
                            "value": { "rev": d.rev.clone().unwrap_or_default() },
                            "doc": d,
                        })
                    })
                    .collect();
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({ "total_rows": rows.len(), "offset": 0, "rows": rows })
                        .to_string(),
                ));
            }
            (hyper::Method::GET, p) if p.ends_with("_changes") => {
                let since = parts
                    .uri
                    .query()
                    .and_then(|q| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            (k == "since").then(|| v.to_string())
                        })
                    })
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let rows: Vec<serde_json::Value> = docs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| (*i as u64) + 1 > since)
                    .map(|(i, d)| {
                        serde_json::json!({
                            "seq": ((i as u64) + 1).to_string(),
                            "id": d.id,
                            "changes": [{ "rev": d.rev.clone().unwrap_or_default() }],
                            "deleted": d.deleted,
                            "doc": d,
                        })
                    })
                    .collect();
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({
                        "results": rows,
                        "last_seq": docs.len().to_string(),
                    })
                    .to_string(),
                ));
            }
            (hyper::Method::GET, p) if p.ends_with("test_db") || p.ends_with("test%5Fdb") => {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({
                        "cluster": { "q": 1, "n": 1, "w": 1, "r": 1 },
                        "compact_running": false,
                        "db_name": "test_db",
                        "disk_format_version": 8,
                        "doc_count": docs.len(),
                        "doc_del_count": 0,
                        "instance_start_time": "0",
                        "purge_seq": "0",
                        "sizes": { "file": 0, "external": 0, "active": 0 },
                        "update_seq": docs.len().to_string(),
                        "props": {},
                    })
                    .to_string(),
                ));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    /// Serve a `_changes` feed from raw JSON row objects so tests can inject
    /// unknown types, system docs, malformed docs, and other non-`FileDoc`
    /// records without ever round-tripping through `FileDoc` serialization.
    ///
    /// Rows are served with `1`-based sequences derived from their position,
    /// filtered to sequences strictly greater than the `since` query parameter
    /// (mirroring [`spawn_fake_changes_couch`]).
    fn spawn_fake_raw_changes_couch(
        rows: Vec<serde_json::Value>,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake raw changes couch");
        let addr = listener.local_addr().expect("fake raw changes addr");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let rows_thread = rows;
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake raw changes runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake raw changes nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("fake raw changes tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let rows = rows_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_fake_raw_changes_couch_req(req, rows.clone())
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {}
                    }
                }
            });
        });
        (format!("http://{addr}"), handle, shutdown)
    }

    /// Serve the `_changes` endpoint from raw JSON rows (see
    /// [`spawn_fake_raw_changes_couch`]).
    #[allow(clippy::unused_async)]
    async fn handle_fake_raw_changes_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        rows: Vec<serde_json::Value>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let method = parts.method.clone();
        let path = parts.uri.path().to_string();
        match (method, path.as_str()) {
            (hyper::Method::GET, p) if p.ends_with("_changes") => {
                let since = parts
                    .uri
                    .query()
                    .and_then(|q| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            (k == "since").then(|| v.to_string())
                        })
                    })
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let results: Vec<serde_json::Value> = rows
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| (*i as u64) + 1 > since)
                    .map(|(i, row)| {
                        let mut r = row.clone();
                        r["seq"] = serde_json::json!(((i as u64) + 1).to_string());
                        r
                    })
                    .collect();
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({ "results": results, "last_seq": rows.len().to_string() })
                        .to_string(),
                ));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    /// Parameters for a stateful fake `CouchDB` that models writes landing
    /// concurrently with a first-run bootstrap.
    ///
    /// Unlike the stateless [`spawn_fake_changes_couch`], the fixture can
    /// return *different* state for the listing versus the sequence/feed,
    /// simulating a document being created, edited or deleted between the
    /// pre-snapshot cursor capture and the end of the bootstrap. `snapshot_seq`
    /// is what `GET /test_db` returns when queried *before* the listing (the
    /// pre-snapshot cursor); `after_seq` is what it returns when queried *after*
    /// the listing — the original racy ordering that would checkpoint over the
    /// intervening writes. `listing_docs` is the file set as of the listing
    /// read; `feed` is the complete ordered change feed `(seq, doc)` from
    /// sequence 0.
    #[derive(Clone)]
    struct RacingChanges {
        snapshot_seq: String,
        after_seq: String,
        listing_docs: Vec<FileDoc>,
        feed: Vec<(u64, FileDoc)>,
    }

    /// Spawn the stateful fake `CouchDB` described by [`RacingChanges`], tracking
    /// whether the listing (`_all_docs`) has been requested so `GET /test_db`
    /// can answer with the pre- or post-listing sequence as appropriate.
    fn spawn_fake_changes_couch_racing(
        state: RacingChanges,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake racing couch");
        let addr = listener.local_addr().expect("fake racing addr");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listing_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let state_thread = state;
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake racing runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake racing nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("fake racing tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let state = state_thread.clone();
                            let listing_called = listing_called.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_fake_changes_couch_racing_req(
                                    req,
                                    state.clone(),
                                    listing_called.clone(),
                                )
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {}
                    }
                }
            });
        });
        (format!("http://{addr}"), handle, shutdown)
    }

    /// Serve the three `CouchDB` endpoints used by `get_changes` against a
    /// [`RacingChanges`] fixture (see [`spawn_fake_changes_couch_racing`]).
    #[allow(clippy::unused_async)]
    async fn handle_fake_changes_couch_racing_req(
        req: hyper::Request<hyper::body::Incoming>,
        state: RacingChanges,
        listing_called: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let method = parts.method.clone();
        let path = parts.uri.path().to_string();
        match (method, path.as_str()) {
            // `couch_rs` percent-encodes the underscore in the db name, so match
            // on the endpoint suffix rather than the exact path.
            (hyper::Method::POST, p) if p.ends_with("_all_docs") => {
                listing_called.store(true, std::sync::atomic::Ordering::SeqCst);
                let rows: Vec<serde_json::Value> = state
                    .listing_docs
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "id": d.id,
                            "key": d.id,
                            "value": { "rev": d.rev.clone().unwrap_or_default() },
                            "doc": d,
                        })
                    })
                    .collect();
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({ "total_rows": rows.len(), "offset": 0, "rows": rows })
                        .to_string(),
                ));
            }
            (hyper::Method::GET, p) if p.ends_with("_changes") => {
                let since = parts
                    .uri
                    .query()
                    .and_then(|q| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            (k == "since").then(|| v.to_string())
                        })
                    })
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let rows: Vec<serde_json::Value> = state
                    .feed
                    .iter()
                    .filter(|(seq, _)| *seq > since)
                    .map(|(seq, d)| {
                        serde_json::json!({
                            "seq": seq.to_string(),
                            "id": d.id,
                            "changes": [{ "rev": d.rev.clone().unwrap_or_default() }],
                            "deleted": d.deleted,
                            "doc": d,
                        })
                    })
                    .collect();
                let last_seq = state
                    .feed
                    .last()
                    .map_or_else(|| "0".to_string(), |(seq, _)| seq.to_string());
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({ "results": rows, "last_seq": last_seq }).to_string(),
                ));
            }
            (hyper::Method::GET, p) if p.ends_with("test_db") || p.ends_with("test%5Fdb") => {
                // The pre-snapshot cursor is captured before the listing, so the
                // fixture reports `snapshot_seq` unless the listing has already
                // been requested (the racy ordering that would skip intervening
                // writes).
                let seq = if listing_called.load(std::sync::atomic::Ordering::SeqCst) {
                    state.after_seq
                } else {
                    state.snapshot_seq
                };
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::json!({
                        "cluster": { "q": 1, "n": 1, "w": 1, "r": 1 },
                        "compact_running": false,
                        "db_name": "test_db",
                        "disk_format_version": 8,
                        "doc_count": state.listing_docs.len(),
                        "doc_del_count": 0,
                        "instance_start_time": "0",
                        "purge_seq": "0",
                        "sizes": { "file": 0, "external": 0, "active": 0 },
                        "update_seq": seq,
                        "props": {},
                    })
                    .to_string(),
                ));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn get_changes_bootstrap_applies_changes_racing_the_snapshot() {
        // A first-run bootstrap must not skip a document created, edited or
        // deleted between the pre-snapshot cursor capture and the end of the
        // listing. The stateful fixture reports the pre-snapshot `update_seq`
        // for the cursor (queried before the listing) while the feed replayed
        // from that cursor carries the racing create/edit/delete.
        let a_listed = changes_file_doc("notes/a.md", 1000); // state at listing time
        let b_listed = changes_file_doc("notes/b.md", 2000);
        let c_new = changes_file_doc("notes/c.md", 3000); // created after the snapshot
        let a_new = changes_file_doc("notes/a.md", 4000); // edited after the snapshot
        let mut b_deleted = changes_file_doc("notes/b.md", 2000); // deleted after the snapshot
        b_deleted.deleted = true;
        b_deleted.deleted_at = TimestampMillis::new(9000);

        let state = RacingChanges {
            snapshot_seq: "2".to_string(), // a@1, b@2 at snapshot time
            after_seq: "5".to_string(),    // the racy post-listing sequence
            listing_docs: vec![a_listed, b_listed],
            feed: vec![
                (1, changes_file_doc("notes/a.md", 1000)),
                (2, changes_file_doc("notes/b.md", 2000)),
                (3, c_new),
                (4, a_new),
                (5, b_deleted),
            ],
        };
        let (addr, server, shutdown) = spawn_fake_changes_couch_racing(state);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, seq) = db
            .get_changes(None)
            .await
            .expect("bootstrap should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake racing couch join");

        // The racing create is not skipped.
        assert!(
            changes.iter().any(|c| c.path() == "notes/c.md"),
            "file created between snapshot and resume must be applied"
        );
        // The racing edit wins over the stale listing copy.
        let a = changes
            .iter()
            .find(|c| c.path() == "notes/a.md")
            .expect("edited file must be present");
        assert_eq!(
            a.mtime().map(chrono::DateTime::timestamp_millis),
            Some(4000),
            "racing edit must win over the listing snapshot"
        );
        // The racing delete is applied, not masked by the listing's live copy.
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, crate::models::Change::RemoteDeleted { path, .. } if path == "notes/b.md")),
            "racing delete must be applied"
        );
        // The resume point is the replayed feed's last_seq, never behind the
        // snapshot cursor, so no intervening change can be skipped.
        assert_eq!(
            seq, "5",
            "bootstrap resumes from the replayed feed's last_seq"
        );
    }

    /// Shared record of the `feed`/`timeout` query params received for each
    /// `_changes` request, captured by [`spawn_recorded_changes_couch`].
    type RecordedFeedQueries = std::sync::Arc<parking_lot::Mutex<Vec<(String, String)>>>;

    /// Spawn an in-process fake `CouchDB` changes server that records the
    /// `feed`/`timeout` query params of every `_changes` request and, for
    /// `longpoll` feeds, simulates the server-side wait by sleeping the
    /// requested `timeout` (capped at `longpoll_delay_cap_ms`).
    ///
    /// Returns the base URL, a shared record of `(feed, timeout)` pairs, the
    /// server thread handle, and a shutdown flag.
    fn spawn_recorded_changes_couch(
        longpoll_delay_cap_ms: u64,
    ) -> (
        String,
        RecordedFeedQueries,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind recorded changes couch");
        let addr = listener.local_addr().expect("recorded changes addr");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let recorded = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let recorded_thread = recorded.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("recorded changes runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set recorded nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("recorded changes tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let recorded = recorded_thread.clone();
                            let cap = longpoll_delay_cap_ms;
                            let service = hyper::service::service_fn(move |req| {
                                handle_recorded_changes_couch_req(req, recorded.clone(), cap)
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_) => {}
                    }
                }
            });
        });
        (format!("http://{addr}"), recorded, handle, shutdown)
    }

    /// Serve `_changes` requests for [`spawn_recorded_changes_couch`], recording
    /// the `feed`/`timeout` query params and simulating the long-poll wait.
    #[allow(clippy::unused_async)]
    async fn handle_recorded_changes_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        recorded: RecordedFeedQueries,
        longpoll_delay_cap_ms: u64,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let method = parts.method.clone();
        let path = parts.uri.path().to_string();
        if method == hyper::Method::GET && path.ends_with("_changes") {
            let query = parts.uri.query().unwrap_or("");
            let param = |key: &str| {
                query.split('&').find_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    (k == key).then(|| v.to_string())
                })
            };
            let feed = param("feed").unwrap_or_default();
            let timeout = param("timeout").unwrap_or_default();
            recorded.lock().push((feed.clone(), timeout.clone()));
            if feed == "longpoll" {
                let wait = timeout
                    .parse::<u64>()
                    .unwrap_or(0)
                    .min(longpoll_delay_cap_ms);
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                serde_json::json!({ "results": [], "last_seq": "0" }).to_string(),
            ));
            return Ok(response);
        }
        *response.status_mut() = hyper::StatusCode::NOT_FOUND;
        Ok(response)
    }

    #[tokio::test]
    async fn periodic_idle_sync_uses_nonblocking_feed_and_succeeds_promptly() {
        // A fake server that would hold a long-poll open long enough to blow
        // past a 1-second request client timeout. Periodic `get_changes` must
        // use the nonblocking `normal` feed, so an idle cycle returns promptly
        // (not waiting the full request timeout) with correct sequence
        // semantics.
        let (addr, recorded, server, shutdown) = spawn_recorded_changes_couch(1500);
        let db = couchdb_at_with_timeout(&addr, "notes/", 1);

        let result = tokio::time::timeout(Duration::from_secs(2), db.get_changes(Some("0")))
            .await
            .expect("idle periodic sync must complete promptly, not wait the request timeout");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("recorded changes couch join");

        let (changes, seq) = result.expect("idle periodic sync must succeed");
        assert!(changes.is_empty(), "idle poll has no changes");
        assert_eq!(seq, "0", "empty poll retains correct sequence semantics");

        let feeds: Vec<String> = recorded.lock().iter().map(|(f, _)| f.clone()).collect();
        assert_eq!(
            feeds,
            vec!["normal".to_string()],
            "periodic sync must use the nonblocking normal feed"
        );
    }

    #[tokio::test]
    async fn live_longpoll_budgets_server_wait_below_client_timeout() {
        // Live long-poll with the default 30s budget must ask the server to
        // wait no more than budget - transport headroom, leaving headroom for
        // the response to complete and decode below the 30s client timeout.
        let (addr, recorded, server, shutdown) = spawn_recorded_changes_couch(0);
        let db = couchdb_at_with_timeout(&addr, "notes/", 30);

        let (entries, seq) = db
            .get_changes_feed("0", ChangesFeed::LongPoll, 30_000)
            .await
            .expect("live long-poll must succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("recorded changes couch join");

        assert!(entries.is_empty(), "empty poll has no entries");
        assert_eq!(seq, "0", "empty poll retains correct sequence semantics");
        let queries = recorded.lock();
        let (feed, timeout) = &queries[0];
        assert_eq!(feed, "longpoll", "live feed must use longpoll");
        assert_eq!(
            timeout.parse::<u64>().unwrap(),
            30_000 - LONGPOLL_TRANSPORT_HEADROOM_MS,
            "live long-poll must leave transport headroom below the client timeout"
        );
        drop(queries);
    }

    #[tokio::test]
    async fn live_longpoll_short_timeout_has_headroom_and_succeeds() {
        // With a 1-second configured timeout, the server-side wait is budgeted
        // to the 1ms floor, so the poll returns with ~1s of transport headroom
        // and succeeds instead of timing out while the response is in flight.
        let (addr, recorded, server, shutdown) = spawn_recorded_changes_couch(1500);
        let db = couchdb_at_with_timeout(&addr, "notes/", 1);

        let (entries, seq) = db
            .get_changes_feed("0", ChangesFeed::LongPoll, 1_000)
            .await
            .expect("live long-poll with a short timeout must succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("recorded changes couch join");

        assert!(entries.is_empty(), "empty poll has no entries");
        assert_eq!(seq, "0", "empty poll retains correct sequence semantics");
        let queries = recorded.lock();
        let (feed, timeout) = &queries[0];
        assert_eq!(feed, "longpoll", "live feed must use longpoll");
        assert_eq!(
            timeout.parse::<u64>().unwrap(),
            1,
            "short timeout must floor the long-poll wait so it returns with headroom"
        );
        drop(queries);
    }

    #[tokio::test]
    async fn get_changes_returns_only_changes_since_checkpoint() {
        // Three in-scope docs on the feed, sequenced 1..=3. A checkpoint at
        // sequence 2 must yield only the change at sequence 3.
        let docs = vec![
            changes_file_doc("notes/a.md", 1000),
            changes_file_doc("notes/b.md", 2000),
            changes_file_doc("notes/c.md", 3000),
        ];
        let (addr, server, shutdown) = spawn_fake_changes_couch(docs);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, seq) = db
            .get_changes(Some("2"))
            .await
            .expect("get_changes should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake changes couch join");

        let paths: Vec<&str> = changes.iter().map(Change::path).collect();
        assert_eq!(
            paths,
            vec!["notes/c.md"],
            "only changes after the checkpoint"
        );
        assert_eq!(seq, "3", "returned seq is the feed last_seq");
    }

    #[tokio::test]
    async fn get_changes_tombstone_uses_authoritative_delete_time_not_stale_mtime() {
        // A soft-delete tombstone preserves the deleted file's own (old) mtime
        // but stamps an authoritative deletion time. The remote-delete change
        // must carry that deletion time so a stale preserved mtime cannot
        // suppress propagation of a genuinely-newer delete.
        let mut tombstone = changes_file_doc("notes/deleted.md", 1000); // stale file mtime
        tombstone.deleted = true;
        tombstone.deleted_at = TimestampMillis::new(9000); // authoritative delete time
        let docs = vec![tombstone];
        let (addr, server, shutdown) = spawn_fake_changes_couch(docs);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, _seq) = db
            .get_changes(None)
            .await
            .expect("get_changes should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake changes couch join");

        let delete = changes
            .iter()
            .find(|c| {
                matches!(c, crate::models::Change::RemoteDeleted { path, .. } if path == "notes/deleted.md")
            })
            .expect("tombstone must surface as a remote delete");
        assert_eq!(
            delete.mtime().map(chrono::DateTime::timestamp_millis),
            Some(9000),
            "remote delete must carry the authoritative deletion time, not the stale file mtime"
        );
    }

    #[tokio::test]
    async fn get_changes_first_run_bootstraps_existing_files() {
        // No checkpoint: get_changes must materialize the existing in-scope
        // file set rather than returning empty.
        let docs = vec![
            changes_file_doc("notes/a.md", 1000),
            changes_file_doc("notes/b.md", 2000),
        ];
        let (addr, server, shutdown) = spawn_fake_changes_couch(docs);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, seq) = db
            .get_changes(None)
            .await
            .expect("get_changes should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake changes couch join");

        let mut paths: Vec<&str> = changes.iter().map(Change::path).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["notes/a.md", "notes/b.md"]);
        assert_eq!(seq, "2", "bootstrap checkpoints the DB update_seq");
    }

    /// Build a raw `_changes` row with an optional body and optional hard-delete
    /// flag, for [`spawn_fake_raw_changes_couch`].
    fn raw_row(id: &str, deleted: bool, doc: Option<serde_json::Value>) -> serde_json::Value {
        let mut row = serde_json::json!({
            "id": id,
            "changes": [{ "rev": "1-abc" }],
        });
        if deleted {
            row["deleted"] = serde_json::json!(true);
        }
        if let Some(doc) = doc {
            row["doc"] = doc;
        }
        row
    }

    /// Build a well-formed `plain` `FileDoc` body for a raw feed row.
    fn raw_file_doc(id: &str, mtime_ms: u64) -> serde_json::Value {
        serde_json::json!({
            "_id": id,
            "_rev": "1-abc",
            "type": "plain",
            "path": id,
            "children": [],
            "mtime": mtime_ms,
            "ctime": 0,
            "size": 1,
            "deleted": false,
            "deleted_at": 0,
            "hash": "",
        })
    }

    #[test]
    fn is_chunk_id_requires_pure_hex_suffix() {
        assert!(is_chunk_id("h:deadbeefcafe"));
        assert!(is_chunk_id("h:0"));
        assert!(
            !is_chunk_id("h:report.md"),
            "a tracked file path with an extension is not a chunk"
        );
        assert!(
            !is_chunk_id("h:"),
            "a bare `h:` with no hex suffix is not a chunk"
        );
        assert!(
            !is_chunk_id("notes/h:thing.md"),
            "a scoped file id is not a chunk"
        );
        assert!(!is_chunk_id("not-a-chunk"));
    }

    #[tokio::test]
    async fn get_changes_mixed_types_do_not_break_scoped_feed() {
        // A scoped changes feed containing unrelated/unsupported records must
        // not fail: unknown types, design docs, and leaf chunks are isolated;
        // only validated file records (plus tracked h:-named files) surface.
        let rows = vec![
            raw_row(
                "notes/good.md",
                false,
                Some(raw_file_doc("notes/good.md", 1000)),
            ),
            // design/system doc must be excluded
            raw_row(
                "_design/notes",
                false,
                Some(serde_json::json!({ "_id": "_design/notes" })),
            ),
            // unknown type must not break the feed
            raw_row(
                "notes/custom.md",
                false,
                Some(serde_json::json!({
                    "_id": "notes/custom.md", "type": "custom", "path": "notes/custom.md"
                })),
            ),
            // leaf chunk without an h: id is not a file
            raw_row(
                "notes/leaf-no-h.md",
                false,
                Some(serde_json::json!({
                    "_id": "notes/leaf-no-h.md", "type": "leaf", "children": []
                })),
            ),
            // no `type` field -> defaults to plain -> a file
            raw_row(
                "notes/no-type.md",
                false,
                Some(serde_json::json!({
                    "_id": "notes/no-type.md", "path": "notes/no-type.md",
                    "children": [], "mtime": 2000, "size": 2,
                    "deleted": false, "deleted_at": 0, "hash": "",
                })),
            ),
            // hard-deleted chunk id (pure hex) -> chunk cleanup, not a file delete
            raw_row("h:deadbeefcafe", true, None),
            // hard-deleted tracked file whose id begins with `h:` -> real delete
            raw_row("h:report.md", true, None),
            // out-of-scope doc is ignored entirely
            raw_row(
                "other.md",
                false,
                Some(serde_json::json!({ "_id": "other.md", "type": "custom" })),
            ),
        ];
        let (addr, server, shutdown) = spawn_fake_raw_changes_couch(rows);
        // Empty scope: root-level chunk ids and root-level h:-named files
        // coexist, which is exactly where a `h:`-prefixed tracked file could be
        // mistaken for a chunk.
        let db = couchdb_at_full(&addr, "");

        let (changes, seq) = db
            .get_changes(Some("0"))
            .await
            .expect("mixed-type feed must not fail");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake raw changes couch join");

        let mut paths: Vec<&str> = changes.iter().map(Change::path).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec!["h:report.md", "notes/good.md", "notes/no-type.md"],
            "only validated in-scope files and tracked h:-named deletions surface"
        );
        assert!(
            changes.iter().any(|c| matches!(
                c,
                Change::RemoteDeleted { path, .. } if path == "h:report.md"
            )),
            "tracked h:-named file deletion must not be discarded as a chunk delete"
        );
        assert_eq!(
            seq, "8",
            "the feed last_seq is still returned even when rows are skipped"
        );
    }

    #[tokio::test]
    async fn get_changes_malformed_in_scope_file_is_actionable_error() {
        // A `plain`-typed in-scope document with a malformed body must produce
        // an actionable error rather than silently dropping the change (which
        // would advance the checkpoint past it without triage).
        let rows = vec![
            raw_row(
                "notes/good.md",
                false,
                Some(raw_file_doc("notes/good.md", 1000)),
            ),
            // mtime is a non-numeric string -> FileDoc decode fails
            raw_row(
                "notes/broken.md",
                false,
                Some(serde_json::json!({
                    "_id": "notes/broken.md", "type": "plain", "path": "notes/broken.md",
                    "children": [], "mtime": "not-a-number", "size": 1,
                })),
            ),
        ];
        let (addr, server, shutdown) = spawn_fake_raw_changes_couch(rows);
        let db = couchdb_at_full(&addr, "notes/");

        let err = db
            .get_changes(Some("0"))
            .await
            .expect_err("malformed in-scope file must error");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake raw changes couch join");

        let err_str = err.to_string();
        assert!(
            err_str.contains("notes/broken.md"),
            "error must name the malformed file, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn get_changes_malformed_out_of_scope_doc_is_ignored() {
        // A malformed document outside the sync scope is filtered by id before
        // any decode, so it neither surfaces nor fails the feed.
        let rows = vec![
            raw_row(
                "outside/broken.md",
                false,
                Some(serde_json::json!({
                    "_id": "outside/broken.md", "type": "plain", "mtime": "garbage"
                })),
            ),
            raw_row(
                "notes/good.md",
                false,
                Some(raw_file_doc("notes/good.md", 1000)),
            ),
        ];
        let (addr, server, shutdown) = spawn_fake_raw_changes_couch(rows);
        let db = couchdb_at_full(&addr, "notes/");

        let (changes, _seq) = db
            .get_changes(Some("0"))
            .await
            .expect("out-of-scope malformed doc must not break the feed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake raw changes couch join");

        let paths: Vec<&str> = changes.iter().map(Change::path).collect();
        assert_eq!(
            paths,
            vec!["notes/good.md"],
            "only the in-scope valid file surfaces"
        );
    }

    // -----------------------------------------------------------------------
    // obsolete_tombstones
    // -----------------------------------------------------------------------

    fn tombstone(id: &str, mtime_ms: u64) -> FileDoc {
        FileDoc {
            id: id.to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: id.to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(mtime_ms),
            deleted_at: TimestampMillis::default(),
            size: 0,
            doc_type: crate::models::DocType::Plain,
            deleted: true,
            hash: String::new(),
        }
    }

    #[test]
    fn obsolete_tombstones_prunes_only_old_deleted_docs() {
        let cutoff = TimestampMillis::new(1_000);
        let old = tombstone("notes/old.md", 500);
        let fresh = tombstone("notes/fresh.md", 2_000);
        let docs = vec![old, fresh];

        let obsolete = obsolete_tombstones(&docs, cutoff);

        assert_eq!(obsolete.len(), 1);
        assert_eq!(obsolete[0].id, "notes/old.md");
    }

    #[test]
    fn obsolete_tombstones_keeps_live_files_and_chunks() {
        let cutoff = TimestampMillis::new(1_000);
        let live = FileDoc {
            id: "notes/live.md".to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: "notes/live.md".to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(500),
            deleted_at: TimestampMillis::default(),
            size: 10,
            doc_type: crate::models::DocType::Plain,
            deleted: false,
            hash: String::new(),
        };
        let chunk = FileDoc {
            id: "h:chunk1".to_string(),
            rev: Some("1-abc".to_string()),
            children: vec![],
            path: "h:chunk1".to_string(),
            ctime: TimestampMillis::new(0),
            mtime: TimestampMillis::new(500),
            deleted_at: TimestampMillis::default(),
            size: 0,
            doc_type: crate::models::DocType::Leaf,
            deleted: true,
            hash: String::new(),
        };
        let docs = vec![live, chunk];

        assert!(obsolete_tombstones(&docs, cutoff).is_empty());
    }

    #[test]
    fn obsolete_tombstones_boundary_at_cutoff_is_not_pruned() {
        // A tombstone whose mtime equals the cutoff is not yet obsolete.
        let cutoff = TimestampMillis::new(1_000);
        let at_cutoff = tombstone("notes/edge.md", 1_000);
        assert!(obsolete_tombstones(&[at_cutoff], cutoff).is_empty());
    }

    #[tokio::test]
    async fn prune_tombstones_deletes_only_obsolete_docs_from_couchdb() {
        let (base_db_url, conn_count, delete_count, server, shutdown) = spawn_tombstone_couch();
        let db = couchdb_at_ephemeral(&base_db_url);

        // The fake server serves two tombstones: one old (mtime 0) and one
        // fresh (mtime = now). Only the old one should be pruned.
        let pruned = db.prune_tombstones(Duration::from_hours(1)).await.unwrap();
        assert_eq!(pruned, 1, "only the obsolete tombstone should be pruned");

        // Exactly one DELETE must have been issued (for the old tombstone).
        assert_eq!(
            delete_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one obsolete tombstone should be hard-deleted"
        );

        drop(db);
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");
        let used = conn_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!((1..=2).contains(&used));
    }

    // -----------------------------------------------------------------------
    // Tombstone-serving fake CouchDB server
    // -----------------------------------------------------------------------

    /// Build a `CouchDb` whose `couch_rs` client points at the given ephemeral
    /// fake-server base URL. Unlike `couchdb_at`, the `couch_rs` `db` (used by
    /// `get_all_files`) is pointed at the fake server's origin rather than the
    /// hard-coded `localhost:15984`, so `prune_tombstones`' document fetch hits
    /// the fake server.
    fn couchdb_at_ephemeral(base_db_url: &str) -> CouchDb {
        let origin = base_db_url.trim_end_matches("/test_db");
        let client = Client::new_no_auth(origin).unwrap();
        let db = Database::new("test_db".to_string(), client.clone());
        CouchDb {
            client,
            db,
            http_client: HttpClient::new(),
            base_db_url: base_db_url.to_string(),
            db_name: DatabaseName::new("test_db"),
            auth: None,
            remote_path: RemotePath::new(String::new()),
            timeout_seconds: 30,
            retry_attempts: 3,
            create_db: true,
            legacy_chunk_data: crate::models::LegacyChunkData::Base64,
            test_state: None,
            test_write_calls: std::sync::atomic::AtomicUsize::new(0),
            test_backoff: Duration::from_millis(1),
        }
    }

    /// Spawn a fake `CouchDB` server that answers `POST /_all_docs` with a
    /// fixed pair of tombstone documents (one old, one fresh) and counts
    /// `DELETE` requests. Returns the base URL, a connection counter, a delete
    /// counter, the server thread handle, and a shutdown flag.
    #[allow(clippy::type_complexity)]
    fn spawn_tombstone_couch() -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake couch");
        let addr = listener.local_addr().expect("fake couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let conn_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delete_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_thread = shutdown.clone();
        let conn_count_thread = conn_count.clone();
        let delete_count_thread = delete_count.clone();
        let now_ms = crate::models::TimestampMillis::now().as_u64();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake couch tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            conn_count_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let delete_count_thread = delete_count_thread.clone();
                            let service = hyper::service::service_fn(move |req| {
                                handle_tombstone_couch_req(req, now_ms, delete_count_thread.clone())
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, conn_count, delete_count, handle, shutdown)
    }

    #[allow(clippy::unused_async)]
    async fn handle_tombstone_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        now_ms: u64,
        delete_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        match parts.method {
            hyper::Method::POST => {
                let body = format!(
                    r#"{{"total_rows":2,"offset":0,"rows":[
                        {{"id":"notes/old.md","key":null,"value":{{"rev":"1-aaa"}},"doc":{{"_id":"notes/old.md","_rev":"1-aaa","path":"notes/old.md","ctime":0,"mtime":0,"size":0,"type":"plain","deleted":true}}}},
                        {{"id":"notes/fresh.md","key":null,"value":{{"rev":"1-bbb"}},"doc":{{"_id":"notes/fresh.md","_rev":"1-bbb","path":"notes/fresh.md","ctime":0,"mtime":{now_ms},"size":0,"type":"plain","deleted":true}}}}
                    ]}}"#
                );
                *response.body_mut() = http_body_util::Full::new(body.into());
            }
            hyper::Method::DELETE => {
                delete_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from_static(br#"{"ok":true}"#));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    /// Regression test for the macOS-CI flake: the fake `CouchDB` server must not
    /// reset the socket between the GET and DELETE requests issued by the real
    /// reqwest client during `delete_chunks`. The fake server is a real hyper
    /// HTTP server, so framing and keep-alive are handled correctly, and
    /// `delete_chunks` succeeds deterministically. Asserting both requests
    /// complete successfully guards against any future regression back to a
    /// connection-resetting fake.
    ///
    /// Reqwest may either reuse the pooled connection or, depending on timing,
    /// open a fresh one for the DELETE, so the connection count is a sanity
    /// bound rather than an exact assertion: with two requests the server must
    /// not open a new connection per request, which would indicate resets.
    #[tokio::test]
    async fn fake_couch_serves_get_then_delete_on_one_connection() {
        let (base_db_url, conn_count, server, shutdown) = spawn_fake_couch(200);
        let db = couchdb_at(&base_db_url);

        // GET the chunk metadata, then DELETE it, and read the responses to
        // completion so reqwest's pool can reuse the connection for the second
        // request when it chooses to.
        let result = db.delete_chunks(&["chunk1".to_string()]).await;
        assert!(
            result.is_ok(),
            "GET then DELETE on one fake server must succeed, got: {result:?}"
        );

        // Drain the pooled connection so the connection thread can observe the
        // client closing it before we tear the server down.
        drop(db);
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        // The server must never reset and reconnect between the two requests:
        // each request may share the connection or use one fresh connection,
        // but never more than the number of requests. A larger count would mean
        // the server was slamming the socket and forcing reconnects.
        let used = conn_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (1..=2).contains(&used),
            "fake CouchDB used {used} connections for two requests, indicating it is resetting \
             connections and forcing reconnects"
        );
    }

    // -----------------------------------------------------------------------
    // Task 3624: fail partial downloads (missing chunk) and verify hash
    // -----------------------------------------------------------------------

    /// Spawn a fake `CouchDB` server that serves a `FileDoc` (with one child
    /// chunk) on GET for the file id but returns `404` for the chunk id —
    /// simulating a partial/corrupt remote download where a referenced chunk is
    /// missing. Returns the base database URL, the server thread handle, and a
    /// shutdown flag.
    fn spawn_fake_missing_chunk_couch(
        file_id: &str,
        chunk_id: &str,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake chunk couch");
        let addr = listener.local_addr().expect("fake chunk couch addr");
        let base_db_url = format!("http://{addr}");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let file_id = file_id.to_string();
        let chunk_id = chunk_id.to_string();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake chunk couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake chunk couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake chunk couch listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let file_id = file_id.clone();
                            let chunk_id = chunk_id.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let file_id = file_id.clone();
                                let chunk_id = chunk_id.clone();
                                async move {
                                    handle_fake_missing_chunk_req(req, &file_id, &chunk_id).await
                                }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, handle, shutdown)
    }

    /// Serve a `GET` for the file doc (with one child chunk) and `404` for the
    /// chunk id, mimicking a missing chunk.
    #[allow(clippy::unused_async)]
    async fn handle_fake_missing_chunk_req(
        req: hyper::Request<hyper::body::Incoming>,
        file_id: &str,
        chunk_id: &str,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        if parts.method == hyper::Method::GET && decoded_last_segment(parts.uri.path()) == chunk_id
        {
            // The chunk is missing: return 404 so `get_chunk` yields None.
            *response.status_mut() = hyper::StatusCode::NOT_FOUND;
        } else if parts.method == hyper::Method::GET {
            // Any other GET is the file doc (couch_rs percent-encodes the path,
            // so match on the chunk absence above rather than the file id).
            let body = format!(
                r#"{{"_id":"{file_id}","_rev":"1-abc","children":["{chunk_id}"],"path":"{file_id}","type":"plain","size":5}}"#
            );
            *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(body));
        } else {
            *response.status_mut() = hyper::StatusCode::NOT_FOUND;
        }
        Ok(response)
    }

    #[tokio::test]
    async fn get_file_content_errors_on_missing_chunk() {
        let file_id = "notes/missing.md";
        let chunk_id = "h:chunk1";
        let (base_db_url, server, shutdown) = spawn_fake_missing_chunk_couch(file_id, chunk_id);
        let db = couchdb_at_full(&base_db_url, "");

        let result = db.get_file_content(file_id).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert!(
            result.is_err(),
            "a missing chunk must error the download, not truncate; got: {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not found") && err.contains(file_id),
            "error should identify the missing chunk and file: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Story 6355: validate content against one consistent metadata snapshot
    // -----------------------------------------------------------------------

    /// Spawn a fake `CouchDB` serving an explicit set of file documents (raw
    /// JSON bodies) and chunk documents, letting tests exercise the
    /// content-validation logic against arbitrary shapes: declared-nonempty
    /// files with no chunks, size mismatches, duplicate or wrong-type chunk
    /// records, zero-length files, and supplied-snapshot reuse. Returns the
    /// base database URL, the server thread handle, and a shutdown flag.
    #[allow(clippy::type_complexity)]
    fn spawn_fake_content_couch(
        file_docs: std::collections::HashMap<String, String>,
        chunks: std::collections::HashMap<String, ChunkDoc>,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake content couch");
        let addr = listener.local_addr().expect("fake content couch addr");
        let base_db_url = format!("http://{addr}");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake content couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake content couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake content couch listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let file_docs = file_docs.clone();
                            let chunks = chunks.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let file_docs = file_docs.clone();
                                let chunks = chunks.clone();
                                async move {
                                    handle_fake_content_req(req, &file_docs, &chunks).await
                                }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, handle, shutdown)
    }

    /// Serve a `GET` for the matching file document JSON or chunk document from
    /// the supplied maps, `404` for anything unknown.
    #[allow(clippy::unused_async)]
    async fn handle_fake_content_req(
        req: hyper::Request<hyper::body::Incoming>,
        file_docs: &std::collections::HashMap<String, String>,
        chunks: &std::collections::HashMap<String, ChunkDoc>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        if parts.method == hyper::Method::GET {
            let decoded_id = decoded_last_segment(parts.uri.path());
            if let Some((_, chunk)) = chunks.iter().find(|(id, _)| decoded_id == **id) {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    serde_json::to_string(chunk).expect("serialize chunk"),
                ));
            } else if let Some((_, body)) = file_docs.iter().find(|(id, _)| decoded_id == **id) {
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from(body.as_bytes().to_vec()));
            } else {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        } else {
            *response.status_mut() = hyper::StatusCode::NOT_FOUND;
        }
        Ok(response)
    }

    fn leaf_chunk(id: &str, data: &[u8]) -> ChunkDoc {
        let mut chunk = ChunkDoc::new_encoded(id.to_string(), data.to_vec());
        chunk.rev = Some("1-abc".to_string());
        chunk
    }

    #[tokio::test]
    async fn get_file_content_rejects_declared_nonempty_file_with_no_chunks() {
        let file_id = "ghost.md";
        let file_json = format!(
            r#"{{"_id":"{file_id}","_rev":"1-abc","children":[],"path":"{file_id}","type":"plain","size":123,"hash":""}}"#
        );
        let (base_db_url, server, shutdown) = spawn_fake_content_couch(
            std::collections::HashMap::from([(file_id.to_string(), file_json)]),
            std::collections::HashMap::new(),
        );
        let db = couchdb_at_full(&base_db_url, "");

        let result = db.get_file_content(file_id).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake content couch server join");

        let err = result
            .expect_err("a declared-nonempty file with no chunks must error, not return empty")
            .to_string();
        assert!(
            err.contains("no chunks") && err.contains(file_id),
            "error must identify the missing content: {err}"
        );
    }

    #[tokio::test]
    async fn get_file_content_rejects_truncated_bytes_via_size_mismatch() {
        let file_id = "trunc.md";
        // Declares 10 bytes (hashless legacy) but its two chunks only carry 6.
        let file_json = format!(
            r#"{{"_id":"{file_id}","_rev":"1-abc","children":["h:c1","h:c2"],"path":"{file_id}","type":"plain","size":10,"hash":""}}"#
        );
        let chunks = std::collections::HashMap::from([
            ("h:c1".to_string(), leaf_chunk("h:c1", b"abc")),
            ("h:c2".to_string(), leaf_chunk("h:c2", b"def")),
        ]);
        let (base_db_url, server, shutdown) = spawn_fake_content_couch(
            std::collections::HashMap::from([(file_id.to_string(), file_json)]),
            chunks,
        );
        let db = couchdb_at_full(&base_db_url, "");

        let result = db.get_file_content(file_id).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake content couch server join");

        let err = result
            .expect_err("truncated bytes must error via the size mismatch, not be accepted")
            .to_string();
        assert!(
            err.contains("size mismatch") && err.contains("10") && err.contains('6'),
            "error must name expected/downloaded sizes: {err}"
        );
    }

    #[tokio::test]
    async fn get_file_content_rejects_duplicate_chunk_ids() {
        let file_id = "dup.md";
        let file_json = format!(
            r#"{{"_id":"{file_id}","_rev":"1-abc","children":["h:c1","h:c1"],"path":"{file_id}","type":"plain","size":3,"hash":""}}"#
        );
        let chunks =
            std::collections::HashMap::from([("h:c1".to_string(), leaf_chunk("h:c1", b"abc"))]);
        let (base_db_url, server, shutdown) = spawn_fake_content_couch(
            std::collections::HashMap::from([(file_id.to_string(), file_json)]),
            chunks,
        );
        let db = couchdb_at_full(&base_db_url, "");

        let result = db.get_file_content(file_id).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake content couch server join");

        let err = result
            .expect_err("a duplicate chunk id must be rejected, not double-counted")
            .to_string();
        assert!(
            err.contains("Duplicate chunk") && err.contains("h:c1"),
            "error must name the duplicate chunk: {err}"
        );
    }

    #[tokio::test]
    async fn get_file_content_rejects_wrong_type_chunk_record() {
        let file_id = "wrongtype.md";
        let file_json = format!(
            r#"{{"_id":"{file_id}","_rev":"1-abc","children":["h:c1"],"path":"{file_id}","type":"plain","size":3,"hash":""}}"#
        );
        // A chunk record whose doc_type is not Leaf violates the format contract.
        let mut bad = leaf_chunk("h:c1", b"abc");
        bad.doc_type = crate::models::DocType::Plain;
        let chunks = std::collections::HashMap::from([("h:c1".to_string(), bad)]);
        let (base_db_url, server, shutdown) = spawn_fake_content_couch(
            std::collections::HashMap::from([(file_id.to_string(), file_json)]),
            chunks,
        );
        let db = couchdb_at_full(&base_db_url, "");

        let result = db.get_file_content(file_id).await;
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake content couch server join");

        let err = result
            .expect_err("a wrong-type chunk record must be rejected, not concatenated")
            .to_string();
        assert!(
            err.contains("non-leaf"),
            "error must name the wrong type: {err}"
        );
    }

    #[tokio::test]
    async fn get_file_content_accepts_zero_length_file() {
        let file_id = "empty.md";
        let file_json = format!(
            r#"{{"_id":"{file_id}","_rev":"1-abc","children":[],"path":"{file_id}","type":"plain","size":0,"hash":""}}"#
        );
        let (base_db_url, server, shutdown) = spawn_fake_content_couch(
            std::collections::HashMap::from([(file_id.to_string(), file_json)]),
            std::collections::HashMap::new(),
        );
        let db = couchdb_at_full(&base_db_url, "");

        let content = db
            .get_file_content(file_id)
            .await
            .expect("a genuine zero-length file must download as empty");
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake content couch server join");

        assert!(
            content.is_empty(),
            "zero-length file must yield empty content"
        );
    }

    #[tokio::test]
    async fn get_file_content_for_doc_uses_supplied_snapshot_not_store_metadata() {
        let file_id = "notes/snap.md";
        // The store's own file doc points at only c1 (size 3)...
        let store_file_json = format!(
            r#"{{"_id":"{file_id}","_rev":"1-abc","children":["h:c1"],"path":"{file_id}","type":"plain","size":3,"hash":""}}"#
        );
        // ...but the caller selects a newer snapshot that also includes c2 (size 6).
        let selected = FileDoc::new(file_id.to_string(), String::new(), 6);
        let mut selected = selected;
        selected.path = file_id.to_string();
        selected.children = vec!["h:c1".to_string(), "h:c2".to_string()];
        let chunks = std::collections::HashMap::from([
            ("h:c1".to_string(), leaf_chunk("h:c1", b"abc")),
            ("h:c2".to_string(), leaf_chunk("h:c2", b"def")),
        ]);
        let (base_db_url, server, shutdown) = spawn_fake_content_couch(
            std::collections::HashMap::from([(file_id.to_string(), store_file_json)]),
            chunks,
        );
        let db = couchdb_at_full(&base_db_url, "");

        let content = db
            .get_file_content_for_doc(&selected)
            .await
            .expect("content must be assembled from the supplied snapshot");
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake content couch server join");

        assert_eq!(
            content,
            b"abcdef".to_vec(),
            "the supplied snapshot's children (c1+c2) must be used, not the store's (c1 only)"
        );
    }

    /// In-memory fake `CouchDB` chunk store that also remembers the insertion
    /// order of chunk ids, so a synthesized file document's `children` list
    /// reconstructs content in the exact order the chunks were uploaded — the
    /// same ordering guarantee the sync engine records in a real file doc.
    #[derive(Default)]
    struct ChunkStore {
        map: std::collections::HashMap<String, ChunkDoc>,
        order: Vec<String>,
    }

    /// Spawn an in-process fake `CouchDB` that stores uploaded chunks in
    /// memory and serves them back, so a byte buffer can round-trip through
    /// `upload_file_content` → `get_file_content`.
    fn spawn_fake_chunk_store_couch() -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake store couch");
        let addr = listener.local_addr().expect("fake store couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let store: std::sync::Arc<parking_lot::Mutex<ChunkStore>> =
            std::sync::Arc::new(parking_lot::Mutex::new(ChunkStore::default()));
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake store couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set fake store couch nonblocking");
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("fake store couch listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let store = store.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let store = store.clone();
                                async move { handle_fake_chunk_store_req(req, store).await }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, handle, shutdown)
    }

    /// Serve `PUT` chunk storage and `GET` chunk/file-doc retrieval from the
    /// in-memory store. Any unknown `GET` is treated as a file document whose
    /// `children` point at the stored chunk ids in insertion (upload) order, so
    /// downloaded content is reassembled from the uploaded chunks.
    #[allow(clippy::unused_async)]
    async fn handle_fake_chunk_store_req(
        req: hyper::Request<hyper::body::Incoming>,
        store: std::sync::Arc<parking_lot::Mutex<ChunkStore>>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let id = decoded_last_segment(parts.uri.path());
        match (parts.method.clone(), id.as_str()) {
            (hyper::Method::PUT, id) if !id.is_empty() => {
                use http_body_util::BodyExt;
                let bytes = body.collect().await.expect("collect chunk body").to_bytes();
                let chunk: ChunkDoc = serde_json::from_slice(&bytes).expect("parse stored chunk");
                {
                    let mut store = store.lock();
                    store.map.insert(id.to_string(), chunk);
                    store.order.push(id.to_string());
                }
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    r#"{"ok":true,"rev":"1-abc"}"#,
                ));
            }
            (hyper::Method::GET, id) if !id.is_empty() => {
                let (chunk, children, size) = {
                    let store = store.lock();
                    let size = store
                        .map
                        .values()
                        .map(|c| {
                            c.content_bytes(crate::models::LegacyChunkData::Base64)
                                .map_or(0, |bytes| bytes.len() as u64)
                        })
                        .sum::<u64>();
                    (store.map.get(id).cloned(), store.order.clone(), size)
                };
                let body = chunk.map_or_else(
                    || {
                        // Any unknown GET is the file doc: point its children
                        // at every stored chunk id in upload order so content
                        // is reassembled in order, and declare the exact
                        // combined size so the download's length validation
                        // passes.
                        let children_json =
                            serde_json::to_string(&children).expect("children json");
                        format!(
                            r#"{{"_id":"{id}","_rev":"1-abc","children":{children_json},"path":"{id}","type":"plain","size":{size}}}"#
                        )
                    },
                    |chunk| serde_json::to_string(&chunk).expect("serialize chunk"),
                );
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(body));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn binary_content_round_trips_byte_identical() {
        let (base_db_url, server, shutdown) = spawn_fake_chunk_store_couch();
        let db = couchdb_at_full(&base_db_url, "");

        // A deliberately non-UTF-8 buffer that `String::from_utf8_lossy` would
        // corrupt: contains invalid UTF-8 sequences and null bytes.
        let original: Vec<u8> = vec![
            0x00, 0xFF, 0xFE, 0x80, 0xC3, 0x28, 0xED, 0xA0, 0xBD, 0x00, 0x9B, 0x01, 0xE2, 0x28,
            0xA1,
        ];

        let chunk_ids = db
            .upload_file_content(original.clone())
            .await
            .expect("upload content");
        assert_eq!(
            chunk_ids.len(),
            1,
            "content should be stored as a single chunk"
        );

        let round_tripped = db
            .get_file_content("notes/binary.dat")
            .await
            .expect("download content");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert_eq!(
            round_tripped, original,
            "non-UTF-8 bytes must round-trip byte-identical through upload+download"
        );
    }

    /// A file larger than a single chunk is split into ordered chunks and
    /// reconstructs byte-for-byte with a matching hash (story 6357: a 9 MiB
    /// upload previously failed `413 document_too_large` as one oversized
    /// chunk).
    #[tokio::test]
    async fn multi_chunk_content_round_trips_byte_identical() {
        let (base_db_url, server, shutdown) = spawn_fake_chunk_store_couch();
        let db = couchdb_at_full(&base_db_url, "");

        // Deliberately above the single-chunk bound so it must split, and
        // non-UTF-8 so the split/reassembly must stay binary-safe.
        let original: Vec<u8> = (0..(MAX_CHUNK_DATA_BYTES + 1000))
            .map(|i| u8::try_from(i % 251).expect("i % 251 fits in u8"))
            .collect();
        assert!(
            original.len() > MAX_CHUNK_DATA_BYTES,
            "test content must exceed one chunk"
        );

        let chunk_ids = db
            .upload_file_content(original.clone())
            .await
            .expect("upload multi-chunk content");
        assert!(
            chunk_ids.len() >= 2,
            "content above the bound must split into multiple chunks, got {}",
            chunk_ids.len()
        );
        assert_eq!(
            chunk_ids.len(),
            original.len().div_ceil(MAX_CHUNK_DATA_BYTES),
            "chunk count must match the bounded split"
        );

        let round_tripped = db
            .get_file_content("notes/big.bin")
            .await
            .expect("download multi-chunk content");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert_eq!(
            round_tripped, original,
            "multi-chunk file must reconstruct byte-for-byte"
        );
        assert_eq!(
            crate::local::scanner::compute_bytes_hash(&round_tripped),
            crate::local::scanner::compute_bytes_hash(&original),
            "reconstructed bytes must hash identically"
        );
    }

    /// Chunk splitting happens exactly at the configured bound: a file exactly
    /// one chunk in size stays one chunk, one byte over becomes two, and a
    /// file of exactly two chunks stays two (no empty tail chunk).
    #[tokio::test]
    async fn chunk_splitting_respects_exact_boundary() {
        let expected = |len: usize| len.div_ceil(MAX_CHUNK_DATA_BYTES);

        for (len, want) in [
            (0usize, 0usize),
            (1, 1),
            (MAX_CHUNK_DATA_BYTES, 1),
            (MAX_CHUNK_DATA_BYTES + 1, 2),
            (2 * MAX_CHUNK_DATA_BYTES, 2),
        ] {
            let (base_db_url, server, shutdown) = spawn_fake_chunk_store_couch();
            let db = couchdb_at_full(&base_db_url, "");
            let chunk_ids = db
                .upload_file_content(vec![0xAB; len])
                .await
                .expect("upload content");
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
            server.join().expect("fake couch server join");

            assert_eq!(
                chunk_ids.len(),
                want,
                "len {len} should split into {want} chunk(s), expected {want} == {}",
                expected(len)
            );
        }
    }

    /// An empty file uploads as no chunks at all, so `get_file_content` (which
    /// maps an empty `children` list to empty content) returns the empty file.
    #[tokio::test]
    async fn empty_content_uploads_no_chunks_and_round_trips_empty() {
        let (base_db_url, server, shutdown) = spawn_fake_chunk_store_couch();
        let db = couchdb_at_full(&base_db_url, "");

        let chunk_ids = db
            .upload_file_content(Vec::new())
            .await
            .expect("upload empty content");
        assert!(
            chunk_ids.is_empty(),
            "empty content must not create any chunk"
        );

        let round_tripped = db
            .get_file_content("notes/empty.txt")
            .await
            .expect("download empty content");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert!(
            round_tripped.is_empty(),
            "empty file must round-trip as empty"
        );
    }

    /// An interrupted multi-chunk upload (a chunk save failing partway through)
    /// must propagate the error and never return a partial chunk set as if the
    /// upload succeeded. The already-saved leading chunks are left in place for
    /// orphan chunk collection, not referenced by any metadata document.
    #[tokio::test]
    async fn interrupted_multi_chunk_upload_propagates_error() {
        let (base_db_url, store, server, shutdown) = spawn_fake_failing_chunk_store_couch(1);
        let db = couchdb_at_full(&base_db_url, "");

        // Two chunks worth of content; the second chunk PUT fails.
        let content = vec![0xCDu8; 2 * MAX_CHUNK_DATA_BYTES];
        let result = db.upload_file_content(content).await;

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert!(
            result.is_err(),
            "interrupted upload must return an error, not a partial chunk set"
        );
        let err = format!("{}", result.expect_err("upload must fail"));
        assert!(
            err.contains("Failed to save chunk"),
            "error must surface the failing chunk save, got: {err}"
        );
        // Exactly the first chunk made it before the failure.
        assert_eq!(
            store.lock().map.len(),
            1,
            "only the leading chunk is written before the interrupted upload"
        );
    }

    /// Spawn an in-process fake `CouchDB` like [`spawn_fake_chunk_store_couch`],
    /// but fail the chunk PUT once more than `fail_after_puts` chunk documents
    /// have been accepted. Returns the base URL, the chunk store, the server
    /// thread handle, and a shutdown flag.
    fn spawn_fake_failing_chunk_store_couch(
        fail_after_puts: usize,
    ) -> (
        String,
        std::sync::Arc<parking_lot::Mutex<ChunkStore>>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind failing store couch");
        let addr = listener.local_addr().expect("failing store couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let store: std::sync::Arc<parking_lot::Mutex<ChunkStore>> =
            std::sync::Arc::new(parking_lot::Mutex::new(ChunkStore::default()));
        let store_for_thread = store.clone();
        let remaining_puts =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(fail_after_puts));
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failing store couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set failing store couch nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("failing store couch listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let store = store_for_thread.clone();
                            let remaining = remaining_puts.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let store = store.clone();
                                let remaining = remaining.clone();
                                async move {
                                    handle_fake_failing_chunk_store_req(req, store, remaining).await
                                }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, store, handle, shutdown)
    }

    /// Serve `PUT` chunk storage from [`spawn_fake_failing_chunk_store_couch`]:
    /// accept (store) up to the configured number of chunk PUTs, then fail
    /// subsequent ones with `500`. GETs return stored chunks so leading chunks
    /// written before the failure remain observable.
    #[allow(clippy::unused_async)]
    async fn handle_fake_failing_chunk_store_req(
        req: hyper::Request<hyper::body::Incoming>,
        store: std::sync::Arc<parking_lot::Mutex<ChunkStore>>,
        remaining_puts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        use std::sync::atomic::Ordering::SeqCst;
        let (parts, body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let id = decoded_last_segment(parts.uri.path());
        match (parts.method.clone(), id.as_str()) {
            (hyper::Method::PUT, id) if !id.is_empty() => {
                use http_body_util::BodyExt;
                // Always drain the request body first so the connection stays in
                // a reusable HTTP/1.1 state regardless of the response path.
                let bytes = body.collect().await.expect("collect chunk body").to_bytes();
                if remaining_puts
                    .try_update(SeqCst, SeqCst, |n| n.checked_sub(1))
                    .is_err()
                {
                    *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
                    *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                        r#"{"error":"internal_server_error","reason":"injected chunk save failure"}"#,
                    ));
                    return Ok(response);
                }
                let chunk: ChunkDoc = serde_json::from_slice(&bytes).expect("parse stored chunk");
                store.lock().map.insert(id.to_string(), chunk);
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(
                    r#"{"ok":true,"rev":"1-abc"}"#,
                ));
            }
            (hyper::Method::GET, id) if !id.is_empty() => {
                let chunk = store.lock().map.get(id).cloned();
                let body = chunk.map_or_else(
                    || r#"{"_id":"none","_rev":"1-abc","children":[],"path":"none","type":"plain","size":0}"#.to_string(),
                    |chunk| serde_json::to_string(&chunk).expect("serialize chunk"),
                );
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(body));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    // -----------------------------------------------------------------------
    // Orphan chunk collection (story 6352)
    // -----------------------------------------------------------------------

    /// Build a well-formed `plain` `FileDoc` row body for the `_all_docs`
    /// response used by `get_all_files`. A `deleted: true` doc models a
    /// soft-delete tombstone whose chunks are still recoverable.
    fn orphan_file_row(id: &str, children: &[&str], deleted: bool, mtime_ms: u64) -> String {
        let children_json = serde_json::to_string(children).expect("children json");
        format!(
            r#"{{"id":"{id}","key":null,"value":{{"rev":"1-{id}-rev"}},"doc":{{"_id":"{id}","_rev":"1-{id}-rev","path":"{id}","children":{children_json},"ctime":0,"mtime":{mtime_ms},"size":1,"type":"plain","deleted":{deleted},"deleted_at":{mtime_ms},"hash":""}}}}"#
        )
    }

    /// Build a chunk document projection (`id`, `rev`, `created_at`) as served
    /// by `_find`.
    fn orphan_chunk_json(id: &str, rev: &str, created_at_ms: u64) -> String {
        format!(r#"{{"_id":"{id}","_rev":"{rev}","created_at":{created_at_ms}}}"#)
    }

    /// Spawn a fake `CouchDB` server that serves the given file rows on
    /// `POST /_all_docs`, the given chunk docs on `POST /_find`, answers
    /// `POST /_index` with success (Mango index creation), and counts
    /// `DELETE` requests (orphan collection). Returns the base database URL,
    /// a delete counter, the server thread handle, and a shutdown flag.
    #[allow(clippy::type_complexity)]
    fn spawn_orphan_collection_couch(
        file_rows: &[String],
        chunk_docs: &[String],
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind orphan collection couch");
        let addr = listener.local_addr().expect("orphan collection couch addr");
        let base_db_url = format!("http://{addr}/test_db");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delete_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shutdown_thread = shutdown.clone();
        let delete_count_thread = delete_count.clone();
        let rows_json = std::sync::Arc::new(file_rows.join(","));
        let chunks_json = std::sync::Arc::new(chunk_docs.join(","));
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("orphan collection fake couch runtime");
            runtime.block_on(async move {
                listener
                    .set_nonblocking(true)
                    .expect("set orphan collection fake couch nonblocking");
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("orphan collection tokio listener");
                loop {
                    if shutdown_thread.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), listener.accept()).await {
                        Ok(Ok((stream, _))) => {
                            let delete_count_thread = delete_count_thread.clone();
                            let rows_json = rows_json.clone();
                            let chunks_json = chunks_json.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let delete_count_thread = delete_count_thread.clone();
                                let rows_json = rows_json.clone();
                                let chunks_json = chunks_json.clone();
                                async move {
                                    handle_orphan_collection_couch_req(
                                        req,
                                        rows_json,
                                        chunks_json,
                                        delete_count_thread,
                                    )
                                    .await
                                }
                            });
                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let _ = hyper::server::conn::http1::Builder::new()
                                    .serve_connection(io, service)
                                    .await;
                            });
                        }
                        Ok(Err(_)) => break,
                        Err(_elapsed) => {}
                    }
                }
            });
        });
        (base_db_url, delete_count, handle, shutdown)
    }

    /// Serve the endpoints used by `collect_orphan_chunks`: `_index`
    /// (index creation), `_find` (chunk enumeration), `_all_docs` (file
    /// documents for the referenced set) and `DELETE` (orphan hard-delete).
    #[allow(clippy::unused_async)]
    async fn handle_orphan_collection_couch_req(
        req: hyper::Request<hyper::body::Incoming>,
        rows_json: std::sync::Arc<String>,
        chunks_json: std::sync::Arc<String>,
        delete_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Result<hyper::Response<http_body_util::Full<hyper::body::Bytes>>, hyper::Error> {
        let (parts, _body) = req.into_parts();
        let mut response =
            hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::new()));
        let last_segment = parts
            .uri
            .path()
            .split('/')
            .next_back()
            .unwrap_or("")
            .to_string();
        match (parts.method.as_str(), last_segment.as_str()) {
            ("POST", "_index") => {
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from_static(
                    br#"{"ok":true,"result":"created"}"#,
                ));
            }
            ("POST", "_find") => {
                let body = format!(
                    r#"{{"docs":[{chunks}],"bookmark":"nil"}}"#,
                    chunks = chunks_json.as_str()
                );
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(body));
            }
            ("POST", "_all_docs") => {
                let body = format!(
                    r#"{{"total_rows":0,"offset":0,"rows":[{rows}]}}"#,
                    rows = rows_json.as_str()
                );
                *response.body_mut() = http_body_util::Full::new(hyper::body::Bytes::from(body));
            }
            ("DELETE", _) => {
                delete_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *response.body_mut() =
                    http_body_util::Full::new(hyper::body::Bytes::from_static(br#"{"ok":true}"#));
            }
            _ => {
                *response.status_mut() = hyper::StatusCode::NOT_FOUND;
            }
        }
        Ok(response)
    }

    #[tokio::test]
    async fn collect_orphan_chunks_reclaims_unreferenced_but_keeps_live_shared_and_recoverable() {
        // Live file references h:live1; a surviving tombstone (recoverable
        // until pruned) references h:rec1. h:orphan1/h:orphan2 are unreferenced
        // orphans left by a failed publication or pruned tombstone.
        let (base_db_url, delete_count, server, shutdown) = spawn_orphan_collection_couch(
            &[
                orphan_file_row("notes/a.md", &["h:live1"], false, 1000),
                orphan_file_row("notes/b.md", &["h:rec1"], true, 1000),
            ],
            &[
                orphan_chunk_json("h:live1", "1-a", 1000),
                orphan_chunk_json("h:rec1", "1-b", 1000),
                orphan_chunk_json("h:orphan1", "1-c", 1000),
                orphan_chunk_json("h:orphan2", "1-d", 1000),
            ],
        );
        let db = couchdb_at_ephemeral(&base_db_url);

        let result = db
            .collect_orphan_chunks(100, Duration::ZERO)
            .await
            .expect("collect orphan chunks");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert_eq!(result.scanned, 4, "all chunk docs should be examined");
        assert_eq!(
            result.collected, 2,
            "only the two unreferenced chunks should be reclaimed"
        );
        assert_eq!(result.deferred, 0, "nothing should be deferred here");
        assert_eq!(
            delete_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly the two orphan chunks should be hard-deleted"
        );
    }

    #[tokio::test]
    async fn collect_orphan_chunks_defers_chunks_within_in_flight_grace_period() {
        // h:fresh is unreferenced but was created just now: a concurrent client
        // may still be mid-publication, so it must be deferred, not collected.
        let now_ms = crate::models::TimestampMillis::now().as_u64();
        let (base_db_url, delete_count, server, shutdown) =
            spawn_orphan_collection_couch(&[], &[orphan_chunk_json("h:fresh", "1-e", now_ms)]);
        let db = couchdb_at_ephemeral(&base_db_url);

        let result = db
            .collect_orphan_chunks(100, std::time::Duration::from_hours(1))
            .await
            .expect("collect orphan chunks");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert_eq!(result.collected, 0, "in-flight chunk must not be collected");
        assert_eq!(result.deferred, 1, "in-flight chunk should be deferred");
        assert_eq!(
            delete_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no chunk should be hard-deleted while in the grace window"
        );
    }

    #[tokio::test]
    async fn collect_orphan_chunks_is_bounded_per_pass() {
        // Two orphans but a batch bound of 1: only one is collected this pass,
        // the other is deferred and reclaimed on a later (resumable) pass.
        let (base_db_url, delete_count, server, shutdown) = spawn_orphan_collection_couch(
            &[],
            &[
                orphan_chunk_json("h:orphan1", "1-f", 1000),
                orphan_chunk_json("h:orphan2", "1-g", 1000),
            ],
        );
        let db = couchdb_at_ephemeral(&base_db_url);

        let result = db
            .collect_orphan_chunks(1, Duration::ZERO)
            .await
            .expect("collect orphan chunks");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().expect("fake couch server join");

        assert_eq!(
            result.collected, 1,
            "collection must respect the per-pass bound"
        );
        assert_eq!(
            result.deferred, 1,
            "the remaining orphan is deferred for a later pass"
        );
        assert_eq!(
            delete_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one orphan should be hard-deleted this pass"
        );
    }
}
