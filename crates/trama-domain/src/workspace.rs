use crate::ids::{RepositoryId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A logical project grouping one or more Git repositories
/// (e.g. "LaFraternita" = backend + frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub settings: WorkspaceSettings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceSettings {
    pub default_model: Option<String>,
    /// Forward-compatible extra settings.
    pub extra: serde_json::Value,
}

/// A Git repository. Many-to-many with workspaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: RepositoryId,
    pub canonical_remote: Option<String>,
    pub local_path: Option<PathBuf>,
    pub default_branch: Option<String>,
}

/// Membership row: how a repository participates in a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMembership {
    pub workspace_id: WorkspaceId,
    pub repository_id: RepositoryId,
    pub alias: Option<String>,
    /// Whether new threads attach this repository by default.
    pub default_enabled: bool,
}
