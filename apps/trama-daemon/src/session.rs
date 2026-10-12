//! Persistent session manager: TramaDB + injected `AgentAdapter`.

mod comments;
mod subthreads;

use crate::approval::{self, ApprovalHookSettings, HookDecision};
use crate::config::permission_mode_from_settings;
use crate::error::{DaemonError, Result};
use chrono::Utc;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use trama_db::{CheckoutLocation, TramaDb};
use trama_domain::agent::{
    AgentAdapter, AgentEvent, AgentKind, AgentPrompt, AgentSession, ApprovalDecision, BillingMode,
    SessionConfig, UsageReport,
};
use trama_domain::event::{Aggregate, DomainEvent, EventRecord, EventStore};
use trama_domain::ids::{AgentSessionId, CheckoutId, CommentId, MessageId, ThreadId, WorkspaceId};
use trama_domain::message::{Actor, ContentBlock, Message, Role};
use trama_domain::thread::{AttachMode, Thread, ThreadRepositoryAttachment};
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_infra::checkout::{CheckoutError, CheckoutManager};
use trama_protocol::{DaemonMessage, RepositorySnapshot, ThreadSnapshot, WorkspaceSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

struct Subscriber {
    id: SubscriptionId,
    tx: mpsc::UnboundedSender<DaemonMessage>,
}

/// Exclusive long-running operation on a thread. Exactly one is held at a
/// time; a reservation is made under the inner mutex before any await and
/// released only by the operation that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reservation {
    /// An agent turn was accepted (`send_prompt` through a terminal event).
    Turn,
    /// `migrate_legacy_thread` is cloning/persisting; `send_prompt` and other
    /// migrations are rejected for the whole window.
    Migration,
    /// A subthread is copying this thread's checkouts, or changes are being
    /// pulled into (or from) it.
    Checkouts,
}

#[derive(Default)]
struct ThreadRuntime {
    live_session: Option<AgentSession>,
    reservation: Option<Reservation>,
    interrupt_requested: bool,
    acc_text: String,
    subscribers: Vec<Subscriber>,
    /// Token in the live session's environment; see [`approval`].
    approval_token: Option<String>,
    /// Tools answered `allow_session` for the live session. Cleared whenever
    /// the live session starts or ends.
    session_allowed_tools: HashSet<String>,
    /// Last `SessionInfo` of the live session, replayed to new subscribers.
    session_info: Option<AgentEvent>,
    /// User prompt of the in-flight turn that delivered comments; taken when
    /// the turn ends to link (or return) them.
    comments_delivered_with: Option<MessageId>,
}

/// A permission request blocking a Claude Code hook process. The hook's
/// connection task holds the receiving end; exactly one resolution sends.
struct PendingApproval {
    thread_id: ThreadId,
    session_id: AgentSessionId,
    tool_name: String,
    input_json: serde_json::Value,
    responder: oneshot::Sender<HookDecision>,
}

struct Inner {
    next_sub: u64,
    threads: HashMap<ThreadId, ThreadRuntime>,
    approval_tokens: HashMap<String, ThreadId>,
    pending_approvals: HashMap<String, PendingApproval>,
}

/// Outcome of [`SessionManager::request_approval`].
pub enum ApprovalWait {
    /// Answered without asking (remembered, or no live session).
    Decided(HookDecision),
    /// Shown to clients; wait on `rx`, and call
    /// [`SessionManager::cancel_approval`] on timeout or hook disconnect.
    Pending {
        request_id: String,
        rx: oneshot::Receiver<HookDecision>,
    },
}

const USER_DENIED: &str = "The user denied this tool call in Trama.";
const REMEMBERED_FOR_SESSION: &str = "allowed for this session";

/// Releases a migration reservation when dropped, including when the future
/// that holds it is cancelled or returns early through `?`.
struct MigrationGuard {
    inner: Arc<Mutex<Inner>>,
    thread_id: ThreadId,
}

impl MigrationGuard {
    fn release_reservation(&mut self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(thread) = inner.threads.get_mut(&self.thread_id) {
            if thread.reservation == Some(Reservation::Migration) {
                thread.reservation = None;
            }
        }
    }
}

impl Drop for MigrationGuard {
    fn drop(&mut self) {
        self.release_reservation();
    }
}

/// A spawned turn that ended in an error. `agent_started` distinguishes a
/// turn that never started (missing adapter) from a session that started but
/// did not accept the prompt, so the persisted notice never claims the agent
/// did not run when it did.
struct TurnFailure {
    error: DaemonError,
    agent_started: bool,
}

/// Registry of live adapter sessions, per-thread subscribers, and DB access.
#[derive(Clone)]
pub struct SessionManager {
    db: TramaDb,
    adapter: Arc<dyn AgentAdapter>,
    checkout_manager: trama_infra::checkout::CheckoutManager,
    inner: Arc<Mutex<Inner>>,
    billing: BillingMode,
    /// `None`: no approval channel; the CLI denies prompts on its own.
    approval_hook: Option<Arc<ApprovalHookSettings>>,
}

impl SessionManager {
    pub fn new(db: TramaDb, adapter: Arc<dyn AgentAdapter>) -> Self {
        let root = trama_protocol::default_app_dir()
            .unwrap_or_else(|| std::env::temp_dir().join("TramaWorkspace"));
        Self::with_checkout_root(db, adapter, root)
    }

