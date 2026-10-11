use crate::ids::{CheckoutId, RepositoryId, ThreadId, WorkspaceId};
use crate::workspace::{Workspace, WorkspaceMembership};
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

impl Thread {
    /// Snapshot workspace memberships that are enabled by default into isolated
    /// attachments. Later membership edits never mutate an existing thread.
    pub fn new_for_workspace(
        workspace: &Workspace,
        memberships: &[WorkspaceMembership],
        title: Option<String>,
    ) -> Self {
        let repo_attachments = memberships
            .iter()
            .filter(|membership| membership.default_enabled)
            .map(|membership| ThreadRepositoryAttachment {
                repository_id: membership.repository_id.clone(),
                base_revision: None,
                checkout_id: None,
                attach_mode: AttachMode::Isolated,
            })
            .collect();

        Self {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title,
            repo_attachments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Repository;

    fn membership(
        workspace: &Workspace,
        repository_id: RepositoryId,
        default_enabled: bool,
    ) -> WorkspaceMembership {
        WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id,
            alias: None,
            default_enabled,
        }
    }

    #[test]
    fn new_for_workspace_snapshots_enabled_memberships_as_isolated() {
        let workspace = Workspace::new("demo");
        let enabled = Repository::new(None, None, Some("main".into()));
        let disabled = Repository::new(None, None, Some("main".into()));
        let memberships = vec![
            membership(&workspace, enabled.id.clone(), true),
            membership(&workspace, disabled.id.clone(), false),
        ];

        let thread =
            Thread::new_for_workspace(&workspace, &memberships, Some("First thread".into()));

        assert_eq!(thread.workspace_id.as_ref(), Some(&workspace.id));
        assert!(thread.parent_thread_id.is_none());
        assert_eq!(thread.kind, ThreadKind::Top);
        assert_eq!(thread.title.as_deref(), Some("First thread"));
        assert_eq!(thread.repo_attachments.len(), 1);
        let attachment = &thread.repo_attachments[0];
        assert_eq!(attachment.repository_id, enabled.id);
        assert_eq!(attachment.attach_mode, AttachMode::Isolated);
        assert!(attachment.base_revision.is_none());
        assert!(attachment.checkout_id.is_none());
    }

    #[test]
    fn later_membership_changes_do_not_alter_existing_thread() {
        let workspace = Workspace::new("demo");
        let first = Repository::new(None, None, None);
        let second = Repository::new(None, None, None);
        let mut memberships = vec![membership(&workspace, first.id.clone(), true)];

        let thread = Thread::new_for_workspace(&workspace, &memberships, None);
        let snapshot = thread.repo_attachments.clone();

        memberships[0].default_enabled = false;
        memberships[0].alias = Some("renamed".into());
        memberships.push(membership(&workspace, second.id.clone(), true));

        assert_eq!(thread.repo_attachments.len(), 1);
        assert_eq!(thread.repo_attachments[0].repository_id, first.id);
        assert_eq!(thread.repo_attachments[0].attach_mode, AttachMode::Isolated);
        assert_eq!(
            thread.repo_attachments[0].base_revision,
            snapshot[0].base_revision
        );

        let later = Thread::new_for_workspace(&workspace, &memberships, None);
        assert_eq!(later.repo_attachments.len(), 1);
        assert_eq!(later.repo_attachments[0].repository_id, second.id);
        assert_ne!(
            later.repo_attachments[0].repository_id,
            thread.repo_attachments[0].repository_id
        );
    }
}
