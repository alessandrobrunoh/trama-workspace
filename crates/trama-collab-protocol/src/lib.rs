//! Wire types shared by the `trama-collab` service, the desktop client and
//! the daemon acting as a runner.
//!
//! # Execution rule (ADR 0008)
//!
//! A turn runs on the machine of the person who pressed Send, with that
//! person's own `claude` login. The service replicates conversation, drafts,
//! comments, verdicts and change summaries; it never routes one participant's
//! prompt to another participant's runner. A runner credential belongs to
//! exactly one user and can only claim turns authored by that user.
//!
//! # Framing
//!
//! HTTP bodies are JSON objects. WebSocket frames are JSON text messages
//! tagged by a `type` field (`{"type":"draft","text":"..."}`); binary frames
//! are rejected. Unknown `type` tags are denied by serde.
//!
//! # Ordering
//!
//! Every durable event carries a per-thread `seq` that increases by exactly
//! one per event. A client keeps the highest `seq` it applied as its cursor
//! and reconnects with `?after=<cursor>`; the server backfills from the log,
//! then streams live events, never skipping or repeating a `seq`. Drafts and
//! presence are ephemeral frames without a `seq` and are never stored in the
//! log.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on any breaking change to the types in this crate.
pub const COLLAB_PROTOCOL_VERSION: u32 = 2;

/// Largest payload of one published event (`POST /threads/{id}/publish`).
pub const MAX_PUBLISH_PAYLOAD_BYTES: usize = 192 * 1024;
/// Events per publish batch.
pub const MAX_PUBLISH_EVENTS: usize = 64;
/// Body limit of the publish route (a whole batch).
pub const PUBLISH_BODY_LIMIT: usize = 1024 * 1024;

/// Who may open a thread besides its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevel {
    /// Only the owner and invited usernames.
    Invited,
    /// Invited usernames plus anyone who presents the current link token.
    Link,
}

/// How a member obtained access. Link members lose access when the link is
/// revoked; invited members keep it until the owner removes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberVia {
    Owner,
    Invite,
    Link,
}

/// Durable event kinds. Payloads are opaque JSON except where noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A user turn: `{"text": ...}`, authored by the sender, `command_id` set.
    MessageAppended,
    /// The sender's own runner claimed the turn: `{"runner": name}`.
    CommandClaimed,
    /// The runner finished the turn: `{"state": "completed"|"failed", "result": ...}`.
    CommandFinished,
    /// The runner was lost after claiming; the outcome is unknown and the turn
    /// is never retried automatically: `{"runner": name, "reason": ...}`.
    CommandAmbiguous,
    /// Streamed from the author's runner, tagged with `command_id`.
    AgentEvent,
    /// Changes summary reported by the author's runner.
    ChangesSummary,
    /// `{"body": ..., "anchor": <opaque>|null}`.
    CommentCreated,
    /// `{"parent_seq": <seq of comment_created>, "body": ...}`.
    CommentReplied,
    /// `{"comment_seq": <seq of comment_created>}`.
    CommentResolved,
    /// `{"verdict": ..., "body": ...}`.
    ReviewVerdict,
    /// `{"username": ..., "via": ...}`.
    MemberJoined,
    /// `{"username": ...}`.
    MemberRemoved,
    /// `{"access": ...}`. Never carries the link token.
    AccessChanged,
    /// A transcript message published by the participant whose daemon holds
    /// it (history backfill on share, then every new local message):
    /// `{"origin", "role", "content", "annotation", "created_at",
    /// "activity_parts"}`. See ADR 0013.
    HistoryMessage,
    /// One part of the tool activity of the turn a `history_message` ends,
    /// published before that message: `{"origin", "part", "parts", "items"}`.
    HistoryActivity,
    /// The publisher's diff of its checkouts: `{"captured_at", "base",
    /// "repos": [RepoChanges]}` (bounded).
    ChangesSnapshot,
    /// A subagent child's task state: `{"status", "detail"}`.
    SubagentStatus,
    /// `{"comment_seq": <seq of comment_created>}`; only its author deletes.
    CommentDeleted,
    /// `{"comment_seq": <seq of comment_created>}`.
    CommentReopened,
    /// A child thread (subthread, fork or subagent) was created under this
    /// thread: `{"child_thread_id", "title", "kind", "subagent",
    /// "fork_origin"}`. Appended to the parent's log by the service.
    ChildThreadAdded,
}