    pub fn with_checkout_root(
        db: TramaDb,
        adapter: Arc<dyn AgentAdapter>,
        root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            db,
            adapter,
            checkout_manager: trama_infra::checkout::CheckoutManager::new(root),
            inner: Arc::new(Mutex::new(Inner {
                next_sub: 1,
                threads: HashMap::new(),
                approval_tokens: HashMap::new(),
                pending_approvals: HashMap::new(),
            })),
            billing: BillingMode::default(),
            approval_hook: None,
        }
    }

    /// Billing for new agent sessions. Defaults to the subscription.
    pub fn with_billing(mut self, billing: BillingMode) -> Self {
        self.billing = billing;
        self
    }

    /// Route tool permission prompts of new sessions through `settings`.
    pub fn with_approval_hook(mut self, settings: ApprovalHookSettings) -> Self {
        self.approval_hook = Some(Arc::new(settings));
        self
    }

    /// How long a hook connection waits for a person before denying.
    pub fn approval_timeout(&self) -> Duration {
        self.approval_hook
            .as_ref()
            .map(|settings| settings.timeout)
            .unwrap_or(approval::DEFAULT_APPROVAL_TIMEOUT)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Release a finished agent-turn reservation. A migration reservation is
    /// left untouched; only its owner (or guard) releases it.
    fn release_turn(&self, thread_id: &ThreadId) {
        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            if thread.reservation == Some(Reservation::Turn) {
                thread.reservation = None;
            }
        }
    }

    /// Reserve the thread for a migration before any await, so a concurrent
    /// `send_prompt` or a second migration (including from another
    /// connection's manager) is rejected for the entire clone/persist window
    /// instead of only at check time. The inner mutex is never held across an
    /// await; the returned guard releases the reservation on drop, so a
    /// cancelled migration future cannot leave the thread stuck busy.
    fn begin_migration(&self, thread_id: &ThreadId) -> Result<MigrationGuard> {
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            if thread.reservation.is_some() || thread.live_session.is_some() {
                return Err(DaemonError::MigrationBusy);
            }
            thread.reservation = Some(Reservation::Migration);
        }
        Ok(MigrationGuard {
            inner: Arc::clone(&self.inner),
            thread_id: thread_id.clone(),
        })
    }

    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceSnapshot>> {
        let workspaces = self.db.list_workspaces().await?;
        let mut out = Vec::with_capacity(workspaces.len());
        for ws in workspaces {
            let repo_count = self.db.list_memberships(&ws.id).await?.len() as u32;
            out.push(WorkspaceSnapshot {
                id: ws.id,
                title: ws.name,
                repo_count,
            });
        }
        Ok(out)
    }

    pub async fn default_workspace_id(&self) -> Result<WorkspaceId> {
        self.list_workspaces()
            .await?
            .into_iter()
            .next()
            .map(|ws| ws.id)
            .ok_or_else(|| DaemonError::NotFound("no workspaces".into()))
    }

    pub async fn list_threads(&self, workspace_id: &WorkspaceId) -> Result<Vec<ThreadSnapshot>> {
        if self.db.get_workspace(workspace_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        let threads = self.db.list_threads(workspace_id).await?;
        self.thread_snapshots(&threads).await
    }

    /// Return thread checkout readiness, exposing a path only after validation.
    pub async fn thread_checkouts(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<trama_protocol::CheckoutSnapshot>> {
        use trama_protocol::{CheckoutSnapshot, CheckoutStatus};
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let memberships = match &thread.workspace_id {
            Some(workspace) => self.db.list_memberships(workspace).await?,
            None => Vec::new(),
        };
        let mut rows = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in thread.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            let source = std::fs::canonicalize(&source).unwrap_or(source);
            let (status, checkout_path, detail) = if let (Some(_id), Some(base)) = (
                attachment.checkout_id.as_ref(),
                attachment.base_revision.as_ref(),
            ) {
                let resolved = self.checkout_path(thread_id, &attachment, &source).await;
                let (path, repo_local) = match resolved {
                    Ok(resolved) if attachment.attach_mode == AttachMode::Isolated => resolved,
                    other => {
                        let detail = match other {
                            Err(error) => error.to_string(),
                            Ok(_) => "Prepara la copia di lavoro prima di aprirla.".into(),
                        };
                        rows.push(CheckoutSnapshot {
                            repository_id: attachment.repository_id,
                            title: source
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                            source_path: source,
                            checkout_path: None,
                            status: if attachment.attach_mode == AttachMode::Adopted {
                                CheckoutStatus::NeedsPreparation
                            } else {
                                CheckoutStatus::Invalid
                            },
                            detail: Some(detail),
                        });
                        continue;
                    }
                };
                let manager = self.checkout_manager.clone();
                let source_check = source.clone();
                let path_check = path.clone();
                let base_check = base.clone();
                let validation = tokio::task::spawn_blocking(move || {
                    validate_managed_checkout(
                        &manager,
                        &source_check,
                        &path_check,
                        &base_check,
                        repo_local,
                    )
                })
                .await
                .map_err(|e| {
                    DaemonError::Invalid(format!("checkout validation task failed: {e}"))
                })?;
                match validation {
                    Ok(()) => (CheckoutStatus::Ready, Some(path), None),
                    Err(e) if !path.exists() => (
                        CheckoutStatus::Missing,
                        None,
                        Some(format!("{e}. Prepare the checkout to continue.")),
                    ),
                    Err(e) => (CheckoutStatus::Invalid, None, Some(e.to_string())),
                }
            } else {
                (
                    CheckoutStatus::NeedsPreparation,
                    None,
                    Some("Prepare this thread's checkout before opening it".into()),
                )
            };
            rows.push(CheckoutSnapshot {
                repository_id: attachment.repository_id.clone(),
                title: memberships
                    .iter()
                    .find(|membership| membership.repository_id == attachment.repository_id)
                    .and_then(|membership| membership.alias.clone())
                    .unwrap_or_else(|| {
                        source
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }),
                source_path: source,
                checkout_path,
                status,
                detail,
            });
        }
        Ok(rows)
    }

    /// A recorded local path is authoritative; absence means a legacy global
    /// checkout, never permission to guess or create a replacement.
    async fn checkout_path(
        &self,
        thread: &ThreadId,
        attachment: &ThreadRepositoryAttachment,
        source: &std::path::Path,
    ) -> Result<(PathBuf, bool)> {
        let id = attachment.checkout_id.as_ref().ok_or_else(|| {
            DaemonError::Invalid("la copia di lavoro deve essere preparata".into())
        })?;
        if let Some(path) = self
            .db
            .checkout_location(thread, &attachment.repository_id, id)
            .await?
        {
            let expected = self
                .checkout_manager
                .repo_local_path(source, thread, id)
                .map_err(checkout_to_invalid)?;
            if path != expected {
                return Err(DaemonError::Invalid(
                    "la posizione registrata non corrisponde a questa copia di lavoro".into(),
                ));
            }
            Ok((path, true))
        } else {
            Ok((
                self.checkout_manager
                    .path(thread, &attachment.repository_id, id)
                    .map_err(checkout_to_invalid)?,
                false,
            ))
        }
    }

    pub async fn create_thread(
        &self,
        workspace_id: WorkspaceId,
        title: String,
    ) -> Result<ThreadSnapshot> {
        let workspace = self
            .db
            .get_workspace(&workspace_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("workspace {workspace_id}")))?;
        let memberships = self.db.list_memberships(&workspace_id).await?;
        if !memberships
            .iter()
            .any(|membership| membership.default_enabled)
        {
            return Err(DaemonError::Invalid(
                "workspace has no default repository. Add a local Git repository before creating a thread"
                    .into(),
            ));
        }
        // Build attachments in the same order the database reloads them
        // (repository id ascending) so the cwd chosen from the first
        // attachment at creation is identical after a restart. The order is
        // derived from persisted ids, not from aliases or insertion order.
        let mut memberships = memberships;
        memberships.sort_by(|a, b| a.repository_id.0.cmp(&b.repository_id.0));
        let mut thread = Thread::new_for_workspace(&workspace, &memberships, Some(title));
        let mut provision_requests = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in &mut thread.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            let checkout_id = CheckoutId::generate();
            provision_requests.push((source, attachment.repository_id.clone(), checkout_id));
        }
        let checkout_manager = self.checkout_manager.clone();
        let thread_id = thread.id.clone();
        let provisioned = tokio::task::spawn_blocking(move || {
            provision_requests
                .into_iter()
                .map(|(source, repository_id, checkout_id)| {
                    checkout_manager
                        .provision_repo_local(&source, &thread_id, &repository_id, &checkout_id)
                        .map(|checkout| {
                            (
                                CheckoutLocation {
                                    checkout_id,
                                    repository_id,
                                    path: checkout.path,
                                },
                                checkout.base_revision,
                            )
                        })
                })
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .await
        .map_err(|error| {
            DaemonError::Invalid(format!("checkout provisioning task failed: {error}"))
        })?
        .map_err(checkout_to_invalid)?;
        let mut locations = Vec::with_capacity(provisioned.len());
        for (attachment, (location, base_revision)) in
            thread.repo_attachments.iter_mut().zip(provisioned)
        {
            attachment.checkout_id = Some(location.checkout_id.clone());
            attachment.base_revision = Some(base_revision);
            locations.push(location);
        }
        self.db
            .create_thread_with_locations(&thread, &locations)
            .await?;
        let record = EventRecord::new(
            Aggregate::Thread {
                id: thread.id.clone(),
            },
            Actor::System,
            DomainEvent::ThreadCreated {
                thread: thread.clone(),
            },
            vec![],
        )?;
        EventStore::append(&self.db, vec![record]).await?;
        Ok(thread_snapshot(&thread))
    }

    pub async fn create_workspace(&self, name: String) -> Result<WorkspaceSnapshot> {
        let name = validate_workspace_name(&name)?;
        if self.db.get_workspace_by_name(&name).await?.is_some() {
            return Err(DaemonError::Invalid(format!(
                "a workspace named `{name}` already exists"
            )));
        }
        let workspace = Workspace::new(name);
        let record = EventRecord::new(
            Aggregate::Workspace {
                id: workspace.id.clone(),
            },
            Actor::System,
            DomainEvent::WorkspaceCreated {
                workspace: workspace.clone(),
            },
            vec![],
        )?;
        self.db
            .create_workspace_with_event(&workspace, &record)
            .await?;
        Ok(WorkspaceSnapshot {
            id: workspace.id,
            title: workspace.name,
            repo_count: 0,
        })
    }

    pub async fn add_repository(
        &self,
        workspace_id: WorkspaceId,
        path: PathBuf,
        alias: String,
    ) -> Result<RepositorySnapshot> {
        if self.db.get_workspace(&workspace_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        let alias = validate_label(&alias, "repository alias")?;
        let inspected =
            CheckoutManager::inspect_working_root(&path).map_err(checkout_to_invalid)?;
        let candidate = Repository::new(
            inspected.origin,
            Some(inspected.path.clone()),
            inspected.default_branch,
        );
        let stored = self
            .db
            .upsert_repository_membership(
                &candidate,
                &WorkspaceMembership {
                    workspace_id,
                    repository_id: candidate.id.clone(),
                    alias: Some(alias.clone()),
                    default_enabled: true,
                },
            )
            .await?;
        Ok(repository_snapshot(&stored, &alias, true))
    }

    pub async fn list_repositories(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<RepositorySnapshot>> {
        if self.db.get_workspace(workspace_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        let memberships = self.db.list_memberships(workspace_id).await?;
        let mut out = Vec::with_capacity(memberships.len());
        for membership in memberships {
            let repository = self
                .db
                .get_repository(&membership.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", membership.repository_id))
                })?;
            let title = membership.alias.clone().unwrap_or_else(|| {
                repository
                    .local_path
                    .as_ref()
                    .and_then(|path| {
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| repository.id.to_string())
            });
            out.push(repository_snapshot(
                &repository,
                &title,
                membership.default_enabled,
            ));
        }
        Ok(out)
    }

    /// Explicit migration. Reserves the thread for the whole operation so a
    /// concurrent prompt or migration is rejected (`busy`), prepares isolated
    /// clones, then atomically records checkout metadata, invalidates the
    /// previous external agent sessions, and appends a system message. Does
    /// not start the adapter and does not modify source working trees.
    pub async fn migrate_legacy_thread(&self, thread_id: ThreadId) -> Result<ThreadSnapshot> {
        let thread = self
            .db
            .get_thread(&thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        let _migration = self.begin_migration(&thread_id)?;
        if thread.repo_attachments.is_empty() {
            return Err(DaemonError::Invalid(
                "thread has no repository attachments to migrate".into(),
            ));
        }

        let mut plans = Vec::with_capacity(thread.repo_attachments.len());
        for attachment in &thread.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::Invalid(format!(
                        "repository {} is missing from the database. The thread was not changed",
                        attachment.repository_id
                    ))
                })?;
            let source = repository.local_path.clone().ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path. Restore the Git working tree before migrating; nothing was changed",
                    attachment.repository_id
                ))
            })?;
            if !source.exists() {
                return Err(DaemonError::Invalid(format!(
                    "repository {} is missing at {}. Restore the local Git working tree before migrating; the thread and its messages were not changed",
                    attachment.repository_id,
                    source.display()
                )));
            }
            let inspected =
                CheckoutManager::inspect_working_root(&source).map_err(migration_checkout_error)?;
            let managed = attachment.attach_mode == AttachMode::Isolated
                && attachment.checkout_id.is_some()
                && attachment.base_revision.is_some();
            if managed {
                let (checkout_path, repo_local) =
                    self.checkout_path(&thread_id, attachment, &source).await?;
                plans.push(MigrationPlan {
                    source: inspected.path,
                    attachment: attachment.clone(),
                    provision: false,
                    checkout_path: Some(checkout_path),
                    repo_local,
                });
            } else {
                plans.push(MigrationPlan {
                    source: inspected.path,
                    attachment: ThreadRepositoryAttachment {
                        repository_id: attachment.repository_id.clone(),
                        base_revision: None,
                        checkout_id: Some(CheckoutId::generate()),
                        attach_mode: AttachMode::Isolated,
                    },
                    provision: true,
                    checkout_path: None,
                    repo_local: true,
                });
            }
        }

        let changed = plans.iter().any(|plan| plan.provision);
        let checkout_manager = self.checkout_manager.clone();
        let thread_key = thread.id.clone();
        let plans = tokio::task::spawn_blocking(move || {
            prepare_migration(checkout_manager, &thread_key, plans)
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout migration task failed: {error}")))?
        .map_err(migration_checkout_error)?;

        if !changed {
            return Ok(thread_snapshot(&thread));
        }

        let attachments: Vec<_> = plans.iter().map(|plan| plan.attachment.clone()).collect();
        let locations: Vec<_> = plans
            .iter()
            .filter(|plan| plan.repo_local)
            .filter_map(|plan| {
                Some(CheckoutLocation {
                    checkout_id: plan.attachment.checkout_id.clone()?,
                    repository_id: plan.attachment.repository_id.clone(),
                    path: plan.checkout_path.clone()?,
                })
            })
            .collect();
        if attachments.iter().any(|attachment| {
            attachment.checkout_id.is_none() || attachment.base_revision.is_none()
        }) {
            return Err(DaemonError::Invalid(
                "migration did not record a checkout. The thread was not changed".into(),
            ));
        }
        let message = Message {
            id: MessageId::generate(),
            thread_id: thread.id.clone(),
            parent_id: None,
            actor: Actor::System,
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: MIGRATION_NOTICE.into(),
            }],
            created_at: Utc::now(),
        };
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread.id.clone(),
            },
            Actor::System,
            DomainEvent::MessageAdded {
                message: message.clone(),
            },
            vec![],
        )?;
        self.db
            .persist_thread_migration_with_locations(
                &thread.id,
                &attachments,
                &message,
                &event,
                &locations,
            )
            .await?;
        self.broadcast(
            &thread.id,
            DaemonMessage::MessageAppended {
                thread_id: thread.id.clone(),
                message,
            },
        );
        let mut updated = thread;
        updated.repo_attachments = attachments;
        Ok(thread_snapshot(&updated))
    }

    pub async fn subscribe(
        &self,
        thread_id: &ThreadId,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> Result<SubscriptionId> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let mut history = self.inherited_messages(thread_id).await?;
        history.extend(self.db.list_messages_by_thread(thread_id).await?);
        let id = {
            let mut inner = self.lock();
            let id = SubscriptionId(inner.next_sub);
            inner.next_sub += 1;
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.subscribers.push(Subscriber { id, tx: tx.clone() });
            id
        };
        for message in history {
            let _ = tx.send(DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            });
        }
        let totals = self.db.usage_totals_for_thread(thread_id).await?;
        if usage_report_present(&totals) {
            let _ = tx.send(DaemonMessage::UsageSnapshot {
                thread_id: thread_id.clone(),
                totals,
            });
        }
        // A client that (re)connects mid-turn must see what the agent is
        // blocked on. Request ids are ULID-based, so sorting keeps order.
        let (session_info, mut pending): (
            Option<AgentEvent>,
            Vec<(String, String, serde_json::Value)>,
        ) = {
            let inner = self.lock();
            (
                inner
                    .threads
                    .get(thread_id)
                    .and_then(|t| t.session_info.clone()),
                inner
                    .pending_approvals
                    .iter()
                    .filter(|(_, p)| &p.thread_id == thread_id)
                    .map(|(id, p)| (id.clone(), p.tool_name.clone(), p.input_json.clone()))
                    .collect(),
            )
        };
        if let Some(event) = session_info {
            let _ = tx.send(DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event,
            });
        }
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        for (request_id, tool_name, input_json) in pending {
            let _ = tx.send(DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::ApprovalRequested {
                    request_id,
                    tool_name,
                    input_json,
                },
            });
        }
        Ok(id)
    }

    pub fn unsubscribe(&self, thread_id: &ThreadId, sub_id: SubscriptionId) -> Result<()> {
        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            thread.subscribers.retain(|s| s.id != sub_id);
            Ok(())
        } else {
            Err(DaemonError::NotFound(format!("thread {thread_id}")))
        }
    }

    pub fn unsubscribe_all(&self, subs: &[(ThreadId, SubscriptionId)]) {
        let mut inner = self.lock();
        for (thread_id, sub_id) in subs {
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.subscribers.retain(|s| s.id != *sub_id);
            }
        }
    }

    /// Persist the user message, then spawn the adapter turn. The thread's
    /// pending comments go out with it, as a block before `text`; the stored
    /// message is exactly the prompt sent. A second prompt on a busy or
    /// migrating thread returns [`DaemonError::Busy`].
    pub async fn send_prompt(&self, thread_id: ThreadId, text: String) -> Result<Message> {
        self.start_turn(thread_id, text, false).await
    }

    /// Deliver the pending comments as a turn of their own. Fails with
    /// [`DaemonError::Invalid`] when nothing is pending.
    pub async fn send_comments(&self, thread_id: ThreadId) -> Result<Message> {
        self.start_turn(thread_id, String::new(), true).await
    }

    async fn start_turn(
        &self,
        thread_id: ThreadId,
        text: String,
        comments_only: bool,
    ) -> Result<Message> {
        if self.db.get_thread(&thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            if thread.reservation.is_some() {
                return Err(DaemonError::Busy);
            }
            thread.reservation = Some(Reservation::Turn);
            thread.interrupt_requested = false;
            thread.acc_text.clear();
            thread.comments_delivered_with = None;
        }
        // Reject a prompt that cannot run before it is stored, so a failed send
        // is not replayed or started later without another user action. The
        // turn reservation is held meanwhile, so a migration cannot retarget
        // the thread's checkouts under this prompt.
        if let Err(err) = self.session_config(&thread_id).await {
            self.release_turn(&thread_id);
            return Err(err);
        }

        let delivery = match self.pending_delivery(&thread_id).await {
            Ok(delivery) => delivery,
            Err(err) => {
                self.release_turn(&thread_id);
                return Err(err);
            }
        };
        if comments_only && delivery.is_none() {
            self.release_turn(&thread_id);
            return Err(DaemonError::Invalid(
                "there are no pending comments to send".into(),
            ));
        }
        let prompt = comments::compose_prompt(
            delivery.as_ref().map(|delivery| delivery.block.as_str()),
            &text,
        );
        let message = Message::new_user(thread_id.clone(), comments::LOCAL_USER, prompt.clone());
        let delivered_ids = match &delivery {
            None => {
                if let Err(err) = self.db.append_message(&message).await {
                    self.release_turn(&thread_id);
                    return Err(err.into());
                }
                if let Err(err) = self
                    .append_event(
                        Aggregate::Thread {
                            id: thread_id.clone(),
                        },
                        message.actor.clone(),
                        DomainEvent::MessageAdded {
                            message: message.clone(),
                        },
                    )
                    .await
                {
                    crate::log::warn(format!("failed to append MessageAdded: {err}"));
                }
                Vec::new()
            }
            Some(delivery) => {
                if let Err(err) = self.persist_delivery(&message, &delivery.ids).await {
                    self.release_turn(&thread_id);
                    return Err(err);
                }
                let mut inner = self.lock();
                if let Some(thread) = inner.threads.get_mut(&thread_id) {
                    thread.comments_delivered_with = Some(message.id.clone());
                }
                delivery.ids.clone()
            }
        };

        self.broadcast(
            &thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: message.clone(),
            },
        );
        self.broadcast_comment_updates(&thread_id, &delivered_ids)
            .await;

        let manager = self.clone();
        tokio::spawn(async move {
            if let Err(failure) = manager.run_turn(thread_id.clone(), prompt).await {
                manager.handle_turn_failure(&thread_id, failure).await;
            }
        });
        Ok(message)
    }

    /// Store the prompt carrying comments and mark them delivered, atomically.
    async fn persist_delivery(&self, message: &Message, ids: &[CommentId]) -> Result<()> {
        let thread_id = &message.thread_id;
        let turn = self.db.count_user_prompts(thread_id).await? + 1;
        let aggregate = Aggregate::Thread {
            id: thread_id.clone(),
        };
        let events = vec![
            EventRecord::new(
                aggregate.clone(),
                message.actor.clone(),
                DomainEvent::MessageAdded {
                    message: message.clone(),
                },
                vec![],
            )?,
            EventRecord::new(
                aggregate,
                message.actor.clone(),
                DomainEvent::CommentsDelivered {
                    thread_id: thread_id.clone(),
                    comment_ids: ids.to_vec(),
                    message_id: message.id.clone(),
                    turn,
                },
                vec![],
            )?,
        ];
        self.db
            .deliver_comments(message, ids, turn, &events)
            .await?;
        Ok(())
    }

    pub async fn interrupt(&self, thread_id: &ThreadId) -> Result<bool> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let session = {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.interrupt_requested = true;
            thread.live_session.clone()
        };
        // Release a hook blocked on a person first; `interrupt` tells Claude
        // Code to stop rather than try another approach.
        self.cancel_thread_approvals(thread_id, "The user interrupted the turn in Trama.", true)
            .await;
        match session {
            Some(session) => {
                self.adapter.interrupt(&session).await?;
                Ok(true)
            }
            None => Ok(true),
        }
    }

    pub async fn respond_approval(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<()> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        if let Some((answered, remembered)) = self.take_answered(thread_id, request_id, decision) {
            let hook = match decision {
                ApprovalDecision::Deny => HookDecision::deny(USER_DENIED),
                allow => HookDecision::allow(allow),
            };
            self.finish_approvals(
                vec![answered],
                &hook,
                None,
                Actor::Human { id: "user".into() },
            )
            .await;
            // Other requests for the same tool that were already waiting.
            self.finish_approvals(
                remembered,
                &HookDecision::allow(ApprovalDecision::AllowSession),
                Some(REMEMBERED_FOR_SESSION.into()),
                Actor::System,
            )
            .await;
            return Ok(());
        }
        // Brokered sessions (Claude Code, or any session given the hook)
        // answer only through the broker above. Other runtimes may hold the
        // request in-band.
        if self.approval_hook.is_some() || self.adapter.kind() == AgentKind::ClaudeCode {
            return Err(DaemonError::NotFound(format!(
                "approval request {request_id} is not pending (already answered, timed out, or the session ended)"
            )));
        }
        let session = {
            let inner = self.lock();
            inner
                .threads
                .get(thread_id)
                .and_then(|t| t.live_session.clone())
        };
        match session {
            Some(session) => {
                self.adapter
                    .respond_approval(&session, request_id, decision)
                    .await?;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Remove the answered request (only if it belongs to `thread_id`). For
    /// `allow_session`, remember the tool for the live session and also take
    /// every other pending request for that tool on the thread.
    #[allow(clippy::type_complexity)]
    fn take_answered(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Option<((String, PendingApproval), Vec<(String, PendingApproval)>)> {
        let mut inner = self.lock();
        if inner
            .pending_approvals
            .get(request_id)
            .is_none_or(|p| &p.thread_id != thread_id)
        {
            return None;
        }
        let answered = inner.pending_approvals.remove(request_id)?;
        let mut remembered = Vec::new();
        if decision == ApprovalDecision::AllowSession {
            let tool = answered.tool_name.clone();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                if thread
                    .live_session
                    .as_ref()
                    .is_some_and(|s| s.id == answered.session_id)
                {
                    thread.session_allowed_tools.insert(tool.clone());
                }
            }
            let siblings: Vec<String> = inner
                .pending_approvals
                .iter()
                .filter(|(_, p)| {
                    &p.thread_id == thread_id
                        && p.tool_name == tool
                        && p.session_id == answered.session_id
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in siblings {
                if let Some(p) = inner.pending_approvals.remove(&id) {
                    remembered.push((id, p));
                }
            }
        }
        Some(((request_id.to_string(), answered), remembered))
    }

    /// Called for every `PermissionRequest` hook. Unknown tokens and ended
    /// sessions are denied; tools remembered for the session are allowed
    /// without asking; anything else is shown to subscribers.
    pub async fn request_approval(
        &self,
        token: &str,
        tool_name: String,
        input_json: serde_json::Value,
    ) -> ApprovalWait {
        enum Next {
            Deny(&'static str),
            Remembered(ThreadId, AgentSessionId),
            Ask(ThreadId, AgentSessionId),
        }
        let request_id = format!("apr-{}", MessageId::generate());
        let (tx, rx) = oneshot::channel();
        let next = {
            let mut inner = self.lock();
            match inner.approval_tokens.get(token).cloned() {
                None => Next::Deny("This approval token does not belong to a live Trama session."),
                Some(thread_id) => {
                    let live = inner.threads.get(&thread_id).and_then(|thread| {
                        let session = thread.live_session.as_ref()?;
                        Some((
                            session.id.clone(),
                            thread.session_allowed_tools.contains(&tool_name),
                        ))
                    });
                    match live {
                        None => Next::Deny("The agent session has ended in Trama."),
                        Some((session_id, true)) => Next::Remembered(thread_id, session_id),
                        Some((session_id, false)) => {
                            inner.pending_approvals.insert(
                                request_id.clone(),
                                PendingApproval {
                                    thread_id: thread_id.clone(),
                                    session_id: session_id.clone(),
                                    tool_name: tool_name.clone(),
                                    input_json: input_json.clone(),
                                    responder: tx,
                                },
                            );
                            Next::Ask(thread_id, session_id)
                        }
                    }
                }
            }
        };

        match next {
            Next::Deny(message) => ApprovalWait::Decided(HookDecision::deny(message)),
            Next::Remembered(thread_id, session_id) => {
                self.record_approval(
                    &session_id,
                    Actor::System,
                    DomainEvent::ApprovalResolved {
                        thread_id: thread_id.clone(),
                        session_id: session_id.clone(),
                        request_id,
                        tool_name,
                        decision: ApprovalDecision::AllowSession,
                        reason: Some(REMEMBERED_FOR_SESSION.into()),
                    },
                )
                .await;
                ApprovalWait::Decided(HookDecision::allow(ApprovalDecision::AllowSession))
            }
            Next::Ask(thread_id, session_id) => {
                self.record_approval(
                    &session_id,
                    Actor::Agent {
                        session: session_id.clone(),
                    },
                    DomainEvent::ApprovalRequested {
                        thread_id: thread_id.clone(),
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        tool_name: tool_name.clone(),
                        input_json: input_json.clone(),
                    },
                )
                .await;
                self.broadcast(
                    &thread_id,
                    DaemonMessage::AgentEventEnvelope {
                        thread_id: thread_id.clone(),
                        event: AgentEvent::ApprovalRequested {
                            request_id: request_id.clone(),
                            tool_name,
                            input_json,
                        },
                    },
                );
                ApprovalWait::Pending { request_id, rx }
            }
        }
    }

    /// Deny a pending request that timed out or whose hook went away.
    /// Returns `false` when it was already resolved, so the caller should
    /// report that resolution instead.
    pub async fn cancel_approval(&self, request_id: &str, message: &str) -> bool {
        let taken = self.lock().pending_approvals.remove(request_id);
        let Some(pending) = taken else {
            return false;
        };
        self.finish_approvals(
            vec![(request_id.to_string(), pending)],
            &HookDecision::deny(message),
            Some(message.to_string()),
            Actor::System,
        )
        .await;
        true
    }

    /// Deny every pending request on a thread (interrupt, session end).
    async fn cancel_thread_approvals(&self, thread_id: &ThreadId, message: &str, interrupt: bool) {
        let taken: Vec<(String, PendingApproval)> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .pending_approvals
                .iter()
                .filter(|(_, p)| &p.thread_id == thread_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| inner.pending_approvals.remove(&id).map(|p| (id, p)))
                .collect()
        };
        if taken.is_empty() {
            return;
        }
        let mut decision = HookDecision::deny(message);
        decision.interrupt = interrupt;
        self.finish_approvals(taken, &decision, Some(message.to_string()), Actor::System)
            .await;
    }

    /// Unblock each hook, persist the resolution, and tell subscribers.
    async fn finish_approvals(
        &self,
        resolved: Vec<(String, PendingApproval)>,
        decision: &HookDecision,
        reason: Option<String>,
        actor: Actor,
    ) {
        for (request_id, pending) in resolved {
            // The hook may already be gone; its connection task then cancels
            // nothing, because the request is no longer pending.
            let _ = pending.responder.send(decision.clone());
            self.record_approval(
                &pending.session_id,
                actor.clone(),
                DomainEvent::ApprovalResolved {
                    thread_id: pending.thread_id.clone(),
                    session_id: pending.session_id.clone(),
                    request_id: request_id.clone(),
                    tool_name: pending.tool_name,
                    decision: decision.decision,
                    reason: reason.clone(),
                },
            )
            .await;
            self.broadcast(
                &pending.thread_id,
                DaemonMessage::AgentEventEnvelope {
                    thread_id: pending.thread_id.clone(),
                    event: AgentEvent::ApprovalResolved {
                        request_id,
                        decision: decision.decision,
                        reason: reason.clone(),
                    },
                },
            );
        }
    }

    async fn record_approval(&self, session_id: &AgentSessionId, actor: Actor, event: DomainEvent) {
        let kind = event.kind();
        if let Err(err) = self
            .append_event(
                Aggregate::Session {
                    id: session_id.clone(),
                },
                actor,
                event,
            )
            .await
        {
            crate::log::warn(format!("failed to append {kind}: {err}"));
        }
    }

    /// Give a new session's config the hook, the daemon socket, and a fresh
    /// token mapped to `thread_id`. Returns the token so a failed start can
    /// drop it.
    fn attach_approval_hook(
        &self,
        thread_id: &ThreadId,
        config: &mut SessionConfig,
    ) -> Option<String> {
        let settings = self.approval_hook.as_ref()?;
        let token = approval::generate_token();
        config.env.insert(
            approval::ENV_SOCKET.into(),
            settings.socket_path.to_string_lossy().into_owned(),
        );
        config.env.insert(approval::ENV_TOKEN.into(), token.clone());
        config.approval_hook = Some(settings.hook_config());
        let mut inner = self.lock();
        inner
            .approval_tokens
            .insert(token.clone(), thread_id.clone());
        let thread = inner.threads.entry(thread_id.clone()).or_default();
        if let Some(old) = thread.approval_token.replace(token.clone()) {
            inner.approval_tokens.remove(&old);
        }
        Some(token)
    }

    fn drop_approval_token(&self, thread_id: &ThreadId, token: Option<String>) {
        let Some(token) = token else {
            return;
        };
        let mut inner = self.lock();
        inner.approval_tokens.remove(&token);
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            if thread.approval_token.as_deref() == Some(token.as_str()) {
                thread.approval_token = None;
            }
        }
    }

    fn broadcast(&self, thread_id: &ThreadId, message: DaemonMessage) {
        let mut inner = self.lock();
        let Some(thread) = inner.threads.get_mut(thread_id) else {
            return;
        };
        thread
            .subscribers
            .retain(|sub| sub.tx.send(message.clone()).is_ok());
    }

    async fn append_event(
        &self,
        aggregate: Aggregate,
        actor: Actor,
        event: DomainEvent,
    ) -> Result<()> {
        let record = EventRecord::new(aggregate, actor, event, vec![])?;
        EventStore::append(&self.db, vec![record]).await?;
        Ok(())
    }

    /// Persist one system message (row plus `MessageAdded` event) and
    /// broadcast it, so the outcome is visible after a restart. A store
    /// failure is logged; it must not lose the in-memory turn state cleanup.
    async fn append_system_notice(&self, thread_id: &ThreadId, text: String) {
        let message = Message {
            id: MessageId::generate(),
            thread_id: thread_id.clone(),
            parent_id: None,
            actor: Actor::System,
            role: Role::System,
            content: vec![ContentBlock::Text { text }],
            created_at: Utc::now(),
        };
        if let Err(err) = self.db.append_message(&message).await {
            crate::log::warn(format!("failed to persist system message: {err}"));
            return;
        }
        if let Err(err) = self
            .append_event(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                Actor::System,
                DomainEvent::MessageAdded {
                    message: message.clone(),
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append MessageAdded: {err}"));
        }
        self.broadcast(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            },
        );
    }

    /// Keep a failed turn visible after restart. The prompt is not retried.
    /// The wording distinguishes a turn that never started (for example
    /// Claude is missing) from a session that started but rejected the
    /// prompt, so a transcript never claims the agent did not run when it
    /// did.
    async fn handle_turn_failure(&self, thread_id: &ThreadId, failure: TurnFailure) {
        let text = if failure.agent_started {
            format!(
                "The agent session started but the prompt was not accepted: {}. \
                 No output was produced for this prompt",
                failure.error
            )
        } else {
            format!("The agent did not run: {}", failure.error)
        };
        self.append_system_notice(thread_id, text).await;
        if failure.agent_started {
            // The session may have read part of the prompt; never redeliver
            // silently, but the comments are not answered either.
            self.take_turn_delivery(thread_id);
        } else {
            self.return_undelivered_comments(thread_id).await;
        }
        self.release_turn(thread_id);
        self.broadcast(
            thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::Failed {
                    message: failure.error.to_string(),
                },
            },
        );
        self.broadcast(
            thread_id,
            DaemonMessage::Error {
                code: failure.error.code().into(),
                message: failure.error.to_string(),
            },
        );
    }

    async fn run_turn(
        &self,
        thread_id: ThreadId,
        text: String,
    ) -> std::result::Result<(), TurnFailure> {
        // Best-effort "last turn" baseline. It runs beside session start and
        // the model's first round trip instead of delaying the prompt; an
        // edit landing within that window would count as pre-turn state.
        let snapshot = self.clone();
        let snapshot_thread = thread_id.clone();
        tokio::spawn(async move { snapshot.snapshot_turn_start(&snapshot_thread).await });
        let session = match self.ensure_session(&thread_id).await {
            Ok(session) => session,
            Err(err) => {
                return Err(TurnFailure {
                    error: err,
                    agent_started: false,
                });
            }
        };
        let interrupted = {
            let inner = self.lock();
            inner
                .threads
                .get(&thread_id)
                .map(|t| t.interrupt_requested)
                .unwrap_or(false)
        };
        if interrupted {
            let _ = self.adapter.interrupt(&session).await;
            self.return_undelivered_comments(&thread_id).await;
            self.release_turn(&thread_id);
            return Ok(());
        }
        let session_id = session.id.clone();
        if let Err(err) = self
            .append_event(
                Aggregate::Session {
                    id: session_id.clone(),
                },
                Actor::Agent {
                    session: session_id.clone(),
                },
                DomainEvent::AgentTurnStarted {
                    thread_id: thread_id.clone(),
                    session_id,
                    agent_kind: agent_kind_label(self.adapter.kind()).into(),
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append AgentTurnStarted: {err}"));
        }
        if let Err(err) = self.adapter.send(&session, AgentPrompt { text }).await {
            return Err(TurnFailure {
                error: err.into(),
                agent_started: true,
            });
        }
        Ok(())
    }

    async fn ensure_session(&self, thread_id: &ThreadId) -> Result<AgentSession> {
        if let Some(session) = {
            let inner = self.lock();
            inner
                .threads
                .get(thread_id)
                .and_then(|t| t.live_session.clone())
        } {
            return Ok(session);
        }

        let mut config = self.session_config(thread_id).await?;
        let token = self.attach_approval_hook(thread_id, &mut config);
        let (tx, rx) = async_channel::unbounded();
        let session = match self.adapter.start(config, tx).await {
            Ok(session) => session,
            Err(err) => {
                self.drop_approval_token(thread_id, token);
                return Err(DaemonError::Agent(format!(
                    "{err}. Install Claude Code or set CLAUDE_BIN to the `claude` binary."
                )));
            }
        };
        if let Err(err) = self
            .db
            .create_agent_session(thread_id, &session, agent_kind_label(self.adapter.kind()))
            .await
        {
            self.drop_approval_token(thread_id, token);
            return Err(err.into());
        }
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.live_session = Some(session.clone());
            thread.session_allowed_tools.clear();
            thread.session_info = None;
        }

        let manager = self.clone();
        let pump_thread = thread_id.clone();
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                manager.on_agent_event(&pump_thread, event).await;
            }
            manager.on_pump_closed(&pump_thread).await;
        });
        Ok(session)
    }

    async fn session_config(&self, thread_id: &ThreadId) -> Result<SessionConfig> {
        let thread = self
            .db
            .get_thread(thread_id)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        if thread.repo_attachments.is_empty() {
            return Err(DaemonError::Invalid(
                "thread has no repository paths. Add a local Git repository before starting the agent"
                    .into(),
            ));
        }
        let mut paths = Vec::with_capacity(thread.repo_attachments.len());
        let mut validations = Vec::new();
        for attachment in &thread.repo_attachments {
            let repository = self
                .db
                .get_repository(&attachment.repository_id)
                .await?
                .ok_or_else(|| {
                    DaemonError::NotFound(format!("repository {}", attachment.repository_id))
                })?;
            let source = repository.local_path.ok_or_else(|| {
                DaemonError::Invalid(format!(
                    "repository {} has no local path",
                    attachment.repository_id
                ))
            })?;
            match attachment.attach_mode {
                AttachMode::Isolated => {
                    let checkout_id = attachment.checkout_id.as_ref().ok_or_else(|| {
                        DaemonError::Invalid(format!(
                            "thread {} predates managed checkouts. Migrate it explicitly (migrate_legacy_thread) before sending. The original files are left untouched, and this prompt was not run",
                            thread.id
                        ))
                    })?;
                    let base_revision = attachment.base_revision.as_deref().ok_or_else(|| {
                        DaemonError::Invalid("managed checkout has no recorded base commit".into())
                    })?;
                    let (path, repo_local) =
                        self.checkout_path(&thread.id, attachment, &source).await?;
                    validations.push((
                        source.clone(),
                        path.clone(),
                        base_revision.to_owned(),
                        repo_local,
                    ));
                    paths.push(path);
                }
                AttachMode::Adopted => {
                    return Err(DaemonError::Invalid(format!(
                        "thread {} has an adopted repository attachment. Migrate it explicitly (migrate_legacy_thread) to an isolated checkout before sending. The agent will not run on the original working tree",
                        thread.id
                    )));
                }
            }
        }
        let checkout_manager = self.checkout_manager.clone();
        tokio::task::spawn_blocking(move || {
            for (source, path, base_revision, repo_local) in validations {
                validate_managed_checkout(
                    &checkout_manager,
                    &source,
                    &path,
                    &base_revision,
                    repo_local,
                )?;
            }
            Ok::<_, trama_infra::checkout::CheckoutError>(())
        })
        .await
        .map_err(|error| DaemonError::Invalid(format!("checkout validation task failed: {error}")))?
        .map_err(|error| DaemonError::Invalid(error.to_string()))?;
        let cwd = paths.remove(0);
        let extra_dirs = paths;

        let workspace: Option<Workspace> = match &thread.workspace_id {
            Some(id) => self.db.get_workspace(id).await?,
            None => None,
        };
        let (permission_mode, model) = match &workspace {
            Some(ws) => (
                permission_mode_from_settings(&ws.settings),
                ws.settings.default_model.clone(),
            ),
            None => (trama_domain::agent::PermissionMode::Default, None),
        };

        let own_session = self
            .db
            .latest_agent_session_for_thread(thread_id)
            .await?
            .filter(|session| same_directory(&session.cwd, &cwd))
            .and_then(|session| session.external_session_id);
        // A subthread's first session inherits its parent's conversation by
        // forking the parent's session; afterwards it resumes its own.
        let (resume_external_session, fork_session) = match own_session {
            Some(id) => (Some(id), false),
            None => match self.inherited_session(&thread).await? {
                Some(parent) => (Some(parent), true),
                None => (None, false),
            },
        };

        Ok(SessionConfig {
            cwd,
            model,
            permission_mode,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            extra_dirs,
            mcp_config_path: None,
            append_system_prompt: None,
            max_turns: None,
            env: BTreeMap::new(),
            resume_external_session,
            billing: self.billing,
            approval_hook: None,
            fork_session,
        })
    }

    async fn on_agent_event(&self, thread_id: &ThreadId, event: AgentEvent) {
        if let AgentEvent::SessionStarted {
            external_session_id,
            model,
        } = &event
        {
            let session_id = {
                let mut inner = self.lock();
                if let Some(thread) = inner.threads.get_mut(thread_id) {
                    if let Some(session) = thread.live_session.as_mut() {
                        if external_session_id.is_some() {
                            session.external_session_id = external_session_id.clone();
                        }
                        if model.is_some() {
                            session.model = model.clone();
                        }
                        Some((
                            session.id.clone(),
                            session.external_session_id.clone(),
                            session.model.clone(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((id, external, model)) = session_id {
                if let Err(err) = self
                    .db
                    .set_agent_session_external(&id, external, model)
                    .await
                {
                    crate::log::warn(format!("failed to persist external session id: {err}"));
                }
            }
        }

        if matches!(event, AgentEvent::SessionInfo { .. }) {
            let mut inner = self.lock();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.session_info = Some(event.clone());
            }
        }

        if let AgentEvent::TextDelta { text } = &event {
            let mut inner = self.lock();
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.acc_text.push_str(text);
            }
        }

        if let AgentEvent::UsageReported { report } = &event {
            let session_id = {
                let inner = self.lock();
                inner
                    .threads
                    .get(thread_id)
                    .and_then(|t| t.live_session.as_ref().map(|s| s.id.clone()))
            };
            if let Some(session_id) = session_id {
                if let Err(err) = self.db.record_usage(&session_id, thread_id, report).await {
                    crate::log::warn(format!("failed to record usage: {err}"));
                }
            }
        }

        let turn_completed = match &event {
            AgentEvent::TurnCompleted {
                num_turns,
                cost_usd_micros,
            } => Some((*num_turns, *cost_usd_micros)),
            _ => None,
        };
        let failure_text = match &event {
            AgentEvent::Failed { message } => Some(message.clone()),
            _ => None,
        };

        self.broadcast(
            thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event,
            },
        );

        if let Some((num_turns, cost_usd_micros)) = turn_completed {
            self.finish_turn(thread_id, num_turns, cost_usd_micros)
                .await;
        } else if let Some(failure) = failure_text {
            self.finish_failed_turn(thread_id, &failure).await;
        }
    }

    /// A terminal `Failed` event ended the turn. Persist one accurate system
    /// message — including any partial output that would otherwise be lost —
    /// and release the thread. Once a turn has completed, its reservation is
    /// already released, so a late `Failed` event appends nothing.
    async fn finish_failed_turn(&self, thread_id: &ThreadId, failure: &str) {
        let (reserved, interrupted, partial) = {
            let mut inner = self.lock();
            match inner.threads.get_mut(thread_id) {
                Some(thread) if thread.reservation == Some(Reservation::Turn) => {
                    thread.reservation = None;
                    (
                        true,
                        thread.interrupt_requested,
                        std::mem::take(&mut thread.acc_text),
                    )
                }
                _ => (false, false, String::new()),
            }
        };
        if !reserved {
            return;
        }
        // The agent received the prompt: its comments stay delivered, unanswered.
        self.take_turn_delivery(thread_id);
        let mut text = if interrupted {
            format!("The agent turn was interrupted: {failure}")
        } else {
            format!("The agent turn failed: {failure}")
        };
        if !partial.is_empty() {
            text.push_str("\n\nPartial output before the failure:\n");
            text.push_str(&partial);
        }
        self.append_system_notice(thread_id, text).await;
    }

    async fn finish_turn(
        &self,
        thread_id: &ThreadId,
        num_turns: u32,
        cost_usd_micros: Option<u64>,
    ) {
        let (text, session) = {
            let mut inner = self.lock();
            match inner.threads.get_mut(thread_id) {
                Some(thread) => (
                    std::mem::take(&mut thread.acc_text),
                    thread.live_session.clone(),
                ),
                None => return,
            }
        };
        let Some(session) = session else {
            self.release_turn(thread_id);
            return;
        };
        let land_result = subthreads::parse_land_result(&text);

        let assistant = Message::new_assistant(
            thread_id.clone(),
            session.id.clone(),
            vec![ContentBlock::Text { text }],
        );
        let persisted = self.db.append_message(&assistant).await;
        if let Err(err) = &persisted {
            crate::log::warn(format!("failed to persist assistant message: {err}"));
        } else if let Err(err) = self
            .append_event(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                assistant.actor.clone(),
                DomainEvent::MessageAdded {
                    message: assistant.clone(),
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append assistant MessageAdded: {err}"));
        }

        let answer_id = assistant.id.clone();
        self.broadcast(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: assistant,
            },
        );
        if persisted.is_ok() {
            self.link_comment_answers(thread_id, &answer_id).await;
        } else {
            self.take_turn_delivery(thread_id);
        }

        if let Err(err) = self
            .append_event(
                Aggregate::Session {
                    id: session.id.clone(),
                },
                Actor::Agent {
                    session: session.id.clone(),
                },
                DomainEvent::AgentTurnCompleted {
                    thread_id: thread_id.clone(),
                    session_id: session.id.clone(),
                    num_turns,
                    cost_usd_micros,
                },
            )
            .await
        {
            crate::log::warn(format!("failed to append AgentTurnCompleted: {err}"));
        }

        let totals = self
            .db
            .usage_totals_for_thread(thread_id)
            .await
            .unwrap_or_else(|_| UsageReport {
                model: session.model.clone().unwrap_or_default(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            });
        self.broadcast(
            thread_id,
            DaemonMessage::UsageSnapshot {
                thread_id: thread_id.clone(),
                totals,
            },
        );
        self.release_turn(thread_id);
        if let Some((outcome, summary)) = land_result {
            self.record_land_result(thread_id, outcome, summary).await;
        }
    }

    /// The adapter event stream closed. If a turn was still in flight it never
    /// reached a terminal event, so exactly one accurate interruption notice
    /// is persisted — with any partial output — and the thread is freed. When
    /// the stream closes after a completed (or failed) turn, nothing is
    /// appended and only the live session is dropped.
    async fn on_pump_closed(&self, thread_id: &ThreadId) {
        let (in_flight, partial) = {
            let mut inner = self.lock();
            let token = inner
                .threads
                .get_mut(thread_id)
                .and_then(|thread| thread.approval_token.take());
            if let Some(token) = token {
                inner.approval_tokens.remove(&token);
            }
            match inner.threads.get_mut(thread_id) {
                Some(thread) => {
                    thread.live_session = None;
                    thread.session_allowed_tools.clear();
                    thread.session_info = None;
                    if thread.reservation == Some(Reservation::Turn) {
                        thread.reservation = None;
                        (true, std::mem::take(&mut thread.acc_text))
                    } else {
                        (false, String::new())
                    }
                }
                None => (false, String::new()),
            }
        };
        // A hook still waiting would otherwise block until its timeout.
        self.cancel_thread_approvals(
            thread_id,
            "The agent session ended before this approval was answered.",
            false,
        )
        .await;
        if !in_flight {
            return;
        }
        self.take_turn_delivery(thread_id);
        let mut text = "The agent session ended unexpectedly before the turn completed.".to_owned();
        if !partial.is_empty() {
            text.push_str("\n\nPartial output before the session ended:\n");
            text.push_str(&partial);
        }
        self.append_system_notice(thread_id, text).await;
        self.broadcast(
            thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::Failed {
                    message: "The agent session ended unexpectedly.".into(),
                },
            },
        );
    }
}

const MIGRATION_NOTICE: &str = "Migrated this thread onto isolated checkouts. The transcript is retained, but the agent session is fresh and will not resume the previous session. Send a new message when you want the agent to continue; the earlier prompt was not run automatically.";

const MAX_LABEL_LEN: usize = 120;

struct MigrationPlan {
    source: PathBuf,
    attachment: ThreadRepositoryAttachment,
    provision: bool,
    checkout_path: Option<PathBuf>,
    repo_local: bool,
}

fn validate_managed_checkout(
    manager: &CheckoutManager,
    source: &std::path::Path,
    path: &std::path::Path,
    base: &str,
    repo_local: bool,
) -> std::result::Result<(), CheckoutError> {
    if repo_local {
        manager.validate_repo_local(source, path, base)
    } else {
        manager.validate(source, path, base)
    }
}

fn prepare_migration(
    checkout_manager: CheckoutManager,
    thread_id: &ThreadId,
    mut plans: Vec<MigrationPlan>,
) -> std::result::Result<Vec<MigrationPlan>, CheckoutError> {
    for plan in &plans {
        if plan.provision {
            CheckoutManager::assert_clean(&plan.source)?;
        } else {
            let base_revision = plan
                .attachment
                .base_revision
                .as_deref()
                .ok_or_else(|| CheckoutError::Invalid(plan.source.clone()))?;
            let path = plan
                .checkout_path
                .as_ref()
                .ok_or_else(|| CheckoutError::Invalid(plan.source.clone()))?;
            validate_managed_checkout(
                &checkout_manager,
                &plan.source,
                path,
                base_revision,
                plan.repo_local,
            )?;
        }
    }
    for plan in &mut plans {
        if !plan.provision {
            continue;
        }
        let checkout_id = plan
            .attachment
            .checkout_id
            .clone()
            .ok_or_else(|| CheckoutError::Invalid(plan.source.clone()))?;
        let provisioned = checkout_manager.provision_repo_local(
            &plan.source,
            thread_id,
            &plan.attachment.repository_id,
            &checkout_id,
        )?;
        plan.checkout_path = Some(provisioned.path);
        plan.attachment.base_revision = Some(provisioned.base_revision);
        plan.attachment.attach_mode = AttachMode::Isolated;
    }
    Ok(plans)
}

fn checkout_to_invalid(error: CheckoutError) -> DaemonError {
    match error {
        CheckoutError::Dirty { path } => DaemonError::Invalid(format!(
            "repository has uncommitted or untracked non-ignored files at {}; commit or stash them before continuing. Those changes stay in the original working tree and are not copied or deleted",
            path.display()
        )),
        CheckoutError::Missing { path } => DaemonError::Invalid(format!(
            "path does not exist: {}. Choose an existing local Git working tree, not a remote URL. Nothing was changed",
            path.display()
        )),
        CheckoutError::Nested {
            root,
            source_path: source,
        } => DaemonError::Invalid(format!(
            "managed checkout root {} would live inside the source repository at {}. Move the managed checkout root outside that repository (for example out of a dotfiles repo) so provisioning cannot dirty the source working tree. Nothing was changed",
            root.display(),
            source.display()
        )),
        other => DaemonError::Invalid(other.to_string()),
    }
}

fn migration_checkout_error(error: CheckoutError) -> DaemonError {
    match error {
        CheckoutError::Dirty { path } => DaemonError::Invalid(format!(
            "repository has uncommitted or untracked non-ignored files at {}; commit or stash them before migrating. Those changes stay in the original working tree and are not copied or deleted. The thread was not changed",
            path.display()
        )),
        CheckoutError::Missing { path } => DaemonError::Invalid(format!(
            "repository path does not exist: {}. Restore the local Git working tree before migrating. The thread was not changed",
            path.display()
        )),
        CheckoutError::Nested {
            root,
            source_path: source,
        } => DaemonError::Invalid(format!(
            "managed checkout root {} would live inside the source repository at {}. Move the managed checkout root outside that repository (for example out of a dotfiles repo) so migrating cannot dirty the source working tree. The thread was not changed",
            root.display(),
            source.display()
        )),
        other => DaemonError::Invalid(format!(
            "{other}. The thread, its messages, and the original files were left unchanged"
        )),
    }
}

fn validate_label(value: &str, what: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DaemonError::Invalid(format!("{what} must not be empty")));
    }
    if value.chars().count() > MAX_LABEL_LEN {
        return Err(DaemonError::Invalid(format!(
            "{what} must be at most {MAX_LABEL_LEN} characters"
        )));
    }
    if value.chars().any(|ch| ch.is_control()) {
        return Err(DaemonError::Invalid(format!(
            "{what} must not contain control characters"
        )));
    }
    Ok(value.to_owned())
}

