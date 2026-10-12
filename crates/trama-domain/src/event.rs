use crate::agent::ApprovalDecision;
use crate::checkpoint::Checkpoint;
use crate::comment::Comment;
use crate::ids::{AgentSessionId, RepositoryId, ThreadId, WorkspaceId};
use crate::message::{Actor, Message};
use crate::revision::FileRevision;
use crate::thread::Thread;
use crate::workspace::Workspace;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const EVENT_SCHEMA_VERSION: u16 = 1;

/// What an event belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "aggregate", rename_all = "snake_case")]
pub enum Aggregate {
    Workspace { id: WorkspaceId },
    Repository { id: RepositoryId },
    Thread { id: ThreadId },
    Session { id: AgentSessionId },
}

/// Append-only record in the TramaDB event log. Never mutated once written;
/// all other tables are projections rebuildable from the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: String,
    /// Monotonic sequence assigned by the store on append.
    pub seq: Option<u64>,
    pub aggregate: Aggregate,
    pub actor: Actor,
    pub schema_version: u16,
    pub kind: String,
    pub payload: serde_json::Value,
    /// Causal parents (event ids) — the frontier. Sync-ready by design.
    pub parents: Vec<String>,
    pub created_at: DateTime<Utc>,
}

impl EventRecord {
    pub fn new(
        aggregate: Aggregate,
        actor: Actor,
        event: DomainEvent,
        parents: Vec<String>,
    ) -> crate::Result<Self> {
        Ok(Self {
            id: ulid::Ulid::new().to_string(),
            seq: None,
            aggregate,
            actor,
            schema_version: EVENT_SCHEMA_VERSION,
            kind: event.kind().to_string(),
            payload: serde_json::to_value(&event)?,
            parents,
            created_at: Utc::now(),
        })
    }

    /// Decode the typed payload. Returns `Ok(None)` when `payload` is JSON null.
    pub fn decode_domain(&self) -> crate::Result<Option<DomainEvent>> {
        if self.payload.is_null() {
            Ok(None)
        } else {
            Ok(Some(serde_json::from_value(self.payload.clone())?))
        }
    }
}

/// The product events. Typed payloads, serialized into `EventRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    WorkspaceCreated {
        workspace: Workspace,
    },
    ThreadCreated {
        thread: Thread,
    },
    MessageAdded {
        message: Message,
    },
    AgentTurnStarted {
        thread_id: ThreadId,
        session_id: AgentSessionId,
        agent_kind: String,
    },
    AgentTurnCompleted {
        thread_id: ThreadId,
        session_id: AgentSessionId,
        num_turns: u32,
        cost_usd_micros: Option<u64>,
    },
    /// The agent asked for permission to run a tool and the request was
    /// shown to clients.
    ApprovalRequested {
        thread_id: ThreadId,
        session_id: AgentSessionId,
        request_id: String,
        tool_name: String,
        input_json: serde_json::Value,
    },
    /// A permission request was answered. `reason` is set when it was not
    /// a client decision (remembered for the session, timeout, interrupt,
    /// session end).
    ApprovalResolved {
        thread_id: ThreadId,
        session_id: AgentSessionId,
        request_id: String,
        tool_name: String,
        decision: ApprovalDecision,
        reason: Option<String>,
    },
    FileRevisionRecorded {
        revision: FileRevision,
    },
    CommentCreated {
        comment: Comment,
    },
    CheckpointCreated {
        checkpoint: Checkpoint,
    },
}

impl DomainEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WorkspaceCreated { .. } => "workspace.created",
            Self::ThreadCreated { .. } => "thread.created",
            Self::MessageAdded { .. } => "message.added",
            Self::AgentTurnStarted { .. } => "agent.turn_started",
            Self::AgentTurnCompleted { .. } => "agent.turn_completed",
            Self::ApprovalRequested { .. } => "agent.approval_requested",
            Self::ApprovalResolved { .. } => "agent.approval_resolved",
            Self::FileRevisionRecorded { .. } => "file.revision_recorded",
            Self::CommentCreated { .. } => "comment.created",
            Self::CheckpointCreated { .. } => "checkpoint.created",
        }
    }
}

