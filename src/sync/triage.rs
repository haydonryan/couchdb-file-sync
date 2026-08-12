use crate::models::{Change, ChangeType, FileState};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// Outcome of triaging a change pair
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageOutcome {
    /// Upload the local change to remote
    Upload,
    /// Download the remote change to local
    Download,
    /// Local and remote both changed; content must be compared to detect conflict
    NeedsComparison,
    /// No action needed
    Skip,
}

/// A single triage decision for a change path
#[derive(Debug, Clone)]
pub struct TriageDecision {
    /// The file path (local path)
    pub path: String,
    /// What action is needed
    pub outcome: TriageOutcome,
    /// The local change, if any
    pub local_change: Option<Change>,
    /// The remote change, if any
    pub remote_change: Option<Change>,
}

/// Result of triaging a full set of local and remote changes
#[derive(Debug, Clone, Default)]
pub struct TriageResult {
    /// Local changes that should be uploaded (remote unchanged or new)
    pub uploads: Vec<Change>,
    /// Remote changes that should be downloaded (local unchanged or new)
    pub downloads: Vec<Change>,
    /// Pairs where both local and remote changed; caller must compare
    /// actual content hashes to decide between conflict and silent sync
    pub needs_comparison: Vec<TriageDecision>,
    /// Remote deletions that should be applied locally
    pub remote_deletes: Vec<Change>,
    /// Skipped changes (already in sync, stale, etc.)
    pub skipped: Vec<TriageDecision>,
}

/// Convert a remote path to a local path by stripping the remote prefix.
#[must_use]
pub fn remote_path_to_local_path(remote_path: &str, remote_prefix: &str) -> String {
    if remote_prefix.is_empty() {
        remote_path.to_string()
    } else {
        remote_path
            .strip_prefix(remote_prefix)
            .unwrap_or(remote_path)
            .to_string()
    }
}

/// Check whether a stored state path was polluted by the remote prefix
/// (e.g., the local DB accidentally stored a remote-prefixed path).
#[must_use]
pub fn is_polluted_state_path(path: &str, remote_prefix: &str) -> bool {
    let remote_prefix = remote_prefix.trim_end_matches('/');
    !remote_prefix.is_empty()
        && (path == remote_prefix || path.starts_with(&format!("{remote_prefix}/")))
}

/// Determine whether a remote delete should be applied locally.
#[must_use]
pub fn should_apply_remote_delete(
    stored_state: Option<&FileState>,
    remote_mtime: Option<DateTime<Utc>>,
    _file_exists: bool,
) -> bool {
    stored_state.is_some_and(|state| {
        remote_mtime.is_none_or(|remote_mtime| remote_mtime > state.last_sync_at)
    })
}

/// Check whether the remote has changed compared to a stored local state.
/// Returns `true` if the remote is newer than the last sync.
#[must_use]
pub fn remote_is_newer(
    remote_mtime: Option<DateTime<Utc>>,
    stored_state: Option<&FileState>,
) -> bool {
    match (remote_mtime, stored_state) {
        (Some(remote_mtime), Some(state)) => remote_mtime > state.last_sync_at,
        // no remote mtime — assume changed; no stored state — first sync
        (None, _) | (_, None) => true,
    }
}

/// Check whether the remote revision differs from the stored revision.
#[must_use]
pub fn remote_revision_changed(remote_rev: Option<&str>, stored_rev: Option<&str>) -> bool {
    match (remote_rev, stored_rev) {
        (Some(remote_rev), Some(stored_rev)) => remote_rev != stored_rev,
        (Some(_), None) => true, // no stored rev — new file
        // no remote rev or no revs at all — skip
        (None, Some(_) | None) => false,
    }
}

