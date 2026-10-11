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

impl Workspace {
    /// Create a workspace with a generated id and default settings.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: WorkspaceId::generate(),
            name: name.into(),
            settings: WorkspaceSettings::default(),
        }
    }
}

impl Repository {
    /// Create a repository with a generated id.
    pub fn new(
        canonical_remote: Option<String>,
        local_path: Option<PathBuf>,
        default_branch: Option<String>,
    ) -> Self {
        Self {
            id: RepositoryId::generate(),
            canonical_remote,
            local_path,
            default_branch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_new_generates_id_and_default_settings() {
        let ws = Workspace::new("LaFraternita");
        assert_eq!(ws.name, "LaFraternita");
        assert!(!ws.id.0.is_empty());
        assert_eq!(ws.settings.default_model, None);
        assert!(ws.settings.extra.is_null());
    }

    #[test]
    fn workspace_new_ids_are_unique() {
        let a = Workspace::new("a");
        let b = Workspace::new("b");
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn repository_new_populates_fields() {
        let repo = Repository::new(
            Some("git@example.com:org/app.git".into()),
            Some(PathBuf::from("/src/app")),
            Some("main".into()),
        );
        assert!(!repo.id.0.is_empty());
        assert_eq!(
            repo.canonical_remote.as_deref(),
            Some("git@example.com:org/app.git")
        );
        assert_eq!(
            repo.local_path.as_deref(),
            Some(PathBuf::from("/src/app").as_path())
        );
        assert_eq!(repo.default_branch.as_deref(), Some("main"));
    }

    #[test]
    fn repository_new_allows_unknown_location() {
        let repo = Repository::new(None, None, None);
        assert!(repo.canonical_remote.is_none());
        assert!(repo.local_path.is_none());
        assert!(repo.default_branch.is_none());
    }
}
