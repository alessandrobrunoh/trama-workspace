//! Persistent session manager: TramaDB + injected `AgentAdapter`.

mod approvals;
mod subscriptions;
mod support;
#[cfg(test)]
mod tests;
mod turns;
mod workspaces;
#[allow(unused_imports)]
pub(crate) use self::approvals::*;
#[allow(unused_imports)]
pub(crate) use self::subscriptions::*;
#[allow(unused_imports)]
pub(crate) use self::support::*;
#[allow(unused_imports)]
pub(crate) use self::turns::*;
#[allow(unused_imports)]
pub(crate) use self::workspaces::*;

mod activity;
#[cfg(test)]
mod activity_tests;
mod agents;
#[cfg(test)]
mod agents_tests;
mod archive;
#[cfg(test)]
mod archive_tests;
mod comments;
// Unix-only: POSIX shell terminals, symlinks and executable scripts.
#[cfg(all(test, unix))]
mod composer_tests;
mod context;
mod forks;
#[cfg(test)]
mod forks_tests;
#[cfg(all(test, unix))]
mod setup_tests;
mod skills;
mod subthreads;
mod terminal;

pub(crate) use context::model_choices;
pub use context::{NoCollaborators, PeopleSource, Person, PromptContext};

pub use agents::SubagentSettings;
pub use archive::RemovalOutcome;

use crate::approval::{self, ApprovalHookSettings, HookDecision};

/// Model passed to `claude --model` unless the workspace sets `default_model`.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-5-5";
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

/// What the collab layer learns from a thread's broadcasts, so a shared
/// thread's family publishes new messages, children, comments and subagent
/// states (ADR 0013). Sent for every thread; the collab layer filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CollabSignal {
    /// Something publishable may have changed in this thread.
    Changed(ThreadId),
    /// The task state of this subagent thread changed.
    SubagentStatus(ThreadId),
}

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
    /// Transcript uuid of the in-flight turn's last assistant entry.
    last_claude_uuid: Option<String>,
    /// Tool calls and thinking of the in-flight turn, stored with the
    /// message that ends it.
    activity: trama_domain::activity::ActivityLog,
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
    /// Mentions, skills, model selection and terminals.
    composer: Arc<context::ComposerState>,
    /// Subagent limits, the Trama MCP server, and where transcripts live.
    agents_config: Arc<agents::AgentsConfig>,
    /// Subagent tasks, MCP tokens, waiters and agent-message mailboxes.
    agents: Arc<agents::AgentsRuntime>,
    /// Where [`CollabSignal`]s go (the collab service), if anywhere.
    collab_observer: Arc<Mutex<Option<mpsc::UnboundedSender<CollabSignal>>>>,
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
        let root = root.into();
        Self {
            db,
            adapter,
            agents_config: Arc::new(agents::AgentsConfig::new(root.clone())),
            agents: Arc::default(),
            checkout_manager: trama_infra::checkout::CheckoutManager::new(root),
            inner: Arc::new(Mutex::new(Inner {
                next_sub: 1,
                threads: HashMap::new(),
                approval_tokens: HashMap::new(),
                pending_approvals: HashMap::new(),
            })),
            billing: BillingMode::default(),
            approval_hook: None,
            composer: Arc::new(context::ComposerState::new()),
            collab_observer: Arc::default(),
        }
    }

    /// Route [`CollabSignal`]s to `tx` (replacing any previous observer).
    pub(crate) fn set_collab_observer(&self, tx: Option<mpsc::UnboundedSender<CollabSignal>>) {
        *self
            .collab_observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tx;
    }

    fn observe(&self, thread_id: &ThreadId, message: &DaemonMessage) {
        let signal = match message {
            DaemonMessage::MessageAppended { .. }
            | DaemonMessage::SubthreadCreated(_)
            | DaemonMessage::ReviewVerdictPosted { .. }
            | DaemonMessage::CommentCreated(_) => CollabSignal::Changed(thread_id.clone()),
            DaemonMessage::SubagentStatusChanged {
                thread_id: child, ..
            } if child == thread_id => CollabSignal::SubagentStatus(child.clone()),
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TurnCompleted { .. },
                ..
            } => CollabSignal::Changed(thread_id.clone()),
            _ => return,
        };
        if let Some(tx) = self
            .collab_observer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(signal);
        }
    }

    /// How new checkouts are copied (`config.json` `checkouts`).
    pub fn with_checkout_copy(mut self, copy: trama_infra::checkout::CopyOptions) -> Self {
        self.checkout_manager = self.checkout_manager.clone().with_copy_options(copy);
        self
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
}
