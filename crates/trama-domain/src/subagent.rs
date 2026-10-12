//! Subagents: child threads an agent spawns through Trama's MCP tools
//! (instead of Claude Code's built-in subagent tool), so a person can open
//! and chat with each one like any other thread.

use crate::ids::ThreadId;
use serde::{Deserialize, Serialize};

/// What a subagent may do and where it works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentProfile {
    /// Own isolated copy of the parent's current state, full tools. When a
    /// task it was given succeeds, its changes are pulled into the parent.
    #[default]
    Worker,
    /// Works directly in the parent's checkout and does not edit files.
    Scout,
    /// A review child (copy of the parent's state) that reports back.
    Reviewer,
}

impl SubagentProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Scout => "scout",
            Self::Reviewer => "reviewer",
        }
    }
}

/// Lifecycle of the task a subagent was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    /// Created; waiting for a free slot (per-thread or global limit).
    Queued,
    Running,
    /// A worker finished, but its changes are not yet in the parent because
    /// the parent is busy. They are pulled when the parent waits or idles.
    WaitingToSync,
    Completed,
    Failed,
}

impl SubagentStatus {
    /// The task is over (successfully or not); waiters may return.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// The task still occupies (or waits for) a running slot.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// Who spawned a subagent thread: the parent thread's agent, through the
/// tool call `tool_use_id` (Claude's id, when the CLI sent it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnedBy {
    pub parent_thread_id: ThreadId,
    pub tool_use_id: Option<String>,
    pub profile: SubagentProfile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shapes_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&SubagentStatus::WaitingToSync).unwrap(),
            r#""waiting_to_sync""#
        );
        assert_eq!(
            serde_json::to_string(&SubagentProfile::Reviewer).unwrap(),
            r#""reviewer""#
        );
        let spawned = SpawnedBy {
            parent_thread_id: ThreadId::from("p"),
            tool_use_id: Some("toolu_1".into()),
            profile: SubagentProfile::Scout,
        };
        let json = serde_json::to_value(&spawned).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"parent_thread_id":"p","tool_use_id":"toolu_1","profile":"scout"})
        );
        assert_eq!(serde_json::from_value::<SpawnedBy>(json).unwrap(), spawned);
        assert!(SubagentStatus::Failed.is_terminal());
        assert!(!SubagentStatus::WaitingToSync.is_terminal());
        assert!(SubagentStatus::Queued.is_active());
        assert!(!SubagentStatus::WaitingToSync.is_active());
    }
}
