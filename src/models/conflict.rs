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
    use chrono::Utc;

    #[test]
    fn test_conflict_new() {
        let local_state = FileState::new(
            "/path/to/file.txt".into(),
            "hash1".into(),
            100,
            Utc::now(),
        );
        let remote_state = RemoteState {
            path: "/path/to/file.txt".into(),
            hash: "hash2".into(),
            size: 200,
            modified_at: Utc::now(),
            couch_rev: "1-abc".into(),
            deleted: false,
        };
        let conflict = Conflict::new("/path/to/file.txt".into(), local_state.clone(), remote_state.clone());
        assert_eq!(conflict.path, "/path/to/file.txt");
        assert!(!conflict.notified);
        // Verify the states are stored
        assert_eq!(conflict.local_state.path, local_state.path);
        assert_eq!(conflict.remote_state.path, remote_state.path);
    }

    #[test]
    fn test_conflict_mark_notified() {
        let local_state = FileState::new(
            "/path/to/file.txt".into(),
            "hash1".into(),
            100,
            Utc::now(),
        );
        let remote_state = RemoteState {
            path: "/path/to/file.txt".into(),
            hash: "hash2".into(),
            size: 200,
            modified_at: Utc::now(),
            couch_rev: "1-abc".into(),
            deleted: false,
        };
        let mut conflict = Conflict::new("/path/to/file.txt".into(), local_state, remote_state);
        assert!(!conflict.notified);
        conflict.mark_notified();
        assert!(conflict.notified);
    }

    #[test]
    fn test_conflict_detected_at_set() {
        let local_state = FileState::new(
            "/path/to/file.txt".into(),
            "hash1".into(),
            100,
            Utc::now(),
        );
        let remote_state = RemoteState {
            path: "/path/to/file.txt".into(),
            hash: "hash2".into(),
            size: 200,
            modified_at: Utc::now(),
            couch_rev: "1-abc".into(),
            deleted: false,
        };
        let conflict = Conflict::new("/path/to/file.txt".into(), local_state, remote_state);
        // detected_at should be set to approximately now
        let now = Utc::now();
        let diff = now - conflict.detected_at;
        assert!(diff.num_seconds() < 5, "detected_at should be recent");
    }

    #[test]
    fn test_resolution_strategy_from_str() {
        assert_eq!("keep-local".parse::<ResolutionStrategy>(), Ok(ResolutionStrategy::KeepLocal));
        assert_eq!("keep-remote".parse::<ResolutionStrategy>(), Ok(ResolutionStrategy::KeepRemote));
        assert_eq!("keep-both".parse::<ResolutionStrategy>(), Ok(ResolutionStrategy::KeepBoth));
        assert_eq!("skip".parse::<ResolutionStrategy>(), Ok(ResolutionStrategy::Skip));
    }

    #[test]
    fn test_resolution_strategy_from_str_invalid() {
        let result = "unknown".parse::<ResolutionStrategy>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown resolution strategy"));
    }

    #[test]
    fn test_resolution_strategy_display() {
        assert_eq!(format!("{}", ResolutionStrategy::KeepLocal), "keep-local");
        assert_eq!(format!("{}", ResolutionStrategy::KeepRemote), "keep-remote");
        assert_eq!(format!("{}", ResolutionStrategy::KeepBoth), "keep-both");
        assert_eq!(format!("{}", ResolutionStrategy::Skip), "skip");
    }

    #[test]
    fn test_resolution_strategy_round_trip() {
        let variants = [
            ResolutionStrategy::KeepLocal,
            ResolutionStrategy::KeepRemote,
            ResolutionStrategy::KeepBoth,
            ResolutionStrategy::Skip,
        ];
        for variant in &variants {
            let display = format!("{}", variant);
            let parsed: ResolutionStrategy = display.parse().unwrap();
            assert_eq!(*variant, parsed, "round-trip failed for {:?}", variant);
        }
    }

    #[test]
    fn test_conflict_resolution_struct() {
        let resolution = ConflictResolution {
            path: "/path/to/file.txt".into(),
            strategy: ResolutionStrategy::KeepLocal,
            resolved_at: Utc::now(),
        };
        assert_eq!(resolution.path, "/path/to/file.txt");
        assert_eq!(resolution.strategy, ResolutionStrategy::KeepLocal);
    }

    #[test]
    fn test_conflict_stats_default() {
        let stats = ConflictStats::default();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.notified, 0);
        assert_eq!(stats.unresolved, 0);
    }
}
