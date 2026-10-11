//! Persistent session manager: TramaDB + injected `AgentAdapter`.

use crate::config::permission_mode_from_settings;
use crate::error::{DaemonError, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc;
use trama_db::TramaDb;
use trama_domain::agent::{
    AgentAdapter, AgentEvent, AgentKind, AgentPrompt, AgentSession, ApprovalDecision,
    SessionConfig, UsageReport,
};
use trama_domain::event::{Aggregate, DomainEvent, EventRecord, EventStore};
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_domain::message::{Actor, ContentBlock, Message};
use trama_domain::thread::Thread;
use trama_domain::workspace::Workspace;
use trama_protocol::{DaemonMessage, ThreadSnapshot, WorkspaceSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

struct Subscriber {
    id: SubscriptionId,
    tx: mpsc::UnboundedSender<DaemonMessage>,
}

#[derive(Default)]
struct ThreadRuntime {
    live_session: Option<AgentSession>,
    busy: bool,
    interrupt_requested: bool,
    acc_text: String,
    subscribers: Vec<Subscriber>,
}

struct Inner {
    next_sub: u64,
    threads: HashMap<ThreadId, ThreadRuntime>,
}

/// Registry of live adapter sessions, per-thread subscribers, and DB access.
#[derive(Clone)]
pub struct SessionManager {
    db: TramaDb,
    adapter: Arc<dyn AgentAdapter>,
    inner: Arc<Mutex<Inner>>,
}

impl SessionManager {
    pub fn new(db: TramaDb, adapter: Arc<dyn AgentAdapter>) -> Self {
        Self {
            db,
            adapter,
            inner: Arc::new(Mutex::new(Inner {
                next_sub: 1,
                threads: HashMap::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn clear_busy(&self, thread_id: &ThreadId) {
        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            thread.busy = false;
        }
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
        Ok(threads.iter().map(thread_snapshot).collect())
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
        let thread = Thread::new_for_workspace(&workspace, &memberships, Some(title));
        self.db.create_thread(&thread).await?;
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

    pub async fn subscribe(
        &self,
        thread_id: &ThreadId,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> Result<SubscriptionId> {
        if self.db.get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let history = self.db.list_messages_by_thread(thread_id).await?;
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

    /// Persist the user message, then spawn the adapter turn.
    /// A second prompt on a busy thread returns [`DaemonError::Busy`].
    pub async fn send_prompt(&self, thread_id: ThreadId, text: String) -> Result<Message> {
        if self.db.get_thread(&thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            if thread.busy {
                return Err(DaemonError::Busy);
            }
            thread.busy = true;
            thread.interrupt_requested = false;
            thread.acc_text.clear();
        }

        let message = Message::new_user(thread_id.clone(), "user", text.clone());
        if let Err(err) = self.db.append_message(&message).await {
            self.clear_busy(&thread_id);
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

        self.broadcast(
            &thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: message.clone(),
            },
        );

        let manager = self.clone();
        tokio::spawn(async move {
            if let Err(err) = manager.run_turn(thread_id.clone(), text).await {
                manager.broadcast(
                    &thread_id,
                    DaemonMessage::Error {
                        code: err.code().into(),
                        message: err.to_string(),
                    },
                );
                manager.clear_busy(&thread_id);
            }
        });
        Ok(message)
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

    async fn run_turn(&self, thread_id: ThreadId, text: String) -> Result<()> {
        let session = self.ensure_session(&thread_id).await?;
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
            self.clear_busy(&thread_id);
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
        self.adapter.send(&session, AgentPrompt { text }).await?;
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

        let config = self.session_config(thread_id).await?;
        let (tx, rx) = async_channel::unbounded();
        let session = match self.adapter.start(config, tx).await {
            Ok(session) => session,
            Err(err) => {
                return Err(DaemonError::Agent(format!(
                    "{err}. Install Claude Code or set CLAUDE_BIN to the `claude` binary."
                )));
            }
        };
        self.db
            .create_agent_session(thread_id, &session, agent_kind_label(self.adapter.kind()))
            .await?;
        {
            let mut inner = self.lock();
            let thread = inner.threads.entry(thread_id.clone()).or_default();
            thread.live_session = Some(session.clone());
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
        let repos = self.db.list_thread_repositories(thread_id).await?;
        let mut paths: Vec<_> = repos.into_iter().filter_map(|r| r.local_path).collect();
        if paths.is_empty() {
            return Err(DaemonError::Invalid(
                "thread has no repository paths; add repos in config.json".into(),
            ));
        }
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

        let resume_external_session = self
            .db
            .latest_agent_session_for_thread(thread_id)
            .await?
            .and_then(|s| s.external_session_id);

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
        let failed = matches!(event, AgentEvent::Failed { .. });

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
        } else if failed {
            self.clear_busy(thread_id);
        }
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
            self.clear_busy(thread_id);
            return;
        };

        let assistant = Message::new_assistant(
            thread_id.clone(),
            session.id.clone(),
            vec![ContentBlock::Text { text }],
        );
        if let Err(err) = self.db.append_message(&assistant).await {
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

        self.broadcast(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: assistant,
            },
        );

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
        self.clear_busy(thread_id);
    }

    async fn on_pump_closed(&self, thread_id: &ThreadId) {
        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            thread.live_session = None;
            thread.busy = false;
        }
    }
}

fn thread_snapshot(thread: &Thread) -> ThreadSnapshot {
    ThreadSnapshot {
        id: thread.id.clone(),
        title: thread.title.clone().unwrap_or_default(),
        repo_count: thread.repo_attachments.len() as u32,
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

    async fn seed_workspace(db: &TramaDb, with_repo: bool) -> WorkspaceId {
        let workspace = Workspace::new("Default");
        let id = workspace.id.clone();
        db.create_workspace(&workspace).await.unwrap();
        if with_repo {
            let repo = Repository::new(None, Some(std::env::temp_dir()), Some("main".into()));
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

    async fn manager(with_repo: bool) -> (SessionManager, WorkspaceId, FakeAdapter) {
        let db = TramaDb::open_in_memory().unwrap();
        let ws = seed_workspace(&db, with_repo).await;
        let fake = FakeAdapter::scripted();
        let mgr = SessionManager::new(db, Arc::new(fake.clone()));
        (mgr, ws, fake)
    }

    fn agent_event(msg: &DaemonMessage) -> Option<&AgentEvent> {
        match msg {
            DaemonMessage::AgentEventEnvelope { event, .. } => Some(event),
            _ => None,
        }
    }

    async fn wait_turn_complete(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) {
        let deadline = Duration::from_secs(3);
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
        let (mgr, ws, _) = manager(false).await;
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
        let (mgr, ws, _) = manager(true).await;
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
        let deadline = Duration::from_secs(3);
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
        let (mgr, ws, _) = manager(true).await;
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
        let (mgr, _, _) = manager(false).await;
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
        let ws = seed_workspace(&db, true).await;
        let fake = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(5));
        let mgr = SessionManager::new(db.clone(), Arc::new(fake.clone()));
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
        let mgr2 = SessionManager::new(db.clone(), Arc::new(fake2.clone()));
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
        let (mgr, ws, _) = manager(true).await;
        let thread = mgr.create_thread(ws, "busy".into()).await.unwrap();
        mgr.send_prompt(thread.id.clone(), "one".into())
            .await
            .unwrap();
        let err = mgr.send_prompt(thread.id, "two".into()).await.unwrap_err();
        assert!(matches!(err, DaemonError::Busy));
    }

    #[tokio::test]
    async fn send_prompt_reports_missing_claude() {
        let db = TramaDb::open_in_memory().unwrap();
        let ws = seed_workspace(&db, true).await;
        let mgr = SessionManager::new(db, Arc::new(FakeAdapter::fail_start()));
        let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).await.unwrap();
        mgr.send_prompt(thread.id, "hi".into()).await.unwrap();
        let deadline = Duration::from_secs(3);
        let mut saw_error = false;
        for _ in 0..8 {
            let msg = timeout(deadline, rx.recv())
                .await
                .expect("event")
                .expect("channel open");
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
    }

    #[tokio::test]
    async fn subscribe_replays_history() {
        let (mgr, ws, _) = manager(true).await;
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
}
