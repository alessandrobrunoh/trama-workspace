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
}

/// The product events. Typed payloads, serialized into `EventRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    WorkspaceCreated { workspace: Workspace },
    ThreadCreated { thread: Thread },
    MessageAdded { message: Message },
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
    FileRevisionRecorded { revision: FileRevision },
    CommentCreated { comment: Comment },
    CheckpointCreated { checkpoint: Checkpoint },
}

impl DomainEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WorkspaceCreated { .. } => "workspace.created",
            Self::ThreadCreated { .. } => "thread.created",
            Self::MessageAdded { .. } => "message.added",
            Self::AgentTurnStarted { .. } => "agent.turn_started",
            Self::AgentTurnCompleted { .. } => "agent.turn_completed",
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