/// Port implemented by `trama-db`.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Appends a batch atomically; assigns `seq` in order.
    async fn append(&self, records: Vec<EventRecord>) -> crate::Result<()>;
    async fn for_aggregate(
        &self,
        aggregate: &Aggregate,
        after_seq: Option<u64>,
        limit: u32,
    ) -> crate::Result<Vec<EventRecord>>;
    async fn decode(record: &EventRecord) -> crate::Result<Option<DomainEvent>>
    where
        Self: Sized,
    {
        Ok(serde_json::from_value(record.payload.clone()).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{Checkpoint, CheckpointLabel, RepoState, WorkingChanges};
    use crate::comment::{CodeAnchor, Comment, CommentTarget};
    use crate::ids::{
        AgentSessionId, CheckpointId, CommentId, RepositoryId, ThreadId, WorkspaceId,
    };
    use crate::message::Message;
    use crate::thread::{AttachMode, Thread, ThreadKind, ThreadRepositoryAttachment};
    use crate::workspace::Workspace;
    use std::path::PathBuf;

    fn workspace() -> Workspace {
        Workspace::new("ws")
    }

    fn thread(workspace_id: WorkspaceId) -> Thread {
        Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace_id),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("t".into()),
            repo_attachments: vec![ThreadRepositoryAttachment {
                repository_id: RepositoryId::generate(),
                base_revision: None,
                checkout_id: None,
                attach_mode: AttachMode::Isolated,
            }],
        }
    }

    fn message(thread_id: ThreadId) -> Message {
        Message::new_user(thread_id, "bob", "hi")
    }

    fn comment(thread_id: ThreadId) -> Comment {
        Comment {
            id: CommentId::generate(),
            thread_id,
            author: Actor::Human { id: "bob".into() },
            target: CommentTarget::Code {
                anchor: CodeAnchor {
                    repository_id: RepositoryId::generate(),
                    path: PathBuf::from("src/lib.rs"),
                    revision: 1,
                    start_line: 2,
                    end_line: 2,
                    text_hash: Some("abc".into()),
                    context_hash: Some("def".into()),
                },
            },
            body: "nit".into(),
            created_at: Utc::now(),
            resolved_at: None,
        }
    }

    fn revision() -> FileRevision {
        FileRevision {
            repository_id: RepositoryId::generate(),
            path: PathBuf::from("src/lib.rs"),
            revision: 3,
            content_hash: "deadbeef".into(),
            size_bytes: 12,
            event_seq: Some(9),
            recorded_at: Utc::now(),
        }
    }

    fn checkpoint(thread_id: ThreadId) -> Checkpoint {
        Checkpoint {
            id: CheckpointId::generate(),
            thread_id,
            event_seq: 4,
            label: CheckpointLabel::Turn,
            repo_states: vec![RepoState {
                repository_id: RepositoryId::generate(),
                commit: Some("abc".into()),
                tree: Some("def".into()),
                working_changes: WorkingChanges::Clean,
            }],
            created_at: Utc::now(),
        }
    }

    fn all_events() -> Vec<(DomainEvent, Aggregate, &'static str)> {
        let ws = workspace();
        let th = thread(ws.id.clone());
        let session = AgentSessionId::generate();
        let msg = message(th.id.clone());
        let cmt = comment(th.id.clone());
        let rev = revision();
        let ckpt = checkpoint(th.id.clone());
        vec![
            (
                DomainEvent::WorkspaceCreated {
                    workspace: ws.clone(),
                },
                Aggregate::Workspace { id: ws.id.clone() },
                "workspace.created",
            ),
            (
                DomainEvent::ThreadCreated { thread: th.clone() },
                Aggregate::Thread { id: th.id.clone() },
                "thread.created",
            ),
            (
                DomainEvent::MessageAdded { message: msg },
                Aggregate::Thread { id: th.id.clone() },
                "message.added",
            ),
            (
                DomainEvent::AgentTurnStarted {
                    thread_id: th.id.clone(),
                    session_id: session.clone(),
                    agent_kind: "claude_code".into(),
                },
                Aggregate::Session {
                    id: session.clone(),
                },
                "agent.turn_started",
            ),
            (
                DomainEvent::AgentTurnCompleted {
                    thread_id: th.id.clone(),
                    session_id: session.clone(),
                    num_turns: 2,
                    cost_usd_micros: Some(1500),
                },
                Aggregate::Session {
                    id: session.clone(),
                },
                "agent.turn_completed",
            ),
            (
                DomainEvent::ApprovalRequested {
                    thread_id: th.id.clone(),
                    session_id: session.clone(),
                    request_id: "apr-1".into(),
                    tool_name: "Bash".into(),
                    input_json: serde_json::json!({"command": "ls"}),
                },
                Aggregate::Session {
                    id: session.clone(),
                },
                "agent.approval_requested",
            ),
            (
                DomainEvent::ApprovalResolved {
                    thread_id: th.id.clone(),
                    session_id: session.clone(),
                    request_id: "apr-1".into(),
                    tool_name: "Bash".into(),
                    decision: ApprovalDecision::Deny,
                    reason: Some("timed out".into()),
                },
                Aggregate::Session { id: session },
                "agent.approval_resolved",
            ),
            (
                DomainEvent::FileRevisionRecorded {
                    revision: rev.clone(),
                },
                Aggregate::Repository {
                    id: rev.repository_id.clone(),
                },
                "file.revision_recorded",
            ),
            (
                DomainEvent::CommentCreated { comment: cmt },
                Aggregate::Thread { id: th.id.clone() },
                "comment.created",
            ),
            (
                DomainEvent::CheckpointCreated { checkpoint: ckpt },
                Aggregate::Thread { id: th.id },
                "checkpoint.created",
            ),
        ]
    }

    #[test]
    fn kind_strings() {
        for (event, _, kind) in all_events() {
            assert_eq!(event.kind(), kind);
        }
    }

    #[test]
    fn event_record_json_roundtrip_decodes_equal_payload() {
        for (event, aggregate, kind) in all_events() {
            let record =
                EventRecord::new(aggregate, Actor::System, event.clone(), vec!["p1".into()])
                    .unwrap();
            assert_eq!(record.kind, kind);
            assert_eq!(record.schema_version, EVENT_SCHEMA_VERSION);
            assert!(record.seq.is_none());

            let json = serde_json::to_string(&record).unwrap();
            let deserialized: EventRecord = serde_json::from_str(&json).unwrap();
            let decoded = deserialized.decode_domain().unwrap().expect("payload");
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                serde_json::to_value(&event).unwrap()
            );
            assert_eq!(decoded.kind(), kind);
        }
    }

    #[test]
    fn decode_domain_null_payload_is_none() {
        let ws = workspace();
        let mut record = EventRecord::new(
            Aggregate::Workspace { id: ws.id.clone() },
            Actor::System,
            DomainEvent::WorkspaceCreated { workspace: ws },
            vec![],
        )
        .unwrap();
        record.payload = serde_json::Value::Null;
        assert!(record.decode_domain().unwrap().is_none());
    }

    #[test]
    fn decode_domain_rejects_invalid_payload() {
        let ws = workspace();
        let mut record = EventRecord::new(
            Aggregate::Workspace { id: ws.id.clone() },
            Actor::System,
            DomainEvent::WorkspaceCreated { workspace: ws },
            vec![],
        )
        .unwrap();
        record.payload = serde_json::json!({"type": "not_a_real_event"});
        assert!(record.decode_domain().is_err());
    }
}