/// Decide, in live mode, whether an incoming remote change should be applied
/// to the local filesystem (`true`) or the local change should be uploaded
/// instead (`false`) to arbitrate a concurrent edit.
///
/// The remote side is authoritative: the `CouchDB` revision, not the remote
/// mtime, tells whether the remote changed since our last sync. A
/// stale/preserved remote mtime (`cp -p`, `rsync -t`, `touch -t`) can no
/// longer mask a genuine remote edit as "older than local". The local side
/// compares the on-disk mtime against the stored sync-time mtime — a
/// same-machine comparison immune to cross-host clock skew.
///
/// * `remote_rev` — revision of the incoming remote change
/// * `stored_state` — last tracked local state (its `couch_rev` and
///   `modified_at` record the last sync)
/// * `local_exists` — whether the local file exists on disk
/// * `local_mtime` — the current on-disk local mtime
///
/// Returns `true` (download the remote change) when there is nothing local to
/// preserve or the local file is unchanged since the last sync. Returns
/// `false` (upload the local change) when the local file changed since the
/// last sync and must be arbitrated against the remote edit.
#[must_use]
pub fn live_should_apply_remote(
    remote_rev: Option<&str>,
    stored_state: Option<&FileState>,
    local_exists: bool,
    local_mtime: DateTime<Utc>,
) -> bool {
    if !local_exists {
        // Nothing local to preserve — the remote change is authoritative.
        return true;
    }
    let Some(state) = stored_state else {
        // An existing local file with no sync record cannot be proven
        // unchanged; preserve it by uploading rather than overwriting.
        return false;
    };
    if !remote_revision_changed(remote_rev, state.couch_rev.as_deref()) {
        // Remote revision still matches what we last stored — our own echo
        // (or an unchanged remote). Nothing new to download.
        return false;
    }
    // Remote changed since our last sync. If the local file is unchanged since
    // that sync, the remote edit is the only change → download. If the local
    // file also changed → both sides changed → upload to arbitrate (surfacing
    // the conflict on the remote).
    local_mtime == state.modified_at
}

/// Triage local and remote changes to determine what actions are needed.
///
/// This is a pure function with no I/O or `CouchDB` dependencies. The caller is
/// responsible for:
/// - Computing content hashes for `needs_comparison` pairs to decide between
///   conflict and silent state update.
/// - Executing the actual uploads, downloads, and deletions.
///
/// # Arguments
///
/// * `local_changes` - Changes detected on the local filesystem
/// * `remote_changes` - Changes fetched from the remote (`CouchDB`)
/// * `stored_states` - Map from local path to last-known file state
/// * `remote_prefix` - Prefix used to convert between local and remote paths
///
/// # Returns
///
/// A `TriageResult` categorising every change into uploads, downloads,
/// comparison-needed pairs, remote deletes, and skips.
#[must_use]
pub fn triage_changes<S: std::hash::BuildHasher>(
    local_changes: &[Change],
    remote_changes: &[Change],
    stored_states: &HashMap<String, FileState, S>,
    remote_prefix: &str,
) -> TriageResult {
    // Build lookup maps
    let local_map: HashMap<&str, &Change> = local_changes.iter().map(|c| (c.path(), c)).collect();
    let remote_map: HashMap<&str, &Change> = remote_changes.iter().map(|c| (c.path(), c)).collect();

    let mut result = TriageResult::default();

    // ── Process local changes ──────────────────────────────────────────
    for lc in local_changes {
        let remote_path = if remote_prefix.is_empty() {
            lc.path().to_string()
        } else {
            format!("{}{}", remote_prefix, lc.path())
        };

        // Local delete → always upload (no content comparison needed)
        if lc.change_type() == ChangeType::Deleted {
            result.uploads.push(lc.clone());
            continue;
        }

        let stored_state = stored_states.get(lc.path());

        // Check if the remote side also changed for this path, performing a
        // single remote_map lookup and reusing the borrow for needs_comparison.
        match remote_map.get(remote_path.as_str()) {
            Some(rc) => {
                // Both sides changed for this path. Decide whether the remote
                // actually differs from what we last stored using the
                // authoritative CouchDB revision, so a stale/preserved remote
                // mtime can no longer mask a real remote edit as "unchanged".
                // Fall back to the mtime heuristic only when no remote
                // revision is available.
                let remote_side_changed = rc.rev().map_or_else(
                    || remote_is_newer(rc.mtime().copied(), stored_state),
                    |remote_rev| {
                        remote_revision_changed(
                            Some(remote_rev),
                            stored_state.and_then(|s| s.couch_rev.as_deref()),
                        )
                    },
                );
                if remote_side_changed {
                    // Both sides changed — the caller must compare content hashes
                    result.needs_comparison.push(TriageDecision {
                        path: lc.path().to_string(),
                        outcome: TriageOutcome::NeedsComparison,
                        local_change: Some(lc.clone()),
                        remote_change: Some((*rc).clone()),
                    });
                } else {
                    // Remote unchanged → upload local change
                    result.uploads.push(lc.clone());
                }
            }
            // Remote absent → upload local change
            None => result.uploads.push(lc.clone()),
        }
    }

    // ── Process remote changes not in local changes ────────────────────
    for rc in remote_changes {
        let local_path = remote_path_to_local_path(rc.path(), remote_prefix);

        // Skip if also in local changes (handled above)
        if local_map.contains_key(local_path.as_str()) {
            continue;
        }

        let stored_state = stored_states.get(&local_path);

        if rc.change_type() == ChangeType::Deleted {
            // Remote delete — check if it should be applied locally
            let relative_path = local_path.trim_start_matches('/');
            let file_path = std::path::Path::new(relative_path);
            if should_apply_remote_delete(stored_state, rc.mtime().copied(), file_path.exists()) {
                result.remote_deletes.push(rc.clone());
            } else {
                result.skipped.push(TriageDecision {
                    path: local_path,
                    outcome: TriageOutcome::Skip,
                    local_change: None,
                    remote_change: Some(rc.clone()),
                });
            }
            continue;
        }

        // Remote change not in local changes — check if it should be downloaded
        let should_download = stored_state.map_or_else(
            || {
                // No local state — check if file exists on disk
                let relative_path = local_path.trim_start_matches('/');
                let file_path = std::path::Path::new(relative_path);
                if file_path.exists() {
                    // File exists but not tracked — skip
                    false
                } else {
                    // New file on remote — download
                    true
                }
            },
            |state| {
                let remote_rev = rc.rev();
                let stored_rev = state.couch_rev.as_deref();
                remote_revision_changed(remote_rev, stored_rev)
            },
        );

        if should_download {
            result.downloads.push(rc.clone());
        } else {
            result.skipped.push(TriageDecision {
                path: local_path,
                outcome: TriageOutcome::Skip,
                local_change: None,
                remote_change: Some(rc.clone()),
            });
        }
    }

    result
}

