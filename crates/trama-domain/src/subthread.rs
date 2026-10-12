//! Value types for subthreads: fork points, changes, pull results, review
//! verdicts, and land results. Shared by the event log, infra and IPC.

use crate::ids::RepositoryId;
use serde::{Deserialize, Serialize};

/// Where a subthread's copy of one repository started. `base_revision` is the
/// parent's `HEAD` at fork time; `fork_commit` is a snapshot commit (in the
/// subthread's checkout) of the parent's full working state, including
/// uncommitted and untracked non-ignored files. A subthread's own changes are
/// measured from `fork_commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoForkPoint {
    pub repository_id: RepositoryId,
    pub base_revision: String,
    pub fork_commit: String,
}

/// The reviewer's decision on its parent's changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges { summary: String },
}

/// What a land subthread reported in its final message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandOutcome {
    Landed,
    Failed,
    /// No land skill existed; the agent proposed one instead of landing.
    Proposed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unknown,
}

/// Per-file stats of a diff. `additions`/`deletions` are `None` for binary
/// files. `old_path` is set for renames and copies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileChangeStatus,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

/// Result of pulling one file from a subthread into its parent's checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PullFileOutcome {
    /// The parent still had the fork-point version; the subthread's version
    /// was written.
    Applied,
    /// Both sides changed; a clean three-way merge was written.
    Merged,
    /// The parent already had the subthread's version.
    Unchanged,
    /// Both sides changed incompatibly. With `markers_written`, the file now
    /// contains conflict markers; otherwise the parent's file was left as is.
    Conflict {
        markers_written: bool,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullFileResult {
    pub path: String,
    /// What the subthread did to this file relative to its fork point.
    pub change: FileChangeStatus,
    #[serde(flatten)]
    pub outcome: PullFileOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPullResult {
    pub repository_id: RepositoryId,
    pub files: Vec<PullFileResult>,
}

impl RepoPullResult {
    pub fn conflict_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| matches!(file.outcome, PullFileOutcome::Conflict { .. }))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_and_pull_json_shapes_are_stable() {
        assert_eq!(
            serde_json::to_string(&ReviewVerdict::Approve).unwrap(),
            r#"{"verdict":"approve"}"#
        );
        assert_eq!(
            serde_json::to_string(&ReviewVerdict::RequestChanges {
                summary: "fix tests".into()
            })
            .unwrap(),
            r#"{"verdict":"request_changes","summary":"fix tests"}"#
        );
        let file = PullFileResult {
            path: "a.txt".into(),
            change: FileChangeStatus::Modified,
            outcome: PullFileOutcome::Conflict {
                markers_written: true,
                reason: "both changed".into(),
            },
        };
        let json = serde_json::to_string(&file).unwrap();
        assert_eq!(
            json,
            r#"{"path":"a.txt","change":"modified","outcome":"conflict","markers_written":true,"reason":"both changed"}"#
        );
        assert_eq!(serde_json::from_str::<PullFileResult>(&json).unwrap(), file);
        let repo = RepoPullResult {
            repository_id: RepositoryId::from("r"),
            files: vec![file],
        };
        assert_eq!(repo.conflict_count(), 1);
    }
}
