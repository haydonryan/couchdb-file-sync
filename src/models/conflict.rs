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