/// Event kinds a member may append through `POST /threads/{id}/publish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishKind {
    HistoryMessage,
    HistoryActivity,
    ChangesSnapshot,
    SubagentStatus,
}

impl PublishKind {
    pub fn event_kind(self) -> EventKind {
        match self {
            Self::HistoryMessage => EventKind::HistoryMessage,
            Self::HistoryActivity => EventKind::HistoryActivity,
            Self::ChangesSnapshot => EventKind::ChangesSnapshot,
            Self::SubagentStatus => EventKind::SubagentStatus,
        }
    }
}

/// One entry of the durable, ordered per-thread log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub thread_id: String,
    pub seq: i64,
    pub kind: EventKind,
    /// Username of the participant on whose behalf the event was written.
    pub author: Option<String>,
    pub command_id: Option<String>,
    pub created_at: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Turn,
    Comment,
    Resolve,
    Verdict,
    /// One event of a publish batch.
    Publish,
    /// Creation of a child thread.
    Child,
    Reopen,
    Delete,
}

/// Turn commands move `pending -> claimed -> completed|failed`, or
/// `claimed -> ambiguous` when the claiming runner disappears. Other command
/// kinds are applied synchronously and report `applied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Pending,
    Claimed,
    Completed,
    Failed,
    Ambiguous,
    Applied,
}

/// Result of any idempotent command. Retrying the same `command_id` returns
/// the original `seq` and the command's current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: String,
    pub thread_id: String,
    pub kind: CommandKind,
    pub state: CommandState,
    /// Sequence number of the event the command originally appended.
    pub seq: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub command_id: String,
    pub thread_id: String,
    pub kind: CommandKind,
    pub author: String,
    pub state: CommandState,
    pub seq: i64,
    pub runner: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approved,
    ChangesRequested,
    Commented,
}

// ---------------------------------------------------------------------------
// HTTP bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateThreadRequest {
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub title: String,
    pub owner: String,
    pub access: AccessLevel,
    /// The caller's own membership (of the root thread, for a child).
    pub via: MemberVia,
    pub last_seq: i64,
    /// Set for a child thread: its parent, and the root whose membership
    /// it inherits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_thread_id: Option<String>,
    /// Child kind label (`sub`, `review`, `land`, `fork`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub username: String,
    pub via: MemberVia,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadDetail {
    #[serde(flatten)]
    pub summary: ThreadSummary,
    pub members: Vec<MemberInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAccessRequest {
    pub access: AccessLevel,
}

/// `link_token` is returned once, when access is set to (or re-set to, which
/// rotates it) `link`. Only its digest is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAccessResponse {
    pub access: AccessLevel,
    pub link_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteRequest {
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
    pub link_token: String,
}

/// Send the sender's trailing draft as one user turn. When `text` is absent
/// the server uses the sender's current live draft.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendRequest {
    pub command_id: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentRequest {
    pub command_id: String,
    pub body: String,
    #[serde(default)]
    pub anchor: Option<Value>,
    /// Reply to the `comment_created` event with this sequence number.
    #[serde(default)]
    pub parent_seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub command_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerdictRequest {
    pub command_id: String,
    pub verdict: Verdict,
    #[serde(default)]
    pub body: Option<String>,
}

/// One event of a publish batch. `command_id` makes it idempotent: a retry
/// returns the original receipt and appends nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishEvent {
    pub command_id: String,
    pub kind: PublishKind,
    pub payload: Value,
}

/// Up to [`MAX_PUBLISH_EVENTS`] events appended in order, in one
/// transaction, counted as one mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    pub events: Vec<PublishEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishResponse {
    pub receipts: Vec<CommandReceipt>,
}

