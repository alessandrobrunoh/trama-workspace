use crate::ids::{CheckoutId, RepositoryId, ThreadId, WorkspaceId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Top,
    Sub,
    Review,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    /// Managed isolated checkout (default).
    Isolated,
    /// Adopt the user's existing checkout (shared, not isolated).
    Adopted,
}

/// A repository attached to a thread, with its own checkout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadRepositoryAttachment {
    pub repository_id: RepositoryId,
    pub base_revision: Option<String>,
    pub checkout_id: Option<CheckoutId>,
    pub attach_mode: AttachMode,
}

/// The central persistent entity: conversation + work, versioned together.
/// Independent from any agent and from any physical checkout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub workspace_id: Option<WorkspaceId>,
    pub parent_thread_id: Option<ThreadId>,
    pub kind: ThreadKind,
    pub title: Option<String>,
    /// Explicit repository list. Initialized from workspace defaults at
    /// creation; changing the workspace later never mutates existing threads.
    pub repo_attachments: Vec<ThreadRepositoryAttachment>,
}
