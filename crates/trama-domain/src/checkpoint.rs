use crate::ids::{CheckpointId, RepositoryId, ThreadId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkingChanges {
    Clean,
    Dirty,
    Unknown,
}

/// State of one repository inside a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub repository_id: RepositoryId,
    pub commit: Option<String>,
    pub tree: Option<String>,
    pub working_changes: WorkingChanges,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointLabel {
    Turn,
    Manual,
    ReviewVerdict,
}

/// A coordinated snapshot across all repositories of a thread.
/// Restoring creates a new thread derivation — history is never rewritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub thread_id: ThreadId,
    pub event_seq: u64,
    pub label: CheckpointLabel,
    pub repo_states: Vec<RepoState>,
    pub created_at: DateTime<Utc>,
}