/// Create a child thread under `/threads/{parent}`. The child inherits the
/// root's membership; a `child_thread_added` event is appended to the
/// parent's log in the same transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateChildRequest {
    pub command_id: String,
    pub title: String,
    /// Kind label: `sub`, `review`, `land` or `fork`.
    pub kind: String,
    /// Subagent profile and status (opaque, at most 1 KiB).
    #[serde(default)]
    pub subagent: Option<Value>,
    /// Origin key of the parent message a fork continues from.
    #[serde(default)]
    pub fork_origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildCreated {
    pub thread: ThreadSummary,
    /// Sequence number of the `child_thread_added` event in the parent log.
    pub seq: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<EventEnvelope>,
    /// Highest sequence number in the thread at read time.
    pub last_seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRunnerRequest {
    pub name: String,
}

/// The runner token is shown once; only its digest is stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerCreated {
    pub runner_id: i64,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerInfo {
    pub runner_id: i64,
    pub name: String,
    pub created_at: i64,
    pub revoked: bool,
}

// ---------------------------------------------------------------------------
// Client WebSocket: GET /threads/{id}/ws?after=<seq>
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Online,
    Active,
    Idle,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Replace the sender's live draft (last write wins; empty clears it).
    Draft {
        text: String,
    },
    /// Report `active` or `idle`; `online`/`offline` are set by the server.
    Presence {
        state: PresenceState,
    },
    Ping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello {
        protocol_version: u32,
        thread_id: String,
        username: String,
    },
    Event {
        event: EventEnvelope,
    },
    /// Backfill reached the log head; live events follow.
    CaughtUp {
        last_seq: i64,
    },
    Draft {
        username: String,
        text: String,
        updated_at: i64,
    },
    Presence {
        username: String,
        state: PresenceState,
    },
    /// Access was revoked (membership, link or session); the socket closes.
    Revoked,
    Error {
        error: String,
    },
    Pong,
}

// ---------------------------------------------------------------------------
// Runner WebSocket: GET /runner/ws (runner credential)
// ---------------------------------------------------------------------------

/// A pending turn authored by the runner's owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingTurn {
    pub command_id: String,
    pub thread_id: String,
    pub seq: i64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerReportKind {
    AgentEvent,
    ChangesSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerClientFrame {
    /// Conditionally claim a pending turn authored by this runner's owner.
    Claim {
        command_id: String,
    },
    /// Append an event for a turn this connection claimed.
    Report {
        command_id: String,
        kind: RunnerReportKind,
        payload: Value,
    },
    Finish {
        command_id: String,
        outcome: TurnOutcome,
        #[serde(default)]
        result: Value,
    },
    Ping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerServerFrame {
    Hello {
        protocol_version: u32,
        runner_id: i64,
        owner: String,
    },
    CommandAvailable {
        turn: PendingTurn,
    },
    Claimed {
        turn: PendingTurn,
    },
    ClaimRejected {
        command_id: String,
        reason: String,
    },
    Reported {
        command_id: String,
        seq: i64,
    },
    Finished {
        command_id: String,
        state: CommandState,
        seq: i64,
    },
    /// The runner credential was revoked; the socket closes.
    Revoked,
    Error {
        error: String,
    },
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_use_snake_case_type_tags_and_deny_unknown_tags() {
        let frame = ClientFrame::Draft { text: "hi".into() };
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            serde_json::json!({"type":"draft","text":"hi"})
        );
        assert!(serde_json::from_str::<ClientFrame>(r#"{"type":"shutdown"}"#).is_err());
        let claim: RunnerClientFrame =
            serde_json::from_str(r#"{"type":"claim","command_id":"c1"}"#).unwrap();
        assert_eq!(
            claim,
            RunnerClientFrame::Claim {
                command_id: "c1".into()
            }
        );
    }

    #[test]
    fn requests_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<SendRequest>(r#"{"command_id":"c","text":"t","runner":"x"}"#)
                .is_err()
        );
    }
}
