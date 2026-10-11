//! In-memory mock agent sessions keyed by thread id.

use crate::error::{DaemonError, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use trama_domain::agent::{AgentEvent, ApprovalDecision, UsageReport};
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_domain::message::Message;
use trama_protocol::DaemonMessage;
use trama_protocol::{ThreadSnapshot, WorkspaceSnapshot};

const MOCK_CHUNKS: &[&str] = &["Hello", " from ", "the mock ", "agent."];

/// Milliseconds between scripted agent events (inclusive range 30–80).
fn chunk_delay_ms(index: usize) -> u64 {
    30 + ((index as u64 * 15) % 51)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

struct Subscriber {
    id: SubscriptionId,
    tx: mpsc::UnboundedSender<DaemonMessage>,
}

struct ThreadState {
    snapshot: ThreadSnapshot,
    workspace_id: WorkspaceId,
    run: Option<AbortHandle>,
    subscribers: Vec<Subscriber>,
}

struct Inner {
    next_sub: u64,
    workspaces: HashMap<WorkspaceId, WorkspaceSnapshot>,
    threads: HashMap<ThreadId, ThreadState>,
}

/// Registry of workspaces, threads, mock runs, and per-thread subscribers.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<Inner>>,
}

impl SessionManager {
    pub fn new() -> Self {
        let workspace = WorkspaceSnapshot {
            id: WorkspaceId::generate(),
            title: "Default".into(),
            repo_count: 0,
        };
        let mut workspaces = HashMap::new();
        workspaces.insert(workspace.id.clone(), workspace);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                next_sub: 1,
                workspaces,
                threads: HashMap::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceSnapshot> {
        self.lock().workspaces.values().cloned().collect()
    }

    pub fn default_workspace_id(&self) -> Result<WorkspaceId> {
        self.lock()
            .workspaces
            .values()
            .next()
            .map(|ws| ws.id.clone())
            .ok_or_else(|| DaemonError::NotFound("no workspaces".into()))
    }

    pub fn list_threads(&self, workspace_id: &WorkspaceId) -> Result<Vec<ThreadSnapshot>> {
        let inner = self.lock();
        if !inner.workspaces.contains_key(workspace_id) {
            return Err(DaemonError::NotFound(format!("workspace {workspace_id}")));
        }
        Ok(inner
            .threads
            .values()
            .filter(|t| t.workspace_id == *workspace_id)
            .map(|t| t.snapshot.clone())
            .collect())
    }

    pub fn create_thread(
        &self,
        workspace_id: WorkspaceId,
        title: String,
    ) -> Result<ThreadSnapshot> {
        let mut inner = self.lock();
        let repo_count = inner
            .workspaces
            .get(&workspace_id)
            .map(|ws| ws.repo_count)
            .ok_or_else(|| DaemonError::NotFound(format!("workspace {workspace_id}")))?;
        let snapshot = ThreadSnapshot {
            id: ThreadId::generate(),
            title,
            repo_count,
        };
        inner.threads.insert(
            snapshot.id.clone(),
            ThreadState {
                snapshot: snapshot.clone(),
                workspace_id,
                run: None,
                subscribers: Vec::new(),
            },
        );
        Ok(snapshot)
    }

    pub fn subscribe(
        &self,
        thread_id: &ThreadId,
        tx: mpsc::UnboundedSender<DaemonMessage>,
    ) -> Result<SubscriptionId> {
        let mut inner = self.lock();
        if !inner.threads.contains_key(thread_id) {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let id = SubscriptionId(inner.next_sub);
        inner.next_sub += 1;
        if let Some(thread) = inner.threads.get_mut(thread_id) {
            thread.subscribers.push(Subscriber { id, tx });
        }
        Ok(id)
    }

    pub fn unsubscribe(&self, thread_id: &ThreadId, sub_id: SubscriptionId) -> Result<()> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        thread.subscribers.retain(|s| s.id != sub_id);
        Ok(())
    }

    pub fn unsubscribe_all(&self, subs: &[(ThreadId, SubscriptionId)]) {
        let mut inner = self.lock();
        for (thread_id, sub_id) in subs {
            if let Some(thread) = inner.threads.get_mut(thread_id) {
                thread.subscribers.retain(|s| s.id != *sub_id);
            }
        }
    }

    /// Start a scripted mock turn. A previous run on the same thread is aborted.
    pub fn send_prompt(&self, thread_id: ThreadId, text: String) -> Result<Message> {
        let message = {
            let mut inner = self.lock();
            let thread = inner
                .threads
                .get_mut(&thread_id)
                .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
            if let Some(run) = thread.run.take() {
                run.abort();
            }
            Message::new_user(thread_id.clone(), "user", text)
        };

        self.broadcast(
            &thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message: message.clone(),
            },
        );

        let manager = self.clone();
        let run_thread = thread_id.clone();
        let handle = tokio::spawn(async move {
            manager.run_mock_turn(run_thread).await;
        });

        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(&thread_id) {
            thread.run = Some(handle.abort_handle());
        } else {
            handle.abort();
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        Ok(message)
    }

    pub fn interrupt(&self, thread_id: &ThreadId) -> Result<bool> {
        let mut inner = self.lock();
        let thread = inner
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| DaemonError::NotFound(format!("thread {thread_id}")))?;
        if let Some(run) = thread.run.take() {
            run.abort();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn respond_approval(
        &self,
        thread_id: &ThreadId,
        _request_id: &str,
        _decision: ApprovalDecision,
    ) -> Result<()> {
        let inner = self.lock();
        if inner.threads.contains_key(thread_id) {
            Ok(())
        } else {
            Err(DaemonError::NotFound(format!("thread {thread_id}")))
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

    async fn run_mock_turn(&self, thread_id: ThreadId) {
        for (index, chunk) in MOCK_CHUNKS.iter().enumerate() {
            tokio::time::sleep(Duration::from_millis(chunk_delay_ms(index))).await;
            self.broadcast(
                &thread_id,
                DaemonMessage::AgentEventEnvelope {
                    thread_id: thread_id.clone(),
                    event: AgentEvent::TextDelta {
                        text: (*chunk).to_string(),
                    },
                },
            );
        }

        tokio::time::sleep(Duration::from_millis(chunk_delay_ms(MOCK_CHUNKS.len()))).await;
        let totals = UsageReport {
            model: "mock".into(),
            input_tokens: 16,
            output_tokens: 8,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        self.broadcast(
            &thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::UsageReported {
                    report: totals.clone(),
                },
            },
        );
        self.broadcast(
            &thread_id,
            DaemonMessage::UsageSnapshot {
                thread_id: thread_id.clone(),
                totals,
            },
        );

        tokio::time::sleep(Duration::from_millis(40)).await;
        self.broadcast(
            &thread_id,
            DaemonMessage::AgentEventEnvelope {
                thread_id: thread_id.clone(),
                event: AgentEvent::TurnCompleted {
                    num_turns: 1,
                    cost_usd_micros: Some(0),
                },
            },
        );

        let mut inner = self.lock();
        if let Some(thread) = inner.threads.get_mut(&thread_id) {
            thread.run = None;
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DaemonError;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};
    use trama_domain::agent::AgentEvent;
    use trama_domain::ids::{ThreadId, WorkspaceId};
    use trama_protocol::DaemonMessage;

    fn agent_event(msg: &DaemonMessage) -> Option<&AgentEvent> {
        match msg {
            DaemonMessage::AgentEventEnvelope { event, .. } => Some(event),
            _ => None,
        }
    }

    #[tokio::test]
    async fn create_and_list_threads() {
        let mgr = SessionManager::new();
        let ws = mgr.default_workspace_id().unwrap();
        assert_eq!(mgr.list_workspaces().len(), 1);
        let created = mgr.create_thread(ws.clone(), "First".into()).unwrap();
        assert_eq!(created.title, "First");
        let listed = mgr.list_threads(&ws).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        let missing = WorkspaceId::from("nope");
        assert!(mgr.list_threads(&missing).is_err());
    }

    #[tokio::test]
    async fn send_prompt_emits_scripted_events() {
        let mgr = SessionManager::new();
        let ws = mgr.default_workspace_id().unwrap();
        let thread = mgr.create_thread(ws, "t".into()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).unwrap();
        mgr.send_prompt(thread.id.clone(), "hi".into()).unwrap();

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
        let mgr = SessionManager::new();
        let ws = mgr.default_workspace_id().unwrap();
        let thread = mgr.create_thread(ws, "t".into()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        mgr.subscribe(&thread.id, tx).unwrap();
        mgr.send_prompt(thread.id.clone(), "hi".into()).unwrap();

        // Wait until the first agent event (or the user message), then cancel.
        let _first = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(mgr.interrupt(&thread.id).unwrap());

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
        let mgr = SessionManager::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = mgr.subscribe(&ThreadId::from("missing"), tx).unwrap_err();
        assert!(matches!(err, DaemonError::NotFound(_)));
    }
}