/// Plan a remote-rebuild operation: upload all local files and delete remote
/// files that have no corresponding local file.
#[must_use]
pub fn plan_remote_rebuild(
    local_states: &[FileState],
    remote_docs: &[crate::models::FileDoc],
    remote_prefix: &str,
) -> (Vec<String>, Vec<String>) {
    use std::collections::HashSet;
    let local_paths: HashSet<&str> = local_states
        .iter()
        .map(|state| state.path.as_str())
        .collect();
    let uploads = local_states
        .iter()
        .map(|state| state.path.clone())
        .collect::<Vec<_>>();
    let remote_deletes = remote_docs
        .iter()
        .filter(|doc| !doc.deleted)
        .filter_map(|doc| {
            let local_path = remote_path_to_local_path(&doc.id, remote_prefix);
            (!local_paths.contains(local_path.as_str())).then(|| doc.id.clone())
        })
        .collect::<Vec<_>>();

    (uploads, remote_deletes)
}

/// Plan a local-rebuild operation: delete all local files and download all
/// live remote files.
#[must_use]
pub fn plan_local_rebuild(
    local_states: &[FileState],
    remote_docs: &[crate::models::FileDoc],
) -> (Vec<String>, Vec<String>) {
    let local_deletes = local_states
        .iter()
        .map(|state| state.path.clone())
        .collect::<Vec<_>>();
    let remote_downloads = remote_docs
        .iter()
        .filter(|doc| !doc.deleted)
        .map(|doc| doc.id.clone())
        .collect::<Vec<_>>();

    (local_deletes, remote_downloads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::file::CouchRev;

    use chrono::{Duration, NaiveDateTime, Utc};

    // ── Helper helpers ──────────────────────────────────────────────

    fn make_state(path: &str, last_sync_at: DateTime<Utc>) -> FileState {
        FileState {
            path: path.to_string(),
            hash: "abc".to_string(),
            size: 1,
            modified_at: last_sync_at,
            couch_rev: Some(CouchRev::new("1-abc").unwrap()),
            last_sync_at,
        }
    }

    fn local_change(path: &str, change_type: ChangeType) -> Change {
        match change_type {
            ChangeType::Created => {
                Change::local_created(path.to_string(), "hash-local".to_string(), 100)
            }
            ChangeType::Modified => {
                Change::local_modified(path.to_string(), "hash-local".to_string(), 100)
            }
            ChangeType::Deleted => Change::local_deleted(path.to_string()),
        }
    }

    fn remote_change(
        path: &str,
        change_type: ChangeType,
        mtime: Option<DateTime<Utc>>,
        rev: Option<&str>,
    ) -> Change {
        match change_type {
            ChangeType::Created => Change::remote_created(
                path.to_string(),
                "hash-remote".to_string(),
                200,
                mtime.unwrap_or_else(Utc::now),
                rev.unwrap_or("").to_string(),
            ),
            ChangeType::Modified => Change::remote_modified(
                path.to_string(),
                "hash-remote".to_string(),
                200,
                mtime.unwrap_or_else(Utc::now),
                rev.unwrap_or("").to_string(),
            ),
            ChangeType::Deleted => Change::remote_deleted(path.to_string(), mtime),
        }
    }

    fn utc(ymd: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(ymd, "%Y-%m-%d %H:%M:%S")
            .map(|d| d.and_utc())
            .unwrap()
    }

    // ── is_polluted_state_path ──────────────────────────────────────

    #[test]
    fn detects_state_entries_polluted_with_remote_prefix() {
        assert!(is_polluted_state_path(
            "Agents/ross-coulthart/AGENT.md",
            "Agents/"
        ));
        assert!(is_polluted_state_path("Agents", "Agents/"));
        assert!(!is_polluted_state_path(
            "ross-coulthart/AGENT.md",
            "Agents/"
        ));
        assert!(!is_polluted_state_path("Agentsmith/profile.md", "Agents/"));
    }

    // ── should_apply_remote_delete ──────────────────────────────────

    #[test]
    fn stale_remote_delete_does_not_remove_tracked_local_file() {
        let last_sync = utc("2026-03-23 17:12:33");
        let remote_delete = last_sync - Duration::days(6);

        assert!(!should_apply_remote_delete(
            Some(&make_state("f", last_sync)),
            Some(remote_delete),
            true,
        ));
    }

    #[test]
    fn newer_remote_delete_removes_tracked_local_file() {
        let last_sync = utc("2026-03-23 17:12:33");
        let remote_delete = last_sync + Duration::seconds(1);

        assert!(should_apply_remote_delete(
            Some(&make_state("f", last_sync)),
            Some(remote_delete),
            true,
        ));
    }

    #[test]
    fn untracked_existing_local_file_is_not_deleted() {
        let remote_delete = utc("2026-03-23 17:12:33");
        assert!(!should_apply_remote_delete(None, Some(remote_delete), true));
    }

    // ── remote_is_newer ─────────────────────────────────────────────

    #[test]
    fn remote_is_newer_when_mtime_after_last_sync() {
        let last_sync = utc("2026-07-28 10:00:00");
        let remote_mtime = utc("2026-07-28 12:00:00");
        assert!(remote_is_newer(
            Some(remote_mtime),
            Some(&make_state("f", last_sync)),
        ));
    }

    #[test]
    fn remote_not_newer_when_mtime_before_last_sync() {
        let last_sync = utc("2026-07-28 12:00:00");
        let remote_mtime = utc("2026-07-28 10:00:00");
        assert!(!remote_is_newer(
            Some(remote_mtime),
            Some(&make_state("f", last_sync)),
        ));
    }

    #[test]
    fn remote_is_newer_when_no_mtime_available() {
        let last_sync = utc("2026-07-28 10:00:00");
        assert!(remote_is_newer(None, Some(&make_state("f", last_sync))));
    }

    #[test]
    fn remote_is_newer_when_no_stored_state() {
        let remote_mtime = utc("2026-07-28 12:00:00");
        assert!(remote_is_newer(Some(remote_mtime), None));
    }

    // ── remote_revision_changed ─────────────────────────────────────

    #[test]
    fn revision_changed_when_different() {
        assert!(remote_revision_changed(Some("2-def"), Some("1-abc")));
    }

    #[test]
    fn revision_unchanged_when_same() {
        assert!(!remote_revision_changed(Some("1-abc"), Some("1-abc")));
    }

    #[test]
    fn revision_changed_when_stored_rev_missing() {
        assert!(remote_revision_changed(Some("1-abc"), None));
    }

    #[test]
    fn revision_not_changed_when_remote_rev_missing() {
        assert!(!remote_revision_changed(None, Some("1-abc")));
    }

    #[test]
    fn revision_not_changed_when_both_missing() {
        assert!(!remote_revision_changed(None, None));
    }

    // ── live_should_apply_remote ────────────────────────────────────

    #[test]
    fn live_remote_change_with_stale_mtime_downloads_when_local_unchanged() {
        // The remote edit advances the CouchDB revision, but its mtime is
        // stale/older than the local mtime — e.g. preserved via cp -p,
        // rsync -t, git checkout, or touch -t. The old mtime heuristic would
        // treat local as newer and wrongly upload, clobbering the remote edit.
        // Revision-based arbitration downloads because the local file is
        // unchanged since the last sync.
        let last_sync = utc("2026-07-28 10:00:00");
        let state = make_state("f.txt", last_sync);
        // Local mtime is identical to the stored sync-time mtime (same machine,
        // immune to clock skew).
        let local_mtime = state.modified_at;

        assert!(live_should_apply_remote(
            Some("2-def"), // remote rev advanced past stored "1-abc"
            Some(&state),
            true,
            local_mtime,
        ));
    }

    #[test]
    fn live_remote_change_uploads_when_local_changed_since_sync() {
        // Both sides changed: remote rev advanced AND the local file was edited
        // since the last sync → upload local to arbitrate the concurrent edit.
        let last_sync = utc("2026-07-28 10:00:00");
        let state = make_state("f.txt", last_sync);
        let local_mtime = state.modified_at + Duration::seconds(5);

        assert!(!live_should_apply_remote(
            Some("2-def"),
            Some(&state),
            true,
            local_mtime,
        ));
    }

    #[test]
    fn live_remote_change_downloads_when_local_file_absent() {
        // No local file to preserve — the remote change is authoritative.
        let last_sync = utc("2026-07-28 10:00:00");
        let state = make_state("f.txt", last_sync);

        assert!(live_should_apply_remote(
            Some("2-def"),
            Some(&state),
            false,
            state.modified_at,
        ));
    }

    #[test]
    fn live_remote_echo_with_matching_rev_does_not_download() {
        // Remote revision still equals the stored revision — our own echo —
        // so there is nothing new to download (the live handler also skips
        // these up front).
        let last_sync = utc("2026-07-28 10:00:00");
        let state = make_state("f.txt", last_sync);

        assert!(!live_should_apply_remote(
            Some("1-abc"),
            Some(&state),
            true,
            state.modified_at,
        ));
    }

    #[test]
    fn live_remote_change_preserves_untracked_local_file() {
        // No stored state for an existing local file: it cannot be proven
        // unchanged, so it is preserved by uploading rather than overwritten.
        let last_sync = utc("2026-07-28 10:00:00");

        assert!(!live_should_apply_remote(
            Some("2-def"),
            None,
            true,
            last_sync
        ));
    }

    // ── triage_changes: local deletes ───────────────────────────────

    #[test]
    fn local_delete_always_uploads() {
        let local = vec![local_change("f.txt", ChangeType::Deleted)];
        let result = triage_changes(&local, &[], &HashMap::new(), "");

        assert_eq!(result.uploads.len(), 1);
        assert_eq!(result.uploads[0].path(), "f.txt");
        assert!(result.needs_comparison.is_empty());
        assert!(result.downloads.is_empty());
    }

    // ── triage_changes: upload when remote unchanged ────────────────

    #[test]
    fn local_created_uploads_when_no_remote() {
        let local = vec![local_change("f.txt", ChangeType::Created)];
        let result = triage_changes(&local, &[], &HashMap::new(), "");

        assert_eq!(result.uploads.len(), 1);
        assert_eq!(result.uploads[0].path(), "f.txt");
    }

    #[test]
    fn local_modified_uploads_when_remote_unchanged() {
        let last_sync = utc("2026-07-28 10:00:00");
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Modified,
            Some(last_sync - Duration::hours(1)), // mtime before last sync
            Some("1-abc"),
        )];
        let mut states = HashMap::new();
        states.insert("f.txt".to_string(), make_state("f.txt", last_sync));

        let local = vec![local_change("f.txt", ChangeType::Modified)];
        let result = triage_changes(&local, &remote, &states, "");

        assert_eq!(result.uploads.len(), 1);
    }

    // ── triage_changes: needs comparison when both changed ──────────

    #[test]
    fn both_changed_needs_comparison() {
        let last_sync = utc("2026-07-28 10:00:00");
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Modified,
            Some(last_sync + Duration::hours(1)), // mtime after last sync
            Some("2-def"),
        )];
        let mut states = HashMap::new();
        states.insert("f.txt".to_string(), make_state("f.txt", last_sync));

        let local = vec![local_change("f.txt", ChangeType::Modified)];
        let result = triage_changes(&local, &remote, &states, "");

        assert_eq!(result.needs_comparison.len(), 1);
        assert_eq!(result.needs_comparison[0].path, "f.txt");
        assert_eq!(
            result.needs_comparison[0].outcome,
            TriageOutcome::NeedsComparison
        );
        assert!(result.uploads.is_empty());
    }

    // ── triage_changes: downloads when remote not in local ──────────

    #[test]
    fn stale_mtime_concurrent_edit_needs_comparison() {
        // Finding #1: a remote edit carrying a stale mtime (cp -p, rsync -t,
        // git checkout, touch -t) must still be surfaced as a concurrent
        // change via the authoritative CouchDB revision, not silently
        // overwritten by a local upload.
        let last_sync = utc("2026-07-28 10:00:00");
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Modified,
            Some(last_sync - Duration::hours(1)), // stale mtime BEFORE last sync
            Some("2-def"),                        // ...but the revision advanced
        )];
        let mut states = HashMap::new();
        states.insert("f.txt".to_string(), make_state("f.txt", last_sync));

        let local = vec![local_change("f.txt", ChangeType::Modified)];
        let result = triage_changes(&local, &remote, &states, "");

        assert_eq!(result.needs_comparison.len(), 1);
        assert_eq!(result.needs_comparison[0].path, "f.txt");
        assert_eq!(
            result.needs_comparison[0].outcome,
            TriageOutcome::NeedsComparison
        );
        assert!(result.uploads.is_empty());
    }

    #[test]
    fn same_revision_remote_uploads_even_with_stale_mtime() {
        // Guard: when the remote revision is unchanged from what we stored, a
        // stale mtime must not turn it into a false conflict — we still upload.
        let last_sync = utc("2026-07-28 10:00:00");
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Modified,
            Some(last_sync - Duration::hours(1)), // stale mtime
            Some("1-abc"),                        // same revision
        )];
        let mut states = HashMap::new();
        states.insert("f.txt".to_string(), make_state("f.txt", last_sync));

        let local = vec![local_change("f.txt", ChangeType::Modified)];
        let result = triage_changes(&local, &remote, &states, "");

        assert_eq!(result.uploads.len(), 1);
        assert!(result.needs_comparison.is_empty());
    }

    #[test]
    fn remote_new_file_downloads_when_not_tracked() {
        let remote = vec![remote_change(
            "remote/f.txt",
            ChangeType::Created,
            Some(utc("2026-07-28 12:00:00")),
            Some("1-abc"),
        )];
        let result = triage_changes(&[], &remote, &HashMap::new(), "remote/");

        assert_eq!(result.downloads.len(), 1);
        assert_eq!(result.downloads[0].path(), "remote/f.txt");
    }

    #[test]
    fn remote_modified_downloads_when_revision_changed() {
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Modified,
            Some(utc("2026-07-28 12:00:00")),
            Some("2-def"),
        )];
        let mut states = HashMap::new();
        let mut state = make_state("f.txt", utc("2026-07-28 10:00:00"));
        state.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        states.insert("f.txt".to_string(), state);

        let result = triage_changes(&[], &remote, &states, "");

        assert_eq!(result.downloads.len(), 1);
    }

    #[test]
    fn remote_unchanged_skips_when_revision_same() {
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Modified,
            Some(utc("2026-07-28 12:00:00")),
            Some("1-abc"),
        )];
        let mut states = HashMap::new();
        let mut state = make_state("f.txt", utc("2026-07-28 10:00:00"));
        state.couch_rev = Some(CouchRev::new("1-abc").unwrap());
        states.insert("f.txt".to_string(), state);

        let result = triage_changes(&[], &remote, &states, "");

        assert_eq!(result.downloads.len(), 0);
        assert_eq!(result.skipped.len(), 1);
    }

    // ── triage_changes: remote delete ───────────────────────────────

    #[test]
    fn remote_delete_applied_when_newer_than_last_sync() {
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Deleted,
            Some(utc("2026-07-28 12:00:00")),
            None,
        )];
        let mut states = HashMap::new();
        states.insert(
            "f.txt".to_string(),
            make_state("f.txt", utc("2026-07-28 10:00:00")),
        );

        let result = triage_changes(&[], &remote, &states, "");

        assert_eq!(result.remote_deletes.len(), 1);
    }

    #[test]
    fn remote_delete_skipped_when_stale() {
        let remote = vec![remote_change(
            "f.txt",
            ChangeType::Deleted,
            Some(utc("2026-07-28 08:00:00")),
            None,
        )];
        let mut states = HashMap::new();
        states.insert(
            "f.txt".to_string(),
            make_state("f.txt", utc("2026-07-28 10:00:00")),
        );

        let result = triage_changes(&[], &remote, &states, "");

        assert_eq!(result.remote_deletes.len(), 0);
        assert_eq!(result.skipped.len(), 1);
    }

    // ── triage_changes: remote prefix handling ──────────────────────

    #[test]
    fn remote_prefix_is_stripped_when_mapping_remote_paths() {
        assert_eq!(
            remote_path_to_local_path("Agents/ross-coulthart/AGENT.md", "Agents/"),
            "ross-coulthart/AGENT.md"
        );
        assert_eq!(
            remote_path_to_local_path("notes/test.md", ""),
            "notes/test.md"
        );
    }

    #[test]
    fn triage_with_remote_prefix_matches_correctly() {
        // Local file at "doc.txt", remote stores it as "prefix/doc.txt"
        let local = vec![local_change("doc.txt", ChangeType::Modified)];
        let remote = vec![remote_change(
            "prefix/doc.txt",
            ChangeType::Modified,
            Some(utc("2026-07-28 08:00:00")), // before last sync
            Some("1-abc"),
        )];
        let mut states = HashMap::new();
        states.insert(
            "doc.txt".to_string(),
            make_state("doc.txt", utc("2026-07-28 10:00:00")),
        );

        let result = triage_changes(&local, &remote, &states, "prefix/");

        // Remote mtime is before last sync → upload local
        assert_eq!(result.uploads.len(), 1);
        assert_eq!(result.uploads[0].path(), "doc.txt");
    }

    // ── plan_remote_rebuild ─────────────────────────────────────────

    #[test]
    fn remote_rebuild_uploads_local_files_and_deletes_remote_orphans() {
        use crate::models::FileDoc;
        let local_states = vec![
            FileState::new(
                "notes/a.md".to_string(),
                "hash-a".to_string(),
                10,
                Utc::now(),
            ),
            FileState::new(
                "notes/b.md".to_string(),
                "hash-b".to_string(),
                20,
                Utc::now(),
            ),
        ];
        let remote_docs = vec![
            FileDoc::new("mirror/notes/a.md".to_string(), String::new(), 10),
            FileDoc::new("mirror/notes/orphan.md".to_string(), String::new(), 30),
            FileDoc {
                deleted: true,
                ..FileDoc::new("mirror/notes/deleted.md".to_string(), String::new(), 0)
            },
        ];

        let (uploads, remote_deletes) = plan_remote_rebuild(&local_states, &remote_docs, "mirror/");

        assert_eq!(
            uploads,
            vec!["notes/a.md".to_string(), "notes/b.md".to_string()]
        );
        assert_eq!(remote_deletes, vec!["mirror/notes/orphan.md".to_string()]);
    }

    // ── plan_local_rebuild ──────────────────────────────────────────

    #[test]
    fn local_rebuild_deletes_local_files_and_downloads_live_remote_docs() {
        use crate::models::FileDoc;
        let local_states = vec![
            FileState::new(
                "notes/old.md".to_string(),
                "hash-old".to_string(),
                10,
                Utc::now(),
            ),
            FileState::new(
                "notes/extra.md".to_string(),
                "hash-extra".to_string(),
                20,
                Utc::now(),
            ),
        ];
        let remote_docs = vec![
            FileDoc::new("mirror/notes/new.md".to_string(), String::new(), 15),
            FileDoc {
                deleted: true,
                ..FileDoc::new("mirror/notes/deleted.md".to_string(), String::new(), 0)
            },
        ];

        let (local_deletes, remote_downloads) = plan_local_rebuild(&local_states, &remote_docs);

        assert_eq!(
            local_deletes,
            vec!["notes/old.md".to_string(), "notes/extra.md".to_string()]
        );
        assert_eq!(remote_downloads, vec!["mirror/notes/new.md".to_string()]);
    }

    // ── Integration-style: mixed changes ────────────────────────────

    #[test]
    fn triage_mixed_scenario() {
        // Simulate multiple changes of different types
        let last_sync = utc("2026-07-28 10:00:00");

        let local = vec![
            // Local delete → upload
            local_change("delete_me.txt", ChangeType::Deleted),
            // Local new file → upload
            local_change("new_file.txt", ChangeType::Created),
            // Both changed → needs comparison
            local_change("both_changed.txt", ChangeType::Modified),
        ];

        let remote = vec![
            remote_change(
                "both_changed.txt",
                ChangeType::Modified,
                Some(last_sync + Duration::hours(1)),
                Some("2-def"),
            ),
            // Remote new file → download
            remote_change(
                "remote_new.txt",
                ChangeType::Created,
                Some(utc("2026-07-28 12:00:00")),
                Some("1-abc"),
            ),
            // Remote delete → apply
            remote_change(
                "remote_delete.txt",
                ChangeType::Deleted,
                Some(last_sync + Duration::hours(1)),
                None,
            ),
        ];

        let mut states = HashMap::new();
        states.insert(
            "delete_me.txt".to_string(),
            make_state("delete_me.txt", last_sync),
        );
        states.insert(
            "both_changed.txt".to_string(),
            make_state("both_changed.txt", last_sync),
        );
        states.insert(
            "remote_delete.txt".to_string(),
            make_state("remote_delete.txt", last_sync),
        );

        let result = triage_changes(&local, &remote, &states, "");

        // Local delete queued for upload
        assert_eq!(result.uploads.len(), 2);
        let upload_paths: Vec<&str> = result
            .uploads
            .iter()
            .map(crate::models::change::Change::path)
            .collect();
        assert!(upload_paths.contains(&"delete_me.txt"));
        assert!(upload_paths.contains(&"new_file.txt"));

        // Both-changed flagged for comparison
        assert_eq!(result.needs_comparison.len(), 1);
        assert_eq!(result.needs_comparison[0].path, "both_changed.txt");

        // Remote new file queued for download
        assert_eq!(result.downloads.len(), 1);
        assert_eq!(result.downloads[0].path(), "remote_new.txt");

        // Remote delete applied
        assert_eq!(result.remote_deletes.len(), 1);
        assert_eq!(result.remote_deletes[0].path(), "remote_delete.txt");
    }
}
