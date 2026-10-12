use crate::error::{DomainError, Result};
use crate::ids::{CheckoutId, RepositoryId, ThreadId, WorkspaceId};
use crate::workspace::{Workspace, WorkspaceMembership};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    Top,
    /// Plain subthread of a top-level thread.
    Sub,
    /// Subthread that writes a change guide for, then reviews, its parent.
    Review,
    Fork,
    /// Subthread that lands its parent's changes (via the repo's land skill).
    Land,
}

impl ThreadKind {
    /// Kinds that are children of a top-level thread. Subthreads cannot have
    /// subthreads of their own.
    pub fn is_subthread(self) -> bool {
        matches!(self, Self::Sub | Self::Review | Self::Land)
    }
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

    /// A child of a top-level `parent`. The attachment list is copied from
    /// the parent (not re-read from the workspace); checkout metadata is left
    /// empty because the subthread gets its own managed copies. Nesting is
    /// rejected: a subthread cannot have subthreads.
    pub fn new_subthread(parent: &Thread, kind: ThreadKind, title: Option<String>) -> Result<Self> {
        if !kind.is_subthread() {
            return Err(DomainError::Invalid(format!(
                "{kind:?} is not a subthread kind"
            )));
        }
        if parent.parent_thread_id.is_some() || parent.kind != ThreadKind::Top {
            return Err(DomainError::Invalid(
                "subthreads cannot have subthreads; create it from the top-level thread".into(),
            ));
        }
        Ok(Self {
            id: ThreadId::generate(),
            workspace_id: parent.workspace_id.clone(),
            parent_thread_id: Some(parent.id.clone()),
            kind,
            title,
            repo_attachments: parent
                .repo_attachments
                .iter()
                .map(|attachment| ThreadRepositoryAttachment {
                    repository_id: attachment.repository_id.clone(),
                    base_revision: None,
                    checkout_id: None,
                    attach_mode: AttachMode::Isolated,
                })
                .collect(),
        })
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

    #[test]
    fn subthreads_copy_parent_attachments_and_cannot_nest() {
        let workspace = Workspace::new("demo");
        let repo = Repository::new(None, None, None);
        let memberships = vec![membership(&workspace, repo.id.clone(), true)];
        let mut parent = Thread::new_for_workspace(&workspace, &memberships, None);
        parent.repo_attachments[0].checkout_id = Some(CheckoutId::from("parent-checkout"));
        parent.repo_attachments[0].base_revision = Some("abc".into());

        let review =
            Thread::new_subthread(&parent, ThreadKind::Review, Some("review".into())).unwrap();
        assert_eq!(review.parent_thread_id.as_ref(), Some(&parent.id));
        assert_eq!(review.workspace_id, parent.workspace_id);
        assert_eq!(review.kind, ThreadKind::Review);
        assert_eq!(review.repo_attachments.len(), 1);
        assert_eq!(review.repo_attachments[0].repository_id, repo.id);
        assert!(review.repo_attachments[0].checkout_id.is_none());
        assert!(review.repo_attachments[0].base_revision.is_none());

        assert!(Thread::new_subthread(&review, ThreadKind::Sub, None).is_err());
        assert!(Thread::new_subthread(&parent, ThreadKind::Top, None).is_err());
        assert!(Thread::new_subthread(&parent, ThreadKind::Fork, None).is_err());
        assert!(ThreadKind::Land.is_subthread());
        assert!(!ThreadKind::Top.is_subthread());
        assert_eq!(
            serde_json::to_string(&ThreadKind::Land).unwrap(),
            r#""land""#
        );
    }
}
