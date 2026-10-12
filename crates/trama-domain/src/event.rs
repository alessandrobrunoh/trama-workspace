use crate::agent::ApprovalDecision;
use crate::checkpoint::Checkpoint;
use crate::checkpoint::RepoState;
use crate::comment::Comment;
use crate::ids::{
    AgentSessionId, CheckpointId, CommentId, MessageId, RepositoryId, ThreadId, WorkspaceId,
};
use crate::message::{Actor, Message};
use crate::revision::FileRevision;
use crate::subagent::{SubagentProfile, SubagentStatus};
use crate::subthread::{LandOutcome, RepoForkPoint, RepoPullResult, ReviewVerdict};
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
    /// Pending comments were sent to the agent with the user prompt
    /// `message_id` (the thread's `turn`-th prompt).
    CommentsDelivered {
        thread_id: ThreadId,
        comment_ids: Vec<CommentId>,
        message_id: MessageId,
        turn: u32,
    },
    /// The turn that carried `delivered_message_id` ended with the assistant
    /// message `answer_message_id`.
    CommentsAnswered {
        thread_id: ThreadId,
        delivered_message_id: MessageId,
        answer_message_id: MessageId,
    },
    /// The prompt carrying these comments never reached the agent (the turn
    /// failed before it was accepted), so they are pending again.
    CommentDeliveryFailed {
        thread_id: ThreadId,
        comment_ids: Vec<CommentId>,
        message_id: MessageId,
    },
    CommentResolved {
        thread_id: ThreadId,
        comment_id: CommentId,
        resolved_at: DateTime<Utc>,
    },
    CommentReopened {
        thread_id: ThreadId,
        comment_id: CommentId,
    },
    /// Pending comments removed by their author (a root with its replies).
    CommentsDeleted {
        thread_id: ThreadId,
        comment_ids: Vec<CommentId>,
    },
    CheckpointCreated {
        checkpoint: Checkpoint,
    },
    /// A subthread copied its parent's checkouts (and, when available, its
    /// agent conversation). Recorded on the subthread aggregate after its
    /// `ThreadCreated`.
    SubthreadForked {
        thread_id: ThreadId,
        parent_thread_id: ThreadId,
        repos: Vec<RepoForkPoint>,
        parent_external_session_id: Option<String>,
    },
    /// A review subthread's verdict, recorded on the parent aggregate.
    /// `snapshot` holds one snapshot commit per repository of what the
    /// review itself changed, so the parent can pull those changes later.
    ReviewVerdictRecorded {
        review_thread_id: ThreadId,
        parent_thread_id: ThreadId,
        verdict: ReviewVerdict,
        snapshot: Vec<RepoState>,
    },
    /// A subthread's changes were applied to its parent's checkouts. Nothing
    /// was committed. Recorded on the parent aggregate.
    ChangesPulled {
        from_thread_id: ThreadId,
        into_thread_id: ThreadId,
        repos: Vec<RepoPullResult>,
    },
    /// A land subthread reported its result. Recorded on the parent aggregate.
    LandResultRecorded {
        land_thread_id: ThreadId,
        parent_thread_id: ThreadId,
        outcome: LandOutcome,
        summary: String,
    },
    /// The thread `thread_id` was spawned by its parent's agent through the
    /// Trama MCP tools. Recorded on the child aggregate after its fork.
    SubagentSpawned {
        thread_id: ThreadId,
        parent_thread_id: ThreadId,
        profile: SubagentProfile,
        tool_use_id: Option<String>,
    },
    /// A subagent's task changed state. Recorded on the child aggregate.
    SubagentStatusChanged {
        thread_id: ThreadId,
        status: SubagentStatus,
        detail: Option<String>,
    },
    /// One thread's agent sent a message to another thread's agent.
    /// Recorded on the sender; the receiver gets the prompt as a
    /// `MessageAdded` on its own aggregate when it is delivered.
    AgentMessageSent {
        from_thread_id: ThreadId,
        to_thread_id: ThreadId,
        message_id: MessageId,
    },
    /// A subthread was forked at a message of its parent rather than at the
    /// parent's current state: its conversation ends with `at_message_id`
    /// and its files are the parent's turn checkpoint `checkpoint_id`.
    /// Recorded on the child aggregate after `SubthreadForked`.
    ThreadForkedAtMessage {
        thread_id: ThreadId,
        parent_thread_id: ThreadId,
        at_message_id: MessageId,
        checkpoint_id: Option<CheckpointId>,
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
            Self::CommentsDelivered { .. } => "comment.delivered",
            Self::CommentsAnswered { .. } => "comment.answered",
            Self::CommentDeliveryFailed { .. } => "comment.delivery_failed",
            Self::CommentResolved { .. } => "comment.resolved",
            Self::CommentReopened { .. } => "comment.reopened",
            Self::CommentsDeleted { .. } => "comment.deleted",
            Self::CheckpointCreated { .. } => "checkpoint.created",
            Self::SubthreadForked { .. } => "subthread.forked",
            Self::ReviewVerdictRecorded { .. } => "review.verdict_recorded",
            Self::ChangesPulled { .. } => "changes.pulled",
            Self::LandResultRecorded { .. } => "land.result_recorded",
            Self::SubagentSpawned { .. } => "subagent.spawned",
            Self::SubagentStatusChanged { .. } => "subagent.status_changed",
            Self::AgentMessageSent { .. } => "agent.message_sent",
            Self::ThreadForkedAtMessage { .. } => "thread.forked_at_message",
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
        AgentSessionId, CheckpointId, CommentId, MessageId, RepositoryId, ThreadId, WorkspaceId,
    };
    use crate::message::Message;
    use crate::subthread::{FileChangeStatus, PullFileOutcome, PullFileResult};
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
            parent_id: None,
            excerpt: Some("let x = 1;".into()),
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
            message_id: Some(MessageId::from("m9")),
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
        let sub = ThreadId::generate();
        let repo = RepositoryId::generate();
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
                DomainEvent::CommentCreated {
                    comment: cmt.clone(),
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.created",
            ),
            (
                DomainEvent::CommentsDelivered {
                    thread_id: th.id.clone(),
                    comment_ids: vec![cmt.id.clone()],
                    message_id: MessageId::from("m1"),
                    turn: 2,
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.delivered",
            ),
            (
                DomainEvent::CommentsAnswered {
                    thread_id: th.id.clone(),
                    delivered_message_id: MessageId::from("m1"),
                    answer_message_id: MessageId::from("m2"),
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.answered",
            ),
            (
                DomainEvent::CommentDeliveryFailed {
                    thread_id: th.id.clone(),
                    comment_ids: vec![cmt.id.clone()],
                    message_id: MessageId::from("m1"),
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.delivery_failed",
            ),
            (
                DomainEvent::CommentResolved {
                    thread_id: th.id.clone(),
                    comment_id: cmt.id.clone(),
                    resolved_at: Utc::now(),
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.resolved",
            ),
            (
                DomainEvent::CommentReopened {
                    thread_id: th.id.clone(),
                    comment_id: cmt.id.clone(),
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.reopened",
            ),
            (
                DomainEvent::CommentsDeleted {
                    thread_id: th.id.clone(),
                    comment_ids: vec![cmt.id],
                },
                Aggregate::Thread { id: th.id.clone() },
                "comment.deleted",
            ),
            (
                DomainEvent::CheckpointCreated { checkpoint: ckpt },
                Aggregate::Thread { id: th.id.clone() },
                "checkpoint.created",
            ),
            (
                DomainEvent::SubthreadForked {
                    thread_id: sub.clone(),
                    parent_thread_id: th.id.clone(),
                    repos: vec![RepoForkPoint {
                        repository_id: repo.clone(),
                        base_revision: "abc".into(),
                        fork_commit: "def".into(),
                    }],
                    parent_external_session_id: Some("ext".into()),
                },
                Aggregate::Thread { id: sub.clone() },
                "subthread.forked",
            ),
            (
                DomainEvent::ReviewVerdictRecorded {
                    review_thread_id: sub.clone(),
                    parent_thread_id: th.id.clone(),
                    verdict: ReviewVerdict::RequestChanges {
                        summary: "fix".into(),
                    },
                    snapshot: vec![RepoState {
                        repository_id: repo.clone(),
                        commit: Some("abc".into()),
                        tree: Some("def".into()),
                        working_changes: WorkingChanges::Dirty,
                    }],
                },
                Aggregate::Thread { id: th.id.clone() },
                "review.verdict_recorded",
            ),
            (
                DomainEvent::ChangesPulled {
                    from_thread_id: sub.clone(),
                    into_thread_id: th.id.clone(),
                    repos: vec![RepoPullResult {
                        repository_id: repo,
                        files: vec![PullFileResult {
                            path: "a.txt".into(),
                            change: FileChangeStatus::Added,
                            outcome: PullFileOutcome::Applied,
                        }],
                    }],
                },
                Aggregate::Thread { id: th.id.clone() },
                "changes.pulled",
            ),
            (
                DomainEvent::LandResultRecorded {
                    land_thread_id: sub.clone(),
                    parent_thread_id: th.id.clone(),
                    outcome: LandOutcome::Landed,
                    summary: "merged".into(),
                },
                Aggregate::Thread { id: th.id.clone() },
                "land.result_recorded",
            ),
            (
                DomainEvent::SubagentSpawned {
                    thread_id: sub.clone(),
                    parent_thread_id: th.id.clone(),
                    profile: SubagentProfile::Worker,
                    tool_use_id: Some("toolu_1".into()),
                },
                Aggregate::Thread { id: sub.clone() },
                "subagent.spawned",
            ),
            (
                DomainEvent::SubagentStatusChanged {
                    thread_id: sub.clone(),
                    status: SubagentStatus::WaitingToSync,
                    detail: None,
                },
                Aggregate::Thread { id: sub.clone() },
                "subagent.status_changed",
            ),
            (
                DomainEvent::AgentMessageSent {
                    from_thread_id: th.id.clone(),
                    to_thread_id: sub.clone(),
                    message_id: MessageId::from("m5"),
                },
                Aggregate::Thread { id: th.id.clone() },
                "agent.message_sent",
            ),
            (
                DomainEvent::ThreadForkedAtMessage {
                    thread_id: sub.clone(),
                    parent_thread_id: th.id.clone(),
                    at_message_id: MessageId::from("m4"),
                    checkpoint_id: Some(CheckpointId::from("c1")),
                },
                Aggregate::Thread { id: sub },
                "thread.forked_at_message",
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
