use crate::models::file::{FileState, RemoteState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Conflict resolution strategies
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionStrategy {
    KeepLocal,
    KeepRemote,
    KeepBoth,
    Skip,
}

impl std::str::FromStr for ResolutionStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "keep-local" => Ok(ResolutionStrategy::KeepLocal),
            "keep-remote" => Ok(ResolutionStrategy::KeepRemote),
            "keep-both" => Ok(ResolutionStrategy::KeepBoth),
            "skip" => Ok(ResolutionStrategy::Skip),
            _ => Err(format!("Unknown resolution strategy: {}", s)),
        }
    }
}

impl std::fmt::Display for ResolutionStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolutionStrategy::KeepLocal => write!(f, "keep-local"),
            ResolutionStrategy::KeepRemote => write!(f, "keep-remote"),
            ResolutionStrategy::KeepBoth => write!(f, "keep-both"),
            ResolutionStrategy::Skip => write!(f, "skip"),
        }
    }
}

/// A conflict between local and remote state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub local_state: FileState,
    pub remote_state: RemoteState,
    pub detected_at: DateTime<Utc>,
    pub notified: bool,
}

impl Conflict {
    pub fn new(path: String, local_state: FileState, remote_state: RemoteState) -> Self {
        Self {
            path,
            local_state,
            remote_state,
            detected_at: Utc::now(),
            notified: false,
        }
    }

    pub fn mark_notified(&mut self) {
        self.notified = true;
    }
}

/// Status of a conflict resolution
#[derive(Debug, Clone)]
pub struct ConflictResolution {
    pub path: String,
    pub strategy: ResolutionStrategy,
    pub resolved_at: DateTime<Utc>,
}

/// Statistics about conflicts
#[derive(Debug, Clone, Default)]
pub struct ConflictStats {
    pub total: usize,
    pub notified: usize,
    pub unresolved: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Tests for ResolutionStrategy parsing and display
    // =========================================================================

    #[test]
    fn resolution_strategy_from_str_keep_local() {
        assert_eq!(
            "keep-local".parse::<ResolutionStrategy>().unwrap(),
            ResolutionStrategy::KeepLocal
        );
    }

    #[test]
    fn resolution_strategy_from_str_keep_remote() {
        assert_eq!(
            "keep-remote".parse::<ResolutionStrategy>().unwrap(),
            ResolutionStrategy::KeepRemote
        );
    }

    #[test]
    fn resolution_strategy_from_str_keep_both() {
        assert_eq!(
            "keep-both".parse::<ResolutionStrategy>().unwrap(),
            ResolutionStrategy::KeepBoth
        );
    }

    #[test]
    fn resolution_strategy_from_str_skip() {
        assert_eq!(
            "skip".parse::<ResolutionStrategy>().unwrap(),
            ResolutionStrategy::Skip
        );
    }

    #[test]
    fn resolution_strategy_from_str_invalid() {
        let result = "invalid".parse::<ResolutionStrategy>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown resolution strategy"));
    }

    #[test]
    fn resolution_strategy_display_keep_local() {
        assert_eq!(format!("{}", ResolutionStrategy::KeepLocal), "keep-local");
    }

    #[test]
    fn resolution_strategy_display_keep_remote() {
        assert_eq!(format!("{}", ResolutionStrategy::KeepRemote), "keep-remote");
    }

    #[test]
    fn resolution_strategy_display_keep_both() {
        assert_eq!(format!("{}", ResolutionStrategy::KeepBoth), "keep-both");
    }

    #[test]
    fn resolution_strategy_display_skip() {
        assert_eq!(format!("{}", ResolutionStrategy::Skip), "skip");
    }

    // =========================================================================
    // Tests for Conflict struct
    // =========================================================================

    #[test]
    fn conflict_new_sets_fields_correctly() {
        use crate::models::file::FileState;
        use chrono::Utc;

        let local_state =
            FileState::new("test.txt".to_string(), "hash1".to_string(), 100, Utc::now());
        let remote_state = RemoteState {
            path: "test.txt".to_string(),
            hash: "hash2".to_string(),
            size: 200,
            modified_at: Utc::now(),
            couch_rev: "1-abc".to_string(),
            deleted: false,
        };

        let conflict = Conflict::new("test.txt".to_string(), local_state, remote_state);

        assert_eq!(conflict.path, "test.txt");
        assert!(!conflict.notified);
        assert_eq!(conflict.local_state.size, 100);
        assert_eq!(conflict.remote_state.size, 200);
    }

    #[test]
    fn conflict_mark_notified_sets_flag() {
        use crate::models::file::FileState;
        use chrono::Utc;

        let local_state =
            FileState::new("test.txt".to_string(), "hash1".to_string(), 100, Utc::now());
        let remote_state = RemoteState {
            path: "test.txt".to_string(),
            hash: "hash2".to_string(),
            size: 200,
            modified_at: Utc::now(),
            couch_rev: "1-abc".to_string(),
            deleted: false,
        };

        let mut conflict = Conflict::new("test.txt".to_string(), local_state, remote_state);
        assert!(!conflict.notified);

        conflict.mark_notified();
        assert!(conflict.notified);
    }

    // =========================================================================
    // Tests for ConflictStats
    // =========================================================================

    #[test]
    fn conflict_stats_default_is_zero() {
        let stats: ConflictStats = Default::default();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.notified, 0);
        assert_eq!(stats.unresolved, 0);
    }
}