fn validate_workspace_name(value: &str) -> Result<String> {
    let value = validate_label(value, "workspace name")?;
    if value.chars().any(|ch| ch == '/' || ch == '\\') {
        return Err(DaemonError::Invalid(
            "workspace name must not contain path separators".into(),
        ));
    }
    Ok(value)
}

fn repository_snapshot(
    repository: &Repository,
    title: &str,
    default_enabled: bool,
) -> RepositorySnapshot {
    RepositorySnapshot {
        id: repository.id.clone(),
        title: title.to_owned(),
        path: repository.local_path.clone().unwrap_or_default(),
        default_enabled,
    }
}

fn usage_report_present(report: &UsageReport) -> bool {
    !report.model.is_empty()
        || report.input_tokens > 0
        || report.output_tokens > 0
        || report.cache_read_tokens > 0
        || report.cache_write_tokens > 0
}

fn same_directory(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn thread_snapshot(thread: &Thread) -> ThreadSnapshot {
    ThreadSnapshot {
        id: thread.id.clone(),
        title: thread.title.clone().unwrap_or_default(),
        repo_count: thread.repo_attachments.len() as u32,
        parent_thread_id: thread.parent_thread_id.clone(),
        kind: thread.kind,
        review_verdict: None,
    }
}

fn agent_kind_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude_code",
        AgentKind::Acp => "acp",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
        AgentKind::Builtin => "builtin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DaemonError;
    use crate::testing::FakeAdapter;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};
    use trama_domain::agent::AgentEvent;
    use trama_domain::ids::ThreadId;
    use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
    use trama_protocol::DaemonMessage;

    async fn seed_workspace(db: &TramaDb, with_repo: bool, root: &std::path::Path) -> WorkspaceId {
        let workspace = Workspace::new("Default");
        let id = workspace.id.clone();
        db.create_workspace(&workspace).await.unwrap();
        if with_repo {
            let source = root.join("source");
            std::fs::create_dir_all(&source).unwrap();
            test_git(&source, &["init", "-b", "main"]);
            std::fs::write(source.join("same.txt"), "source\n").unwrap();
            test_git(&source, &["add", "same.txt"]);
            test_git(&source, &["commit", "-m", "initial"]);
            let repo = Repository::new(None, Some(source), Some("main".into()));
            db.create_repository(&repo).await.unwrap();
            db.add_membership(&WorkspaceMembership {
                workspace_id: id.clone(),
                repository_id: repo.id,
                alias: Some("repo".into()),
                default_enabled: true,
            })
            .await
            .unwrap();
        }
        id
    }

    async fn manager(
        with_repo: bool,
    ) -> (SessionManager, WorkspaceId, FakeAdapter, tempfile::TempDir) {
        let db = TramaDb::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ws = seed_workspace(&db, with_repo, dir.path()).await;
        let fake = FakeAdapter::scripted();
        let mgr = SessionManager::with_checkout_root(
            db,
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        (mgr, ws, fake, dir)
    }

    fn test_git(cwd: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn test_repository(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        test_git(path, &["init", "-b", "main"]);
        std::fs::write(path.join("same.txt"), "source\n").unwrap();
        test_git(path, &["add", "same.txt"]);
        test_git(path, &["commit", "-m", "initial"]);
    }

    #[tokio::test]
    async fn two_threads_receive_isolated_checkouts_for_every_default_repo() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db = TramaDb::open(&db_path).unwrap();
        let workspace = Workspace::new("isolated");
        db.create_workspace(&workspace).await.unwrap();
        let sources = [dir.path().join("source-a"), dir.path().join("source-b")];
        for (name, source) in ["a", "b"].into_iter().zip(&sources) {
            test_repository(source);
            let repository = Repository::new(None, Some(source.clone()), Some("main".into()));
            db.create_repository(&repository).await.unwrap();
            db.add_membership(&WorkspaceMembership {
                workspace_id: workspace.id.clone(),
                repository_id: repository.id,
                alias: Some(name.into()),
                default_enabled: true,
            })
            .await
            .unwrap();
        }

        let fake = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(1));
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        let first = manager
            .create_thread(workspace.id.clone(), "first".into())
            .await
            .unwrap();
        let second = manager
            .create_thread(workspace.id.clone(), "second".into())
            .await
            .unwrap();
        let first_thread = db.get_thread(&first.id).await.unwrap().unwrap();
        assert_eq!(first_thread.repo_attachments.len(), 2);
        assert!(first_thread.repo_attachments.iter().all(|attachment| {
            attachment.checkout_id.is_some() && attachment.base_revision.is_some()
        }));
        let snapshots = manager.thread_checkouts(&first.id).await.unwrap();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots.iter().all(|snapshot| {
            snapshot.status == trama_protocol::CheckoutStatus::Ready
                && snapshot.checkout_path.as_ref().is_some_and(|path| {
                    path.starts_with(&snapshot.source_path.join(".trama/worktrees"))
                })
        }));

        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        manager.subscribe(&first.id, tx1).await.unwrap();
        manager.subscribe(&second.id, tx2).await.unwrap();
        manager
            .send_prompt(first.id.clone(), "edit".into())
            .await
            .unwrap();
        manager
            .send_prompt(second.id.clone(), "edit".into())
            .await
            .unwrap();
        wait_turn_complete(&mut rx1).await;
        wait_turn_complete(&mut rx2).await;

        let configs = fake
            .start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].extra_dirs.len(), 1);
        assert_eq!(configs[1].extra_dirs.len(), 1);
        let sources: Vec<_> = sources
            .iter()
            .map(|source| source.canonicalize().unwrap())
            .collect();
        let first_paths = [&configs[0].cwd, &configs[0].extra_dirs[0]];
        let second_paths = [&configs[1].cwd, &configs[1].extra_dirs[0]];
        assert!(first_paths.iter().all(|path| {
            sources
                .iter()
                .any(|source| path.starts_with(source.join(".trama/worktrees")))
        }));
        assert!(second_paths.iter().all(|path| {
            sources
                .iter()
                .any(|source| path.starts_with(source.join(".trama/worktrees")))
        }));
        assert!(first_paths.iter().all(|path| !sources.contains(path)));
        assert!(second_paths.iter().all(|path| !sources.contains(path)));
        std::fs::write(first_paths[0].join("same.txt"), "first edit\n").unwrap();
        std::fs::write(first_paths[1].join("same.txt"), "first edit repo two\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(second_paths[0].join("same.txt")).unwrap(),
            "source\n"
        );
        assert_eq!(
            std::fs::read_to_string(second_paths[1].join("same.txt")).unwrap(),
            "source\n"
        );
        assert_eq!(
            std::fs::read_to_string(&sources[0].join("same.txt")).unwrap(),
            "source\n"
        );
        assert_eq!(
            std::fs::read_to_string(&sources[1].join("same.txt")).unwrap(),
            "source\n"
        );
        assert_ne!(first_paths[0], second_paths[0]);

        drop(manager);
        let reopened = TramaDb::open(&db_path).unwrap();
        let persisted = reopened.get_thread(&first.id).await.unwrap().unwrap();
        assert_eq!(
            persisted.repo_attachments.len(),
            first_thread.repo_attachments.len()
        );
        for (persisted, original) in persisted
            .repo_attachments
            .iter()
            .zip(&first_thread.repo_attachments)
        {
            assert_eq!(persisted.checkout_id, original.checkout_id);
            assert_eq!(persisted.base_revision, original.base_revision);
        }
        let after_restart = SessionManager::with_checkout_root(
            reopened,
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed"),
        );
        let resumed_config = after_restart.session_config(&first.id).await.unwrap();
        let expected_path = after_restart
            .db
            .checkout_location(
                &first.id,
                &first_thread.repo_attachments[0].repository_id,
                first_thread.repo_attachments[0]
                    .checkout_id
                    .as_ref()
                    .unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed_config.cwd, expected_path);
    }

    #[tokio::test]
    async fn provisioning_and_validation_fail_closed_before_adapter_start() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let workspace = Workspace::new("failure");
        db.create_workspace(&workspace).await.unwrap();
        let invalid_source = dir.path().join("not-a-git-repo");
        std::fs::create_dir_all(&invalid_source).unwrap();
        let invalid_repo = Repository::new(None, Some(invalid_source), None);
        let invalid_repo_id = invalid_repo.id.clone();
        db.create_repository(&invalid_repo).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: invalid_repo_id.clone(),
            alias: None,
            default_enabled: true,
        })
        .await
        .unwrap();
        let fake = FakeAdapter::scripted();
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        assert!(
            manager
                .create_thread(workspace.id.clone(), "cannot clone".into())
                .await
                .is_err()
        );
        assert!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        db.upsert_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: invalid_repo_id,
            alias: None,
            default_enabled: false,
        })
        .await
        .unwrap();

        let valid_source = dir.path().join("valid-repo");
        test_repository(&valid_source);
        let valid_repo = Repository::new(None, Some(valid_source.clone()), Some("main".into()));
        db.create_repository(&valid_repo).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: valid_repo.id.clone(),
            alias: Some("valid".into()),
            default_enabled: true,
        })
        .await
        .unwrap();
        // Deliberately model a historical global checkout. Losing its source
        // must not delete the local work kept outside that source repository.
        let mut valid_thread = Thread::new_for_workspace(
            &workspace,
            &db.list_memberships(&workspace.id).await.unwrap(),
            Some("valid".into()),
        );
        let checkout_id = CheckoutId::generate();
        let provisioned = manager
            .checkout_manager
            .provision(
                &valid_source,
                &valid_thread.id,
                &valid_repo.id,
                &checkout_id,
            )
            .unwrap();
        valid_thread.repo_attachments[0].checkout_id = Some(checkout_id);
        valid_thread.repo_attachments[0].base_revision = Some(provisioned.base_revision);
        db.create_thread(&valid_thread).await.unwrap();
        let attachment = db
            .get_thread(&valid_thread.id)
            .await
            .unwrap()
            .unwrap()
            .repo_attachments
            .remove(0);
        let checkout_path = manager
            .checkout_manager
            .path(
                &valid_thread.id,
                &attachment.repository_id,
                attachment.checkout_id.as_ref().unwrap(),
            )
            .unwrap();
        std::fs::write(checkout_path.join("user-work.txt"), "keep me\n").unwrap();
        let moved_source = dir.path().join("moved-valid-repo");
        std::fs::rename(&valid_source, &moved_source).unwrap();
        std::fs::remove_dir_all(&moved_source).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&checkout_path)
            .args(["remote", "set-url", "local", valid_source.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(manager.session_config(&valid_thread.id).await.is_err());
        assert_eq!(
            std::fs::read_to_string(checkout_path.join("user-work.txt")).unwrap(),
            "keep me\n"
        );
        assert!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn adopted_threads_are_rejected_before_start_with_migration_guidance() {
        use trama_domain::agent::AgentSession;
        use trama_domain::ids::AgentSessionId;
        use trama_domain::thread::{AttachMode, ThreadKind, ThreadRepositoryAttachment};

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("adopted-repo");
        test_repository(&source);
        let db = TramaDb::open_in_memory().unwrap();
        let workspace = Workspace::new("legacy-adopted");
        db.create_workspace(&workspace).await.unwrap();
        let repository = Repository::new(None, Some(source.clone()), Some("main".into()));
        db.create_repository(&repository).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("old adopted thread".into()),
            repo_attachments: vec![ThreadRepositoryAttachment {
                repository_id: repository.id.clone(),
                base_revision: None,
                checkout_id: None,
                attach_mode: AttachMode::Adopted,
            }],
        };
        db.create_thread(&thread).await.unwrap();
        db.create_agent_session(
            &thread.id,
            &AgentSession {
                id: AgentSessionId::generate(),
                external_session_id: Some("old-external-session".into()),
                model: None,
                cwd: source.clone(),
            },
            "claude_code",
        )
        .await
        .unwrap();

        let fake = FakeAdapter::scripted();
        let manager = SessionManager::with_checkout_root(
            db,
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        let error = manager.ensure_session(&thread.id).await.unwrap_err();
        assert!(error.to_string().contains("adopted"));
        assert!(error.to_string().contains("migrate_legacy_thread"));
        assert!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    fn agent_event(msg: &DaemonMessage) -> Option<&AgentEvent> {
        match msg {
            DaemonMessage::AgentEventEnvelope { event, .. } => Some(event),
            _ => None,
        }
    }

    async fn wait_turn_complete(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) {
        let deadline = Duration::from_secs(10);
        loop {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
            if matches!(
                msg,
                DaemonMessage::UsageSnapshot { .. }
                    | DaemonMessage::AgentEventEnvelope {
                        event: AgentEvent::Failed { .. },
                        ..
                    }
            ) {
                return;
            }
        }
    }

    #[tokio::test]
    async fn create_and_list_threads() {
        let (mgr, ws, _, _dir) = manager(true).await;
        assert_eq!(mgr.list_workspaces().await.unwrap().len(), 1);
        let created = mgr.create_thread(ws.clone(), "First".into()).await.unwrap();
        assert_eq!(created.title, "First");
        let listed = mgr.list_threads(&ws).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        let missing = WorkspaceId::from("nope");
        assert!(mgr.list_threads(&missing).await.is_err());
    }

    #[tokio::test]
    async fn send_prompt_emits_scripted_events() {
        let (mgr, ws, _, _dir) = manager(true).await;
        let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "hi".into())
            .await
            .unwrap();

        let mut saw_delta = false;
        let mut saw_usage = false;
        let mut saw_turn = false;
        let mut saw_snapshot = false;
        let deadline = Duration::from_secs(10);
        while !(saw_delta && saw_usage && saw_turn && saw_snapshot) {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
            match msg {
                DaemonMessage::MessageAppended { .. } => {}
                DaemonMessage::AgentEventEnvelope { event, .. } => match event {
                    AgentEvent::TextDelta { .. } => saw_delta = true,
                    AgentEvent::UsageReported { .. } => saw_usage = true,
                    AgentEvent::TurnCompleted { .. } => saw_turn = true,
                    _ => {}
                },
                DaemonMessage::UsageSnapshot { .. } => saw_snapshot = true,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn interrupt_aborts_mock_turn() {
        let (mgr, ws, _, _dir) = manager(true).await;
        let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "hi".into())
            .await
            .unwrap();

        let _first = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(mgr.interrupt(&thread.id).await.unwrap());

        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut rest = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            rest.push(msg);
        }
        let completed = rest
            .iter()
            .any(|m| matches!(agent_event(m), Some(AgentEvent::TurnCompleted { .. })));
        assert!(
            !completed,
            "interrupt should drop the mock task before TurnCompleted"
        );
    }

    #[tokio::test]
    async fn subscribe_unknown_thread_fails() {
        let (mgr, _, _, _dir) = manager(false).await;
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = mgr
            .subscribe(&ThreadId::from("missing"), tx)
            .await
            .unwrap_err();
        assert!(matches!(err, DaemonError::NotFound(_)));
    }

    #[tokio::test]
    async fn send_prompt_persists_messages_usage_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db = TramaDb::open(&db_path).unwrap();
        let ws = seed_workspace(&db, true, dir.path()).await;
        let fake = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(5));
        let checkout_root = dir.path().join("managed");
        let mgr = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            checkout_root.clone(),
        );
        let thread = mgr.create_thread(ws, "persist".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "hello world".into())
            .await
            .unwrap();
        wait_turn_complete(&mut rx).await;

        let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "hello world");
        assert_eq!(messages[1].text(), "Hello from the mock agent.");
        let totals = db.usage_totals_for_thread(&thread.id).await.unwrap();
        assert_eq!(totals.input_tokens, 16);
        assert_eq!(totals.output_tokens, 8);
        let stored = db
            .latest_agent_session_for_thread(&thread.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.external_session_id.as_deref(), Some("fake-ext-1"));

        drop(mgr);
        let fake2 = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(5));
        let reopened = TramaDb::open(&db_path).unwrap();
        let mgr2 =
            SessionManager::with_checkout_root(reopened, Arc::new(fake2.clone()), checkout_root);
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        mgr2.subscribe(&thread.id, tx2).await.unwrap();
        let replayed: Vec<_> = {
            let mut out = Vec::new();
            while let Ok(msg) = rx2.try_recv() {
                out.push(msg);
            }
            out
        };
        let replayed_texts: Vec<_> = replayed
            .iter()
            .filter_map(|m| match m {
                DaemonMessage::MessageAppended { message, .. } => Some(message.text()),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed_texts,
            vec![
                "hello world".to_string(),
                "Hello from the mock agent.".to_string()
            ]
        );
        assert!(replayed.iter().any(|message| matches!(
            message,
            DaemonMessage::UsageSnapshot { totals, .. } if totals.input_tokens == 16
        )));

        mgr2.send_prompt(thread.id.clone(), "second".into())
            .await
            .unwrap();
        wait_turn_complete(&mut rx2).await;
        let configs = fake2
            .start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(configs.len(), 1);
        assert_eq!(
            configs[0].resume_external_session.as_deref(),
            Some("fake-ext-1")
        );
        let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].text(), "second");
    }

    #[tokio::test]
    async fn send_prompt_rejects_when_busy() {
        let (mgr, ws, _, _dir) = manager(true).await;
        let thread = mgr.create_thread(ws, "busy".into()).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "one".into())
            .await
            .unwrap();
        let err = mgr.send_prompt(thread.id, "two".into()).await.unwrap_err();
        assert!(matches!(err, DaemonError::Busy));
    }

    #[tokio::test]
    async fn send_prompt_reports_missing_claude() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let ws = seed_workspace(&db, true, dir.path()).await;
        let mgr = SessionManager::with_checkout_root(
            db,
            Arc::new(FakeAdapter::fail_start()),
            dir.path().join("managed"),
        );
        let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id, "hi".into()).await.unwrap();
        let deadline = Duration::from_secs(10);
        let mut saw_error = false;
        let mut saw_persisted_failure = false;
        for _ in 0..8 {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
            if let DaemonMessage::MessageAppended { message, .. } = &msg {
                if message.role == Role::System && message.text().contains("did not run") {
                    assert!(
                        message.text().contains("CLAUDE_BIN") || message.text().contains("Claude"),
                        "{}",
                        message.text()
                    );
                    saw_persisted_failure = true;
                }
            }
            if let DaemonMessage::Error { code, message } = msg {
                assert_eq!(code, "agent");
                assert!(
                    message.contains("CLAUDE_BIN") || message.contains("Claude"),
                    "{message}"
                );
                saw_error = true;
                break;
            }
        }
        assert!(saw_error, "expected agent error envelope");
        assert!(
            saw_persisted_failure,
            "turn failure must be stored as a system message"
        );
    }

    #[tokio::test]
    async fn subscribe_replays_history() {
        let (mgr, ws, _, _dir) = manager(true).await;
        let thread = mgr.create_thread(ws, "hist".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "hi".into())
            .await
            .unwrap();
        wait_turn_complete(&mut rx).await;

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx2).await.unwrap();
        let first = timeout(Duration::from_secs(2), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(2), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        match (first, second) {
            (
                DaemonMessage::MessageAppended { message: a, .. },
                DaemonMessage::MessageAppended { message: b, .. },
            ) => {
                assert_eq!(a.text(), "hi");
                assert_eq!(b.text(), "Hello from the mock agent.");
            }
            other => panic!("expected two appended messages, got {other:?}"),
        }
    }

    async fn insert_legacy(
        db: &TramaDb,
        root: &std::path::Path,
        names: &[&str],
        mode: AttachMode,
    ) -> (WorkspaceId, Thread, Vec<std::path::PathBuf>) {
        let workspace = Workspace::new("legacy");
        db.create_workspace(&workspace).await.unwrap();
        let mut sources = Vec::new();
        let mut attachments = Vec::new();
        for name in names {
            let source = root.join(name);
            test_repository(&source);
            let repository = Repository::new(None, Some(source.clone()), Some("main".into()));
            db.create_repository(&repository).await.unwrap();
            db.add_membership(&WorkspaceMembership {
                workspace_id: workspace.id.clone(),
                repository_id: repository.id.clone(),
                alias: Some((*name).into()),
                default_enabled: true,
            })
            .await
            .unwrap();
            attachments.push(ThreadRepositoryAttachment {
                repository_id: repository.id,
                base_revision: None,
                checkout_id: None,
                attach_mode: mode,
            });
            sources.push(source);
        }
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: trama_domain::thread::ThreadKind::Top,
            title: Some("legacy".into()),
            repo_attachments: attachments,
        };
        db.create_thread(&thread).await.unwrap();
        db.append_message(&Message::new_user(
            thread.id.clone(),
            "user",
            "please fix the bug",
        ))
        .await
        .unwrap();
        db.append_message(&Message::new_assistant(
            thread.id.clone(),
            trama_domain::ids::AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "I patched it".into(),
            }],
        ))
        .await
        .unwrap();
        db.create_agent_session(
            &thread.id,
            &trama_domain::agent::AgentSession {
                id: trama_domain::ids::AgentSessionId::generate(),
                external_session_id: Some("old-claude-session".into()),
                model: Some("old-model".into()),
                cwd: sources[0].clone(),
            },
            "claude_code",
        )
        .await
        .unwrap();
        (workspace.id, thread, sources)
    }

    fn checkout_ids(thread: &Thread) -> Vec<Option<CheckoutId>> {
        thread
            .repo_attachments
            .iter()
            .map(|attachment| attachment.checkout_id.clone())
            .collect()
    }

    #[tokio::test]
    async fn legacy_migration_retains_transcript_and_does_not_resume() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db = TramaDb::open(&db_path).unwrap();
        let (_workspace, thread, sources) =
            insert_legacy(&db, dir.path(), &["app", "web"], AttachMode::Isolated).await;
        let fake = FakeAdapter::scripted();
        let managed = dir.path().join("managed");
        let manager =
            SessionManager::with_checkout_root(db.clone(), Arc::new(fake.clone()), managed.clone());

        let rejected = manager
            .send_prompt(thread.id.clone(), "should not run".into())
            .await
            .unwrap_err();
        assert!(rejected.to_string().contains("migrate_legacy_thread"));
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            2
        );
        assert!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );

        let migrated = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap();
        assert_eq!(migrated.id, thread.id);
        let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
        let ids = checkout_ids(&stored);
        assert!(ids.iter().all(|id| id.is_some()));
        assert!(stored.repo_attachments.iter().all(|attachment| {
            attachment.attach_mode == AttachMode::Isolated && attachment.base_revision.is_some()
        }));
        let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(messages[0].text(), "please fix the bug");
        assert_eq!(messages[1].text(), "I patched it");
        assert_eq!(messages[2].role, Role::System);
        assert!(messages[2].text().contains("transcript is retained"));
        assert!(messages[2].text().contains("fresh"));
        for source in &sources {
            assert_eq!(
                std::fs::read_to_string(source.join("same.txt")).unwrap(),
                "source\n"
            );
        }

        let again = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap();
        assert_eq!(again.id, thread.id);
        assert_eq!(
            checkout_ids(&db.get_thread(&thread.id).await.unwrap().unwrap()),
            ids
        );
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            3
        );

        drop(manager);
        drop(db);
        let reopened = TramaDb::open(&db_path).unwrap();
        let persisted = reopened.get_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(checkout_ids(&persisted), ids);
        assert_eq!(
            reopened
                .list_messages_by_thread(&thread.id)
                .await
                .unwrap()
                .len(),
            3
        );
        let fake2 = FakeAdapter::scripted();
        let manager2 = SessionManager::with_checkout_root(
            reopened.clone(),
            Arc::new(fake2.clone()),
            managed.clone(),
        );
        manager2
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .list_messages_by_thread(&thread.id)
                .await
                .unwrap()
                .len(),
            3
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager2.subscribe(&thread.id, tx).await.unwrap();
        manager2
            .send_prompt(thread.id.clone(), "continue".into())
            .await
            .unwrap();
        wait_turn_complete(&mut rx).await;
        let configs = fake2
            .start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(configs.len(), 1);
        assert!(configs[0].resume_external_session.is_none());
        assert!(sources.iter().any(|source| {
            configs[0]
                .cwd
                .starts_with(source.canonicalize().unwrap().join(".trama/worktrees"))
        }));
        assert_eq!(configs[0].extra_dirs.len(), 1);
        assert!(sources.iter().any(|source| {
            configs[0].extra_dirs[0]
                .starts_with(source.canonicalize().unwrap().join(".trama/worktrees"))
        }));
        assert!(
            sources
                .iter()
                .all(|source| { &configs[0].cwd != source && &configs[0].extra_dirs[0] != source })
        );
    }

    #[tokio::test]
    async fn migration_failure_leaves_attachments_and_does_not_start_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let (_workspace, thread, sources) =
            insert_legacy(&db, dir.path(), &["one", "two"], AttachMode::Isolated).await;
        let fake = FakeAdapter::scripted();
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        std::fs::write(sources[1].join("dirty.txt"), "wip\n").unwrap();
        let dirty = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert!(dirty.to_string().contains("commit or stash"), "{dirty}");
        assert!(
            db.get_thread(&thread.id)
                .await
                .unwrap()
                .unwrap()
                .repo_attachments
                .iter()
                .all(|attachment| attachment.checkout_id.is_none())
        );
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(sources[1].join("dirty.txt")).unwrap(),
            "wip\n"
        );

        std::fs::remove_file(sources[1].join("dirty.txt")).unwrap();
        let blocked = sources[1].join(".trama");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(
            blocked.join("foreign.txt"),
            "foreign content: do not replace\n",
        )
        .unwrap();
        std::fs::write(sources[1].join(".git/info/exclude"), "/.trama/\n").unwrap();
        let failed = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert!(failed.to_string().contains("unchanged"), "{failed}");
        assert!(
            db.get_thread(&thread.id)
                .await
                .unwrap()
                .unwrap()
                .repo_attachments
                .iter()
                .all(|attachment| {
                    attachment.checkout_id.is_none() && attachment.base_revision.is_none()
                })
        );
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            2
        );
        assert!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(sources[0].join("same.txt")).unwrap(),
            "source\n"
        );

        std::fs::remove_dir_all(&sources[0]).unwrap();
        std::fs::remove_dir_all(blocked).unwrap();
        let missing = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert!(
            missing.to_string().contains("missing")
                || missing.to_string().contains("does not exist"),
            "{missing}"
        );
        assert!(
            db.get_thread(&thread.id)
                .await
                .unwrap()
                .unwrap()
                .repo_attachments
                .iter()
                .all(|attachment| attachment.checkout_id.is_none())
        );
    }

    #[tokio::test]
    async fn busy_or_live_thread_is_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let (_workspace, thread, _sources) =
            insert_legacy(&db, dir.path(), &["app"], AttachMode::Isolated).await;
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed"),
        );
        {
            let mut inner = manager.lock();
            inner
                .threads
                .entry(thread.id.clone())
                .or_default()
                .reservation = Some(Reservation::Turn);
        }
        let err = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "busy");
        {
            let mut inner = manager.lock();
            let runtime = inner.threads.get_mut(&thread.id).unwrap();
            runtime.reservation = None;
            runtime.live_session = Some(trama_domain::agent::AgentSession {
                id: trama_domain::ids::AgentSessionId::generate(),
                external_session_id: Some("live".into()),
                model: None,
                cwd: dir.path().join("app"),
            });
        }
        let err = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "busy");
        assert!(
            db.get_thread(&thread.id)
                .await
                .unwrap()
                .unwrap()
                .repo_attachments[0]
                .checkout_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn adopted_legacy_thread_migrates_to_isolated_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let (_workspace, thread, sources) =
            insert_legacy(&db, dir.path(), &["adopted"], AttachMode::Adopted).await;
        let fake = FakeAdapter::scripted();
        let managed = dir.path().join("managed");
        let manager =
            SessionManager::with_checkout_root(db.clone(), Arc::new(fake.clone()), managed.clone());
        manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap();
        let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(stored.repo_attachments[0].attach_mode, AttachMode::Isolated);
        assert!(stored.repo_attachments[0].checkout_id.is_some());
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager.subscribe(&thread.id, tx).await.unwrap();
        manager.send_prompt(thread.id, "go".into()).await.unwrap();
        wait_turn_complete(&mut rx).await;
        let configs = fake
            .start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(configs.len(), 1);
        assert!(configs[0].resume_external_session.is_none());
        assert!(
            configs[0]
                .cwd
                .starts_with(sources[0].canonicalize().unwrap().join(".trama/worktrees"))
        );
        assert_ne!(configs[0].cwd, sources[0]);
        assert_eq!(
            std::fs::read_to_string(sources[0].join("same.txt")).unwrap(),
            "source\n"
        );
    }

    #[tokio::test]
    async fn create_workspace_and_add_repository_round_trip_without_config() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open(&dir.path().join("trama.db")).unwrap();
        crate::config::seed_from_config(&db, &crate::config::FileConfig::default())
            .await
            .unwrap();
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed"),
        );
        assert!(manager.create_workspace("  ".into()).await.is_err());
        let created = manager.create_workspace("Demo".into()).await.unwrap();
        assert_eq!(created.repo_count, 0);
        assert!(manager.create_workspace("Demo".into()).await.is_err());
        let empty = manager
            .create_thread(created.id.clone(), "no repos".into())
            .await
            .unwrap_err();
        assert!(empty.to_string().contains("Add a local Git repository"));
        assert!(manager.list_threads(&created.id).await.unwrap().is_empty());

        let source = dir.path().join("app");
        test_repository(&source);
        let child = source.join("src");
        std::fs::create_dir_all(&child).unwrap();
        assert!(
            manager
                .add_repository(created.id.clone(), child, "child".into())
                .await
                .is_err()
        );
        assert!(
            manager
                .add_repository(
                    created.id.clone(),
                    dir.path().to_path_buf(),
                    "parent".into()
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .add_repository(
                    created.id.clone(),
                    dir.path().join("missing"),
                    "missing".into(),
                )
                .await
                .is_err()
        );
        let added = manager
            .add_repository(created.id.clone(), source.clone(), "app".into())
            .await
            .unwrap();
        assert!(added.default_enabled);
        assert_eq!(added.title, "app");
        let again = manager
            .add_repository(created.id.clone(), source, "app".into())
            .await
            .unwrap();
        assert_eq!(again.id, added.id);
        let listed = manager.list_repositories(&created.id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, added.id);
        let workspaces = manager.list_workspaces().await.unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].repo_count, 1);

        crate::config::seed_from_config(&db, &crate::config::FileConfig::default())
            .await
            .unwrap();
        assert_eq!(db.list_workspaces().await.unwrap().len(), 1);
        assert_eq!(db.list_memberships(&created.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_migrations_are_serialized_one_notice_no_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let (_workspace, thread, _sources) =
            insert_legacy(&db, dir.path(), &["app"], AttachMode::Isolated).await;
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed"),
        );

        let (first, second) = tokio::join!(
            manager.migrate_legacy_thread(thread.id.clone()),
            manager.migrate_legacy_thread(thread.id.clone())
        );
        let results = [first, second];
        assert_eq!(
            results.iter().filter(|result| result.is_ok()).count(),
            1,
            "exactly one migration may run: {:?}",
            results
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(err) if err.code() == "busy"))
                .count(),
            1,
            "the losing migration must be rejected as busy"
        );

        // One transcript notice, one set of published checkouts.
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            3
        );
        let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
        let ids = checkout_ids(&stored);
        assert!(ids.iter().all(|id| id.is_some()));

        // A later migration validates and keeps the same targets.
        manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap();
        assert_eq!(
            checkout_ids(&db.get_thread(&thread.id).await.unwrap().unwrap()),
            ids
        );
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            3
        );
    }

    #[tokio::test]
    async fn send_prompt_is_rejected_for_the_whole_migration_window() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let (_workspace, thread, _sources) =
            insert_legacy(&db, dir.path(), &["app"], AttachMode::Isolated).await;
        let fake = FakeAdapter::scripted();
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        // Simulate a migration that has reserved the thread and is inside its
        // clone/persist window.
        {
            let mut inner = manager.lock();
            inner
                .threads
                .entry(thread.id.clone())
                .or_default()
                .reservation = Some(Reservation::Migration);
        }
        let err = manager
            .send_prompt(thread.id.clone(), "must not run".into())
            .await
            .unwrap_err();
        assert!(matches!(err, DaemonError::Busy), "{err}");
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            2
        );
        assert!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn migration_is_rejected_while_a_turn_is_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let ws = seed_workspace(&db, true, dir.path()).await;
        let fake = FakeAdapter::new(
            crate::testing::scripted_events(),
            Duration::from_millis(150),
        );
        let mgr = SessionManager::with_checkout_root(
            db,
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        let thread = mgr.create_thread(ws, "in flight".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        // send_prompt returns once the turn is reserved and spawned; the
        // scripted delay keeps it running well past the migration attempt.
        mgr.send_prompt(thread.id.clone(), "long turn".into())
            .await
            .unwrap();
        let err = mgr
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "busy");
        // The in-flight turn still completes on its original checkouts.
        wait_turn_complete(&mut rx).await;
        assert_eq!(
            fake.start_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_failed_event_persists_failure_with_partial_output_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db = TramaDb::open(&db_path).unwrap();
        let ws = seed_workspace(&db, true, dir.path()).await;
        let failing = vec![
            AgentEvent::SessionStarted {
                external_session_id: Some("fake-ext-f1".into()),
                model: Some("fake".into()),
            },
            AgentEvent::TextDelta {
                text: "partial output before the crash".into(),
            },
            AgentEvent::Failed {
                message: "model overloaded".into(),
            },
        ];
        let fake = FakeAdapter::new(failing, Duration::from_millis(5));
        let managed = dir.path().join("managed");
        let mgr =
            SessionManager::with_checkout_root(db.clone(), Arc::new(fake.clone()), managed.clone());
        let thread = mgr.create_thread(ws, "fails".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "hello".into())
            .await
            .unwrap();

        let deadline = Duration::from_secs(10);
        let mut notice = None;
        while notice.is_none() {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
            if let DaemonMessage::MessageAppended { message, .. } = &msg {
                if message.role == Role::System {
                    notice = Some(message.text());
                }
            }
        }
        let notice = notice.unwrap();
        assert!(notice.contains("turn failed"), "{notice}");
        assert!(notice.contains("model overloaded"), "{notice}");
        assert!(
            notice.contains("partial output before the crash"),
            "{notice}"
        );

        let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "hello");
        assert_eq!(messages[1].text(), notice);

        // The thread is free again after the failure.
        {
            let inner = mgr.lock();
            assert_eq!(
                inner.threads.get(&thread.id).unwrap().reservation,
                None,
                "failure must release the turn reservation"
            );
        }

        // Replay after a restart still shows the error.
        drop(mgr);
        drop(db);
        let reopened = TramaDb::open(&db_path).unwrap();
        let mgr2 = SessionManager::with_checkout_root(
            reopened.clone(),
            Arc::new(FakeAdapter::scripted()),
            managed,
        );
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        mgr2.subscribe(&thread.id, tx2).await.unwrap();
        let mut replayed_failure = false;
        while let Ok(msg) = rx2.try_recv() {
            if let DaemonMessage::MessageAppended { message, .. } = msg {
                if message.text().contains("model overloaded") {
                    replayed_failure = true;
                }
            }
        }
        assert!(replayed_failure, "failure notice must replay after restart");
    }

    #[tokio::test]
    async fn failed_event_after_completed_turn_appends_nothing() {
        let (mgr, ws, _, _dir) = manager(true).await;
        let thread = mgr.create_thread(ws, "late failure".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "ok turn".into())
            .await
            .unwrap();
        wait_turn_complete(&mut rx).await;
        // The turn completed and released the thread; a duplicate terminal
        // failure for the same turn must not persist anything.
        mgr.on_agent_event(
            &thread.id,
            AgentEvent::Failed {
                message: "late failure after success".into(),
            },
        )
        .await;
        let messages = mgr.db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages.iter().all(|message| message.role != Role::System),
            "no failure notice may follow a completed turn"
        );
    }

    #[tokio::test]
    async fn unexpected_pump_closure_persists_interruption_and_frees_thread() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let ws = seed_workspace(&db, true, dir.path()).await;
        // Deltas only: the stream never reaches a terminal event.
        let hanging = vec![
            AgentEvent::SessionStarted {
                external_session_id: None,
                model: Some("fake".into()),
            },
            AgentEvent::TextDelta {
                text: "work in progress ".into(),
            },
        ];
        let fake = FakeAdapter::new(hanging, Duration::from_millis(5));
        let mgr = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        let thread = mgr.create_thread(ws, "hangs".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "hello".into())
            .await
            .unwrap();

        let deadline = Duration::from_secs(10);
        loop {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
            if matches!(
                &msg,
                DaemonMessage::AgentEventEnvelope {
                    event: AgentEvent::TextDelta { .. },
                    ..
                }
            ) {
                break;
            }
        }
        // The adapter process exits without a terminal event.
        fake.close_all_event_streams();

        let mut notice = None;
        while notice.is_none() {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
            if let DaemonMessage::MessageAppended { message, .. } = &msg {
                if message.role == Role::System {
                    notice = Some(message.text());
                }
            }
        }
        let notice = notice.unwrap();
        assert!(notice.contains("ended unexpectedly"), "{notice}");
        assert!(notice.contains("work in progress "), "{notice}");

        let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), notice);
        // Busy was cleared: a new prompt is accepted rather than Busy.
        mgr.send_prompt(thread.id.clone(), "retry".into())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mixed_migration_keeps_managed_edits_and_invalidates_resume() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let workspace = Workspace::new("mixed");
        db.create_workspace(&workspace).await.unwrap();
        // Explicit ids so the persisted attachment order (repository id
        // ascending) is deterministic: alpha is the cwd.
        let alpha_id = trama_domain::ids::RepositoryId::from("repo-alpha");
        let beta_id = trama_domain::ids::RepositoryId::from("repo-beta");
        let sources = [dir.path().join("alpha"), dir.path().join("beta")];
        for source in &sources {
            test_repository(source);
        }
        let thread_id = ThreadId::generate();
        let checkout_alpha = CheckoutId::generate();
        let provisioner = trama_infra::checkout::CheckoutManager::new(dir.path().join("managed"));
        let managed_checkout = provisioner
            .provision(&sources[0], &thread_id, &alpha_id, &checkout_alpha)
            .unwrap();
        let thread = Thread {
            id: thread_id,
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: trama_domain::thread::ThreadKind::Top,
            title: Some("mixed".into()),
            repo_attachments: vec![
                ThreadRepositoryAttachment {
                    repository_id: alpha_id.clone(),
                    base_revision: Some(managed_checkout.base_revision.clone()),
                    checkout_id: Some(checkout_alpha.clone()),
                    attach_mode: AttachMode::Isolated,
                },
                ThreadRepositoryAttachment {
                    repository_id: beta_id.clone(),
                    base_revision: None,
                    checkout_id: None,
                    attach_mode: AttachMode::Adopted,
                },
            ],
        };
        for (repository_id, source) in [(&alpha_id, &sources[0]), (&beta_id, &sources[1])] {
            let repository = Repository {
                id: repository_id.clone(),
                canonical_remote: None,
                local_path: Some(source.clone()),
                default_branch: Some("main".into()),
            };
            db.create_repository(&repository).await.unwrap();
            db.add_membership(&WorkspaceMembership {
                workspace_id: workspace.id.clone(),
                repository_id: repository_id.clone(),
                alias: None,
                default_enabled: true,
            })
            .await
            .unwrap();
        }
        db.create_thread(&thread).await.unwrap();
        // The managed checkout already carries user work that must survive.
        std::fs::write(
            managed_checkout.path.join("same.txt"),
            "edited in managed checkout\n",
        )
        .unwrap();
        // A pre-migration external session whose cwd matches the unchanged
        // managed checkout: without invalidation it would be resumed.
        db.create_agent_session(
            &thread.id,
            &trama_domain::agent::AgentSession {
                id: trama_domain::ids::AgentSessionId::generate(),
                external_session_id: Some("pre-migration-session".into()),
                model: Some("old-model".into()),
                cwd: managed_checkout.path.clone(),
            },
            "claude_code",
        )
        .await
        .unwrap();

        let fake = FakeAdapter::scripted();
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(fake.clone()),
            dir.path().join("managed"),
        );
        manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap();

        let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(stored.repo_attachments.len(), 2);
        assert!(stored.repo_attachments.iter().all(|attachment| {
            attachment.attach_mode == AttachMode::Isolated && attachment.checkout_id.is_some()
        }));
        assert_eq!(stored.repo_attachments[0].repository_id, alpha_id);
        assert_eq!(
            stored.repo_attachments[0].checkout_id,
            Some(checkout_alpha.clone()),
            "the managed target must not be replaced"
        );
        // Managed edit retained in place.
        assert_eq!(
            std::fs::read_to_string(managed_checkout.path.join("same.txt")).unwrap(),
            "edited in managed checkout\n"
        );
        let config = manager.session_config(&thread.id).await.unwrap();
        assert_eq!(config.cwd, managed_checkout.path);
        assert!(
            config.resume_external_session.is_none(),
            "migration must invalidate the pre-migration external session"
        );
        assert_eq!(config.extra_dirs.len(), 1);
        assert_ne!(config.extra_dirs[0], sources[1]);
        assert_eq!(
            std::fs::read_to_string(sources[0].join("same.txt")).unwrap(),
            "source\n"
        );
        assert_eq!(
            std::fs::read_to_string(sources[1].join("same.txt")).unwrap(),
            "source\n"
        );
    }

    #[tokio::test]
    async fn migration_without_attachments_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        let workspace = Workspace::new("empty");
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: trama_domain::thread::ThreadKind::Top,
            title: Some("no repos".into()),
            repo_attachments: vec![],
        };
        db.create_thread(&thread).await.unwrap();
        let manager = SessionManager::with_checkout_root(
            db.clone(),
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed"),
        );
        let err = manager
            .migrate_legacy_thread(thread.id.clone())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("no repository attachments"),
            "{err}"
        );
        assert_eq!(
            db.list_messages_by_thread(&thread.id).await.unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn concurrent_workspace_creation_yields_one_row_per_name() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("trama.db");
        let db_a = TramaDb::open(&db_path).unwrap();
        let db_b = TramaDb::open(&db_path).unwrap();
        let manager_a = SessionManager::with_checkout_root(
            db_a.clone(),
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed-a"),
        );
        let manager_b = SessionManager::with_checkout_root(
            db_b.clone(),
            Arc::new(FakeAdapter::scripted()),
            dir.path().join("managed-b"),
        );
        let (a, b, c, d) = tokio::join!(
            manager_a.create_workspace("Race".into()),
            manager_b.create_workspace("Race".into()),
            manager_a.create_workspace("Race".into()),
            manager_b.create_workspace("Race".into())
        );
        let outcomes = [&a, &b, &c, &d];
        assert_eq!(
            outcomes.iter().filter(|result| result.is_ok()).count(),
            1,
            "exactly one creator may win: {a:?} {b:?} {c:?} {d:?}"
        );
        let names: Vec<_> = db_a
            .list_workspaces()
            .await
            .unwrap()
            .into_iter()
            .map(|workspace| workspace.name)
            .collect();
        assert_eq!(names.iter().filter(|name| *name == "Race").count(), 1);
        assert_eq!(db_b.list_workspaces().await.unwrap().len(), 1);
    }
}
