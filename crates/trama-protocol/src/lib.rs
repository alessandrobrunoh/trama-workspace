//! Versioned JSON-lines IPC between the desktop UI, the local daemon, and (later) runners.
//!
//! # Framing
//!
//! Each message is a single JSON object encoded as UTF-8 and terminated by a newline
//! (`\n`). There is no length prefix and no embedded raw newlines inside a value
//! (`serde_json::to_string` emits compact JSON). A transport is any reliable byte
//! stream (Unix domain socket today; TCP/SSH for remote runners later).
//!
//! Decode one message per line; ignore empty lines. A line that is not valid JSON
//! or does not match a known schema is a protocol error.
//!
//! # Tagging
//!
//! [`ClientMessage`] and [`DaemonMessage`] are **adjacently tagged** serde enums:
//! `{"type":"<snake_case_variant>","payload": ... }`. Unit variants omit `payload`.
//!
//! # Unknown-tag policy
//!
//! Unknown `type` tags are **denied**. Serde rejects them at deserialize time; the
//! daemon must not silently ignore a variant it does not understand. Because of that,
//! a new variant is a breaking handshake change: an older peer would accept `Hello`
//! and then fail the command. [`PROTOCOL_VERSION`] **2** added workspace creation,
//! repository membership, and legacy-thread migration. **3** added thread
//! checkout discovery. **4** added the approval channel
//! ([`ClientMessage::ApprovalHook`], [`DaemonMessage::ApprovalHookDecision`])
//! and the `session_info` / `approval_resolved` agent events. **5** added
//! subthreads, changes, pull changes, review verdicts and land results. **6**
//! added anchored comments (create, list, reply, resolve, reopen, delete, send)
//! and delivers pending comments with `SendPrompt`. **7** added the composer
//! backend: `@` mentionables, `/` skills, per-thread model selection, inline
//! terminals, and `SendPrompt.mentions` / `SendPrompt.terminals`. **8** added
//! shared threads through a `trama-collab` service (connect, share, join, list,
//! drafts, presence, mirrored remote activity). **9** adds Trama subagents (the
//! MCP helper's [`ClientMessage::McpToolCall`] /
//! [`DaemonMessage::McpToolResult`], [`DaemonMessage::SubagentStatusChanged`],
//! `depth` and `subagent` in thread snapshots, message annotations), nesting
//! at any depth, and [`ClientMessage::ForkThread`] at a message. **10** adds
//! [`DaemonMessage::TurnActivity`]: a turn's stored tool calls and thinking,
//! replayed on subscribe before the message that ends the turn. Each bump is
//! transport-only. Persisted domain ids and SQLite rows are not rewritten.
//!
//! A version mismatch fails the handshake (`Welcome.accepted = false`). Extra object
//! fields inside a known payload are ignored (serde default). Bump [`PROTOCOL_VERSION`]
//! again for further tag or payload shape changes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

pub use trama_domain::activity::ActivityItem;
pub use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource, UsageReport};
pub use trama_domain::comment::{AnchorStatus, CodeAnchor, CommentDelivery, CommentTarget};
pub use trama_domain::ids::{CommentId, MessageId, RepositoryId, ThreadId, WorkspaceId};
pub use trama_domain::message::{Actor, Message, MessageAnnotation};
pub use trama_domain::subagent::{SpawnedBy, SubagentProfile, SubagentStatus};
pub use trama_domain::subthread::{
    FileChange, FileChangeStatus, LandOutcome, PullFileOutcome, PullFileResult, RepoPullResult,
    ReviewVerdict,
};
pub use trama_domain::thread::ThreadKind;

/// Shared-thread wire types reused from the collab service.
pub use trama_collab_protocol::{
    AccessLevel as CollabAccess, EventEnvelope as CollabEventEnvelope,
    EventKind as CollabEventKind, MemberInfo as CollabMember, MemberVia as CollabMemberVia,
    PresenceState as CollabPresenceState,
};

/// Wire protocol version carried in `Hello` / `Welcome`.
///
/// Version 10 adds `TurnActivity`: the bounded tool calls, thinking and
/// subagent lifecycle of each stored turn (and of the turn in flight), sent
/// on subscribe so tool views survive restarts.
/// Version 9 added subagents spawned through Trama's MCP tools
/// (`McpToolCall`, `McpToolResult`, `SubagentStatusChanged`, `depth` and
/// `subagent` in `ThreadSnapshot`, `Message::annotation`), subthreads of
/// subthreads at any depth, and `ForkThread` (fork at a message: the
/// conversation up to it and the files of that turn's checkpoint).
/// Version 8 added shared threads: `ConnectCollab`, `DisconnectCollab`,
/// `GetCollabStatus`, `ShareThread`, `GetSharedThread`,
/// `RemoveSharedThreadMember`, `JoinSharedThread`, `ListSharedThreads`,
/// `UnlinkSharedThread`, `SetCollabDraft`, `SetCollabPresence`, and the
/// matching `Collab*` / `SharedThread*` daemon messages. `SendPrompt` on a
/// linked thread submits the turn to the collab service; this daemon's runner
/// then runs it locally (ADR 0008).
/// Version 7 added the composer backend: `ListMentionables`, `ListSkills`,
/// `SetThreadModel`, `ListModels`, inline terminals (`OpenTerminal`,
/// `TerminalInput`, `ResizeTerminal`, `CloseTerminal`, `ListTerminals`,
/// `ReplayTerminal`), `ThreadSnapshot.model`, and `SendPrompt` mentions and
/// terminal attachments.
/// Version 6 added anchored comments: `CreateComment`, `ListComments`,
/// `ResolveComment`, `ReopenComment`, `DeleteComment`, `SendComments`, and
/// `SendPrompt` now delivers the thread's pending comments with the prompt.
/// Version 5 added subthreads (create/list, parent and kind in thread
/// snapshots), changes, pull changes, review verdicts and land results.
/// Version 4 added the Claude approval channel and 3 thread checkout
/// discovery. It does not migrate stored data; peers with a different version
/// are rejected at handshake time.
pub const PROTOCOL_VERSION: u32 = 10;

/// Environment variable that relocates the whole app-data directory (socket,
/// database, config, legacy checkouts). Use it for isolated test instances
/// instead of overriding `HOME`: the `claude` child still needs the real
/// `HOME` to find the user's Claude Code login.
pub const DATA_DIR_ENV: &str = "TRAMA_DATA_DIR";

/// `$TRAMA_DATA_DIR`, else `$HOME/Library/Application Support/TramaWorkspace`.
pub fn default_app_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/TramaWorkspace"))
}

/// Encode `value` as one JSON line (compact JSON plus a trailing `\n`).
pub fn encode_json_line<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Decode a single JSON line. A trailing `\n` / `\r\n` is tolerated.
pub fn decode_json_line<T: DeserializeOwned>(line: &str) -> serde_json::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']))
}

/// UI → daemon (and later runner).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    ListWorkspaces,
    ListThreads {
        workspace_id: WorkspaceId,
    },
    CreateThread {
        workspace_id: WorkspaceId,
        title: String,
    },
    SubscribeThread {
        thread_id: ThreadId,
    },
    UnsubscribeThread {
        thread_id: ThreadId,
    },
    /// Run a turn. The thread's pending comments are delivered with it: the
    /// agent receives a comment block followed by `text`, and the stored user
    /// message holds exactly what was sent.
    ///
    /// `mentions` are expanded into a short `Referenced:` preamble (absolute
    /// checkout paths, thread excerpts, names); `terminals` attach a plain-text
    /// snapshot of each terminal's recent output. A `text` starting with `/`
    /// is a Claude Code slash command or skill: it is sent first and
    /// unchanged, attachments follow it, and pending comments wait for the
    /// next prompt.
    SendPrompt {
        thread_id: ThreadId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<PromptMention>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        terminals: Vec<String>,
    },
    Interrupt {
        thread_id: ThreadId,
    },
    RespondApproval {
        thread_id: ThreadId,
        request_id: String,
        decision: ApprovalDecision,
    },
    /// Create a workspace in the database. Does not edit `config.json`.
    CreateWorkspace {
        name: String,
    },
    /// Attach an existing local Git working tree. Not a remote clone.
    AddRepository {
        workspace_id: WorkspaceId,
        path: PathBuf,
        alias: String,
    },
    ListRepositories {
        workspace_id: WorkspaceId,
    },
    /// Explicit command. Prepares isolated clones and records checkout metadata.
    /// Does not run the agent and does not modify the source working trees.
    MigrateLegacyThread {
        thread_id: ThreadId,
    },
    GetThreadCheckouts {
        thread_id: ThreadId,
    },
    /// Sent only by the `trama-daemon approval-hook` helper that Claude Code
    /// runs as its `PermissionRequest` hook. `token` identifies the agent
    /// session (from the hook's environment). The daemon answers once with
    /// [`DaemonMessage::ApprovalHookDecision`]; closing the connection first
    /// cancels the request. UI clients never send this.
    ApprovalHook {
        token: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    /// Fork a subthread from any thread (subthreads included): copies of the
    /// parent's checkouts (uncommitted and untracked changes included) and, when the
    /// parent has an agent session, its conversation. Review and land
    /// subthreads start their first turn immediately; a plain subthread only
    /// when `initial_prompt` is set. Answered with `SubthreadCreated`.
    CreateSubthread {
        parent_thread_id: ThreadId,
        kind: SubthreadKind,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        initial_prompt: Option<String>,
    },
    /// Answered with `Subthreads`.
    ListSubthreads {
        parent_thread_id: ThreadId,
    },
    /// Unified diff per repository of the thread's current working state
    /// (untracked non-ignored files included) against `base`. Answered with
    /// `Changes`.
    GetChanges {
        thread_id: ThreadId,
        #[serde(default)]
        base: ChangeBase,
    },
    /// Apply what a subthread changed since its fork point to its parent's
    /// checkouts. Never commits. Answered with `ChangesPulled`.
    PullChanges {
        subthread_id: ThreadId,
    },
    /// Record a review subthread's verdict and post it to the parent thread.
    /// Answered with `ReviewVerdictPosted`.
    SubmitReviewVerdict {
        review_thread_id: ThreadId,
        verdict: ReviewVerdict,
    },
    /// Comment on a transcript span, code lines or a diff hunk of this thread
    /// (a subthread's comments stay in the subthread). For `Code` targets the
    /// daemon reads the file in the thread's checkout and fills the anchor's
    /// hashes itself; client-sent hashes are ignored. A reply sets
    /// `parent_comment_id` and repeats the parent's target. The comment is
    /// pending until the next `SendPrompt` or `SendComments`. Answered with
    /// `CommentCreated` (also broadcast to the thread's subscribers).
    CreateComment {
        thread_id: ThreadId,
        target: CommentTarget,
        body: String,
        #[serde(default)]
        parent_comment_id: Option<CommentId>,
    },
    /// Answered with `Comments`; code anchors are relocated against the
    /// thread's current checkout.
    ListComments {
        thread_id: ThreadId,
    },
    /// Resolve a root comment (its replies follow it). Resolved comments are
    /// never delivered. Answered with `CommentUpdated`.
    ResolveComment {
        comment_id: CommentId,
    },
    ReopenComment {
        comment_id: CommentId,
    },
    /// Delete one of your own pending comments (with its pending replies).
    /// Answered with `CommentDeleted`.
    DeleteComment {
        comment_id: CommentId,
    },
    /// Deliver the pending comments without extra text, as a turn of their
    /// own. Fails when nothing is pending.
    SendComments {
        thread_id: ThreadId,
    },
    /// `@` completion: files and directories of the thread's checkouts,
    /// threads of the same workspace, and people, fuzzy-ranked by `query`.
    /// `limit` defaults to 30 (max 200). Answered with `Mentionables`.
    ListMentionables {
        thread_id: ThreadId,
        #[serde(default)]
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },
    /// `/` completion: skills and commands visible to the thread's agent.
    /// Answered with `Skills`.
    ListSkills {
        thread_id: ThreadId,
    },
    /// Model for the thread's next turns (`None` or `"default"`: the
    /// workspace default). Applied on the next turn, resuming the session
    /// with `--model`. Answered with `ThreadModelChanged` (also broadcast).
    SetThreadModel {
        thread_id: ThreadId,
        #[serde(default)]
        model: Option<String>,
    },
    /// Answered with `Models`.
    ListModels,
    /// Start an interactive terminal (a real PTY) in the thread's checkout
    /// (`repository_id`, else the first one). `command` runs through the
    /// user's shell; without it an interactive shell starts. Output goes to
    /// the thread's subscribers. Answered with `TerminalOpened`.
    OpenTerminal {
        thread_id: ThreadId,
        #[serde(default)]
        repository_id: Option<RepositoryId>,
        #[serde(default)]
        command: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Keystrokes for the terminal, UTF-8 text as typed (control characters
    /// included, e.g. `"\u0003"` for Ctrl-C).
    TerminalInput {
        terminal_id: String,
        data: String,
    },
    ResizeTerminal {
        terminal_id: String,
        cols: u16,
        rows: u16,
    },
    /// Kill the terminal (if running) and forget it.
    CloseTerminal {
        terminal_id: String,
    },
    /// Answered with `Terminals` (running and exited terminals of the thread).
    ListTerminals {
        thread_id: ThreadId,
    },
    /// Re-send the bounded scrollback as one `TerminalOutput` with
    /// `replay = true`, e.g. after reconnecting.
    ReplayTerminal {
        terminal_id: String,
    },
    /// Sign in to a `trama-collab` service (loopback `http://` only) and
    /// start this daemon's runner. The session token and the runner token are
    /// stored owner-only in the app dir and never logged; the password is
    /// not stored. Without `runner_token` the daemon provisions one for
    /// `username` (`POST /runners`). Answered with `CollabStatus`.
    ConnectCollab {
        server_url: String,
        username: String,
        password: String,
        #[serde(default)]
        runner_token: Option<String>,
    },
    /// Stop the runner and every shared-thread socket. `forget` also logs the
    /// session out and deletes the stored credentials. Links and mirrored
    /// history stay. Answered with `CollabStatus`.
    DisconnectCollab {
        #[serde(default)]
        forget: bool,
    },
    /// Answered with `CollabStatus`.
    GetCollabStatus,
    /// Share a top-level thread: create its collab thread on first use
    /// (access `invited`), then apply `access` when set and invite the
    /// `invite` usernames (owner only). Setting `link` (again) returns a fresh
    /// link token once and revokes the previous one; setting `invited`
    /// removes members who joined by link. Answered with `SharedThread`.
    ShareThread {
        thread_id: ThreadId,
        #[serde(default)]
        access: Option<CollabAccess>,
        #[serde(default)]
        invite: Vec<String>,
    },
    /// Members and access of a linked thread. Answered with `SharedThread`.
    GetSharedThread {
        thread_id: ThreadId,
    },
    /// Owner only. Answered with `SharedThread`.
    RemoveSharedThreadMember {
        thread_id: ThreadId,
        username: String,
    },
    /// Join a shared thread (as an invitee, or with its link token) into a
    /// new local thread of `workspace_id`, with this workspace's isolated
    /// checkouts. Answered with `SharedThreadJoined`.
    JoinSharedThread {
        workspace_id: WorkspaceId,
        collab_thread_id: String,
        #[serde(default)]
        link_token: Option<String>,
    },
    /// Shared threads the signed-in user can open, with their local thread
    /// when linked. Answered with `SharedThreads`.
    ListSharedThreads,
    /// Stop mirroring a thread locally. Its history stays; the collab thread
    /// and its members are unchanged. Answered with `SharedThreadUnlinked`.
    UnlinkSharedThread {
        thread_id: ThreadId,
    },
    /// Relay the composer draft to the thread's other participants. Never
    /// stored; empty clears it.
    SetCollabDraft {
        thread_id: ThreadId,
        text: String,
    },
    /// `active` or `idle`.
    SetCollabPresence {
        thread_id: ThreadId,
        state: CollabPresenceState,
    },
    /// Fork a child of `thread_id`. Without `at_message_id` this is
    /// `CreateSubthread` (current files and conversation). With it, the
    /// child's conversation is the parent's transcript up to the end of the
    /// turn that message belongs to (an assistant message ends its own
    /// turn; for any other message, the last turn completed before it), and
    /// its files are that turn's checkpoint, not the parent's current state.
    /// The message must be the thread's own (not inherited). Answered with
    /// `SubthreadCreated` (also broadcast to the parent's subscribers).
    ForkThread {
        thread_id: ThreadId,
        #[serde(default)]
        at_message_id: Option<MessageId>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        initial_prompt: Option<String>,
        #[serde(default = "plain_kind")]
        kind: SubthreadKind,
    },
    /// Sent only by the `trama-daemon mcp` helper that Claude Code runs as a
    /// stdio MCP server. `token` identifies the calling agent session (from
    /// the helper's environment); every tool acts on that session's thread.
    /// `tool_use_id` is Claude's id for the call when the CLI sent it. The
    /// daemon answers once with [`DaemonMessage::McpToolResult`]; closing the
    /// connection first cancels a waiting call. UI clients never send this.
    McpToolCall {
        token: String,
        tool: String,
        #[serde(default)]
        arguments: serde_json::Value,
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    Shutdown,
}

/// The local runner's connection to the collab service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollabRunnerState {
    /// Not configured, or disconnected on request.
    Off,
    Connecting,
    /// Claims and runs this user's own turns.
    Online,
    /// Stopped: the credential was revoked or belongs to another user, or
    /// the session expired. `last_error` says why; reconnect to recover.
    Failed,
}

/// Reply to the `*Collab*` commands. Never carries a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollabStatusSnapshot {
    pub server_url: Option<String>,
    pub username: Option<String>,
    /// A session token is stored and was not rejected yet.
    pub signed_in: bool,
    pub runner: CollabRunnerState,
    pub linked_threads: u32,
    pub last_error: Option<String>,
}

/// A local thread's shared state. `link_token` is set only in the reply that
/// (re)enabled link access; it is shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedThreadSnapshot {
    pub thread_id: ThreadId,
    pub collab_thread_id: String,
    pub title: String,
    pub owner: String,
    pub access: CollabAccess,
    pub members: Vec<CollabMember>,
    pub link_token: Option<String>,
}

/// One entry of `SharedThreads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedThreadListing {
    pub collab_thread_id: String,
    pub title: String,
    pub owner: String,
    pub access: CollabAccess,
    /// How the signed-in user is a member.
    pub via: CollabMemberVia,
    pub last_seq: i64,
    pub local_thread_id: Option<ThreadId>,
}

/// What an `@` mention refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    File,
    Directory,
    Thread,
    Person,
}

/// One `@` completion. `value` is opaque: send it back unchanged in
/// `SendPrompt.mentions` (files and directories: `<repository_id>:<path>`,
/// threads: the thread id, people: a person id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mentionable {
    pub kind: MentionKind,
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub value: String,
}

/// A mention carried by `SendPrompt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMention {
    pub kind: MentionKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// The thread's checkout (`.claude/skills`, `.agents/skills`,
    /// `.claude/commands`).
    Project,
    /// `~/.claude/skills`, `~/.claude/commands`.
    User,
    /// A Claude Code plugin under `~/.claude/plugins`.
    Plugin,
    /// Built into Claude Code, or reported by the agent without a file Trama
    /// could find.
    Builtin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    Skill,
    Command,
}

/// One `/` completion. Send `/<name> [args]` as the prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source: SkillSource,
    pub kind: SkillKind,
}

/// A selectable model. `id` `"default"` clears the thread override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
}

/// How a terminal's process ended. `signal` is set when it was killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// A terminal as listed by `ListTerminals`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub terminal_id: String,
    pub thread_id: ThreadId,
    pub repository_id: RepositoryId,
    pub cwd: PathBuf,
    pub command: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// Sequence number of the latest output chunk (0: none yet).
    pub last_seq: u64,
    /// `None` while running.
    pub exit_status: Option<TerminalExitStatus>,
}

fn plain_kind() -> SubthreadKind {
    SubthreadKind::Plain
}

/// Where a comment is in its lifecycle. Serialized with a `kind` tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommentState {
    /// Not yet sent to the agent.
    Pending,
    /// Sent with the user prompt `message_id` (turn `turn`);
    /// `answer_message_id` is set once that turn's reply is stored.
    Delivered(CommentDelivery),
    /// Resolved by a person; `delivery` keeps where it was answered, if it
    /// was delivered.
    Resolved {
        resolved_at: DateTime<Utc>,
        delivery: Option<CommentDelivery>,
    },
}

/// One comment as the UI shows it. Replies are separate snapshots that carry
/// `parent_comment_id`; resolution lives on the root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommentSnapshot {
    pub id: CommentId,
    pub thread_id: ThreadId,
    pub parent_comment_id: Option<CommentId>,
    pub author: Actor,
    pub target: CommentTarget,
    pub body: String,
    /// The anchored passage captured at creation.
    pub excerpt: Option<String>,
    pub created_at: DateTime<Utc>,
    pub state: CommentState,
    /// Code targets only: where the anchor is in the current checkout.
    /// `orphaned` comments stay visible but are never reattached.
    pub anchor_status: Option<AnchorStatus>,
}

/// The kind of subthread to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubthreadKind {
    Plain,
    Review,
    Land,
}

impl SubthreadKind {
    pub fn thread_kind(self) -> ThreadKind {
        match self {
            Self::Plain => ThreadKind::Sub,
            Self::Review => ThreadKind::Review,
            Self::Land => ThreadKind::Land,
        }
    }
}

/// What `GetChanges` diffs against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeBase {
    /// Where the thread started: the base revision of a top-level thread's
    /// checkout, or a subthread's fork point (the parent's full working state
    /// when the subthread was created).
    #[default]
    ThreadBase,
    /// The checkout's last commit (`HEAD`): uncommitted changes only.
    Head,
    /// The working state when the latest agent turn started.
    LastTurn,
}

/// One repository's changes. `base_revision` is the resolved base commit.
/// `diff` is `git diff --no-color -M` text (binary contents are not inlined)
/// and is cut at 8 MiB when `truncated`; `files` is always complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoChanges {
    pub repository_id: RepositoryId,
    pub title: String,
    pub base_revision: String,
    pub diff: String,
    pub truncated: bool,
    pub files: Vec<FileChange>,
}

/// Daemon → UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DaemonMessage {
    Welcome {
        protocol_version: u32,
        daemon_version: String,
        accepted: bool,
    },
    Workspaces(Vec<WorkspaceSnapshot>),
    Threads(Vec<ThreadSnapshot>),
    ThreadCreated(ThreadSnapshot),
    MessageAppended {
        thread_id: ThreadId,
        message: Message,
    },
    AgentEventEnvelope {
        thread_id: ThreadId,
        event: AgentEvent,
    },
    UsageSnapshot {
        thread_id: ThreadId,
        totals: UsageReport,
    },
    Error {
        code: String,
        message: String,
    },
    WorkspaceCreated(WorkspaceSnapshot),
    Repositories {
        workspace_id: WorkspaceId,
        repositories: Vec<RepositorySnapshot>,
    },
    RepositoryAdded {
        workspace_id: WorkspaceId,
        repository: RepositorySnapshot,
    },
    ThreadMigrated(ThreadSnapshot),
    /// Reply to [`ClientMessage::ApprovalHook`], sent only on that hook's
    /// connection. `message` explains a denial to the agent; `interrupt`
    /// asks the agent to stop the turn (set when the user interrupted).
    ApprovalHookDecision {
        decision: ApprovalDecision,
        message: Option<String>,
        interrupt: bool,
    },
    ThreadCheckouts {
        thread_id: ThreadId,
        checkouts: Vec<CheckoutSnapshot>,
    },
    /// Reply to `CreateSubthread`; also broadcast to the parent's subscribers.
    SubthreadCreated(ThreadSnapshot),
    Subthreads {
        parent_thread_id: ThreadId,
        threads: Vec<ThreadSnapshot>,
    },
    Changes {
        thread_id: ThreadId,
        base: ChangeBase,
        repos: Vec<RepoChanges>,
    },
    /// Per-file outcome of `PullChanges`; also broadcast to the parent's
    /// subscribers. Conflicts are `PullFileOutcome::Conflict` entries.
    ChangesPulled {
        subthread_id: ThreadId,
        parent_thread_id: ThreadId,
        repos: Vec<RepoPullResult>,
    },
    /// A review verdict was recorded. Sent to the requester and the
    /// subscribers of both threads; the parent also gets a persisted system
    /// message (replayed on subscribe).
    ReviewVerdictPosted {
        review_thread_id: ThreadId,
        parent_thread_id: ThreadId,
        verdict: ReviewVerdict,
    },
    /// A land subthread reported its result (broadcast to the subscribers of
    /// both threads; the parent also gets a persisted system message).
    LandResult {
        land_thread_id: ThreadId,
        parent_thread_id: ThreadId,
        outcome: LandOutcome,
        summary: String,
    },
    /// A comment was created. Sent to the thread's subscribers, and to the
    /// requester when it is not subscribed.
    CommentCreated(CommentSnapshot),
    /// A comment changed state (delivered, answered, resolved, reopened, or
    /// pending again after a failed send). Same audience as `CommentCreated`.
    CommentUpdated(CommentSnapshot),
    /// Reply to `ListComments`: every comment of the thread, oldest first.
    Comments {
        thread_id: ThreadId,
        comments: Vec<CommentSnapshot>,
    },
    /// Pending comments were deleted (a root with its replies).
    CommentDeleted {
        thread_id: ThreadId,
        comment_ids: Vec<CommentId>,
    },
    /// Reply to `ListMentionables`, best match first.
    Mentionables {
        thread_id: ThreadId,
        query: String,
        items: Vec<Mentionable>,
    },
    /// Reply to `ListSkills`, sorted by name.
    Skills {
        thread_id: ThreadId,
        items: Vec<SkillInfo>,
    },
    /// Reply to `SetThreadModel`; also broadcast to the thread's subscribers.
    ThreadModelChanged {
        thread_id: ThreadId,
        model: Option<String>,
    },
    /// Reply to `ListModels`.
    Models {
        items: Vec<ModelInfo>,
    },
    /// Reply to `OpenTerminal`.
    TerminalOpened {
        thread_id: ThreadId,
        terminal: TerminalSnapshot,
    },
    /// Raw PTY output, base64 (standard alphabet, padded) so escape sequences
    /// and split UTF-8 survive. `seq` increases by one per chunk. Sent to the
    /// thread's subscribers; `replay` marks a `ReplayTerminal` answer that
    /// carries the whole retained scrollback.
    TerminalOutput {
        thread_id: ThreadId,
        terminal_id: String,
        data: String,
        seq: u64,
        #[serde(default)]
        replay: bool,
    },
    /// The terminal's process ended (sent to the thread's subscribers).
    TerminalExited {
        thread_id: ThreadId,
        terminal_id: String,
        status: TerminalExitStatus,
    },
    /// Reply to `ListTerminals`.
    Terminals {
        thread_id: ThreadId,
        terminals: Vec<TerminalSnapshot>,
    },
    /// Reply to `ConnectCollab`, `DisconnectCollab` and `GetCollabStatus`.
    CollabStatus(CollabStatusSnapshot),
    /// Reply to `ShareThread`, `GetSharedThread`, `RemoveSharedThreadMember`.
    SharedThread(SharedThreadSnapshot),
    /// Reply to `JoinSharedThread`: the new local thread, already mirroring.
    SharedThreadJoined {
        thread: ThreadSnapshot,
        shared: SharedThreadSnapshot,
    },
    SharedThreads(Vec<SharedThreadListing>),
    SharedThreadUnlinked {
        thread_id: ThreadId,
    },
    /// Reply to `SendPrompt` on a linked thread: the turn was submitted and
    /// runs when this daemon's runner claims it (immediately while online).
    /// The user message appears when the turn starts.
    CollabTurnQueued {
        thread_id: ThreadId,
        command_id: String,
    },
    /// A durable event from another participant (or another client of this
    /// user), sent to the thread's subscribers. Remote user and assistant
    /// messages, comments, verdicts, change summaries and ambiguous turns
    /// are also persisted as transcript messages (actor `collab:<username>`)
    /// and replayed on subscribe; this frame carries the structured form.
    CollabEvent {
        thread_id: ThreadId,
        event: CollabEventEnvelope,
    },
    /// Live agent output of another participant's turn (that participant's
    /// runner). Not stored; the final assistant message is.
    CollabAgentEvent {
        thread_id: ThreadId,
        author: String,
        command_id: String,
        event: AgentEvent,
    },
    /// Another participant's live draft (not stored; empty = cleared).
    CollabDraft {
        thread_id: ThreadId,
        username: String,
        text: String,
    },
    CollabPresence {
        thread_id: ThreadId,
        username: String,
        state: CollabPresenceState,
    },
    /// The signed-in user lost access to the shared thread; mirroring
    /// stopped. Local history stays.
    CollabAccessRevoked {
        thread_id: ThreadId,
    },
    /// Reply to [`ClientMessage::McpToolCall`], sent only on that helper's
    /// connection. `text` is the tool result shown to the agent.
    McpToolResult {
        is_error: bool,
        text: String,
    },
    /// A subagent's task changed state. Sent to the subscribers of the
    /// subagent and of its parent.
    SubagentStatusChanged {
        thread_id: ThreadId,
        parent_thread_id: ThreadId,
        status: SubagentStatus,
        detail: Option<String>,
    },
    /// Sent on subscribe only (live clients already saw the events): the
    /// tool calls, thinking and subagent lifecycle of the turn that
    /// `message_id` ends, right before that message's `MessageAppended`.
    /// `message_id: None` is the turn in flight (what it did so far), sent
    /// after the history; its later steps arrive as agent events. Inputs and
    /// outputs are bounded ([`trama_domain::activity`]); unfinished calls
    /// of a stored turn have `finished: false`.
    TurnActivity {
        thread_id: ThreadId,
        message_id: Option<MessageId>,
        items: Vec<ActivityItem>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutSnapshot {
    pub repository_id: RepositoryId,
    pub title: String,
    pub source_path: PathBuf,
    pub checkout_path: Option<PathBuf>,
    pub status: CheckoutStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutStatus {
    Ready,
    NeedsPreparation,
    Missing,
    Invalid,
}

/// Minimal workspace view for IPC. Not the domain `Workspace` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: WorkspaceId,
    pub title: String,
    pub repo_count: u32,
}

/// Minimal thread view for IPC. Not the domain `Thread` aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub id: ThreadId,
    pub title: String,
    pub repo_count: u32,
    /// Set for subthreads; the UI nests them under this thread.
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(default = "top_kind")]
    pub kind: ThreadKind,
    /// Latest verdict of a review subthread.
    #[serde(default)]
    pub review_verdict: Option<ReviewVerdict>,
    /// The thread's model override; `None` uses the workspace default.
    #[serde(default)]
    pub model: Option<String>,
    /// Number of ancestors (0 for a top-level thread). Nesting has no limit;
    /// `parent_thread_id` alone is enough to rebuild the tree.
    #[serde(default)]
    pub depth: u32,
    /// Set for threads spawned by another thread's agent (Trama subagents).
    #[serde(default)]
    pub subagent: Option<SubagentSnapshot>,
}

fn top_kind() -> ThreadKind {
    ThreadKind::Top
}

/// How a subagent thread was spawned and where its task is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub spawned_by: SpawnedBy,
    pub status: SubagentStatus,
    /// Failure reason, or what the last sync into the parent did.
    pub detail: Option<String>,
}

/// One workspace membership. `title` is the membership alias when set.
/// `path` is the canonical local working tree, not a remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    pub id: RepositoryId,
    pub title: String,
    pub path: PathBuf,
    pub default_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::agent::AgentEvent;
    use trama_domain::ids::ThreadId;
    use trama_domain::message::Message;

    fn roundtrip_eq<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value);
        let again = serde_json::to_string(&back).unwrap();
        assert_eq!(again, json);
    }

    fn roundtrip_json<T>(value: &T)
    where
        T: Serialize + DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
        let line = encode_json_line(value).unwrap();
        assert_eq!(*line.last().unwrap(), b'\n');
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        let decoded: T = decode_json_line(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::to_value(&decoded).unwrap()
        );
    }

    fn workspace_id() -> WorkspaceId {
        WorkspaceId::from("01hworkspace000000000000001")
    }

    fn thread_id() -> ThreadId {
        ThreadId::from("01hthread0000000000000000001")
    }

    fn snapshot_ws() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: workspace_id(),
            title: "LaFraternita".into(),
            repo_count: 2,
        }
    }

    fn snapshot_th() -> ThreadSnapshot {
        ThreadSnapshot {
            id: thread_id(),
            title: "Fix auth".into(),
            repo_count: 1,
            parent_thread_id: None,
            kind: ThreadKind::Top,
            review_verdict: None,
            model: None,
            depth: 0,
            subagent: None,
        }
    }

    fn snapshot_repo() -> RepositorySnapshot {
        RepositorySnapshot {
            id: RepositoryId::from("01hrepo000000000000000000001"),
            title: "app".into(),
            path: std::path::PathBuf::from("/src/app"),
            default_enabled: true,
        }
    }

    #[test]
    fn client_message_roundtrip_every_kind() {
        let tid = thread_id();
        let wid = workspace_id();
        let kinds = [
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_name: "trama-desktop".into(),
            },
            ClientMessage::ListWorkspaces,
            ClientMessage::ListThreads {
                workspace_id: wid.clone(),
            },
            ClientMessage::CreateThread {
                workspace_id: wid.clone(),
                title: "New thread".into(),
            },
            ClientMessage::SubscribeThread {
                thread_id: tid.clone(),
            },
            ClientMessage::UnsubscribeThread {
                thread_id: tid.clone(),
            },
            ClientMessage::SendPrompt {
                thread_id: tid.clone(),
                text: "hello".into(),
                mentions: Vec::new(),
                terminals: Vec::new(),
            },
            ClientMessage::Interrupt {
                thread_id: tid.clone(),
            },
            ClientMessage::RespondApproval {
                thread_id: tid.clone(),
                request_id: "req-1".into(),
                decision: ApprovalDecision::AllowOnce,
            },
            ClientMessage::CreateWorkspace {
                name: "LaFraternita".into(),
            },
            ClientMessage::AddRepository {
                workspace_id: wid,
                path: std::path::PathBuf::from("/src/app"),
                alias: "app".into(),
            },
            ClientMessage::ListRepositories {
                workspace_id: workspace_id(),
            },
            ClientMessage::MigrateLegacyThread { thread_id: tid },
            ClientMessage::ApprovalHook {
                token: "tok".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            },
            ClientMessage::Shutdown,
        ];
        for msg in &kinds {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 15);
    }

    #[test]
    fn daemon_message_roundtrip_every_kind() {
        let tid = thread_id();
        let message = Message::new_user(tid.clone(), "alice", "hi");
        let kinds = [
            DaemonMessage::Welcome {
                protocol_version: PROTOCOL_VERSION,
                daemon_version: "0.1.0".into(),
                accepted: true,
            },
            DaemonMessage::Workspaces(vec![snapshot_ws()]),
            DaemonMessage::Threads(vec![snapshot_th()]),
            DaemonMessage::ThreadCreated(snapshot_th()),
            DaemonMessage::MessageAppended {
                thread_id: tid.clone(),
                message,
            },
            DaemonMessage::AgentEventEnvelope {
                thread_id: tid.clone(),
                event: AgentEvent::TextDelta {
                    text: "chunk".into(),
                },
            },
            DaemonMessage::UsageSnapshot {
                thread_id: tid,
                totals: UsageReport {
                    model: "mock".into(),
                    input_tokens: 3,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 1,
                },
            },
            DaemonMessage::Error {
                code: "not_found".into(),
                message: "thread missing".into(),
            },
            DaemonMessage::WorkspaceCreated(snapshot_ws()),
            DaemonMessage::Repositories {
                workspace_id: workspace_id(),
                repositories: vec![snapshot_repo()],
            },
            DaemonMessage::RepositoryAdded {
                workspace_id: workspace_id(),
                repository: snapshot_repo(),
            },
            DaemonMessage::ThreadMigrated(snapshot_th()),
            DaemonMessage::ApprovalHookDecision {
                decision: ApprovalDecision::Deny,
                message: Some("denied".into()),
                interrupt: false,
            },
        ];
        for msg in &kinds {
            roundtrip_json(msg);
        }
        assert_eq!(kinds.len(), 13);
    }

    #[test]
    fn unknown_tags_are_denied() {
        let client =
            serde_json::from_str::<ClientMessage>(r#"{"type":"launch_nukes","payload":{}}"#);
        assert!(client.is_err(), "unknown client tags must be denied");
        let daemon =
            serde_json::from_str::<DaemonMessage>(r#"{"type":"secret_backdoor","payload":null}"#);
        assert!(daemon.is_err(), "unknown daemon tags must be denied");
        let client_err = client.unwrap_err().to_string();
        assert!(
            client_err.contains("unknown variant") || client_err.contains("launch_nukes"),
            "{client_err}"
        );
    }

    #[test]
    fn hello_json_shape_is_adjacently_tagged() {
        let json = serde_json::to_string(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "ui".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            format!(
                r#"{{"type":"hello","payload":{{"protocol_version":{PROTOCOL_VERSION},"client_name":"ui"}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListWorkspaces).unwrap(),
            r#"{"type":"list_workspaces"}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::Workspaces(vec![])).unwrap(),
            r#"{"type":"workspaces","payload":[]}"#
        );
    }

    #[test]
    fn onboarding_and_migration_tags_keep_existing_shape() {
        let workspace = workspace_id();
        let repo = snapshot_repo();
        assert_eq!(
            serde_json::to_string(&ClientMessage::CreateWorkspace {
                name: "Demo".into(),
            })
            .unwrap(),
            r#"{"type":"create_workspace","payload":{"name":"Demo"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::AddRepository {
                workspace_id: workspace.clone(),
                path: std::path::PathBuf::from("/src/app"),
                alias: "app".into(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"add_repository","payload":{{"workspace_id":"{workspace}","path":"/src/app","alias":"app"}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListRepositories {
                workspace_id: workspace.clone(),
            })
            .unwrap(),
            format!(r#"{{"type":"list_repositories","payload":{{"workspace_id":"{workspace}"}}}}"#)
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::MigrateLegacyThread {
                thread_id: thread_id(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"migrate_legacy_thread","payload":{{"thread_id":"{}"}}}}"#,
                thread_id()
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::WorkspaceCreated(snapshot_ws())).unwrap(),
            format!(
                r#"{{"type":"workspace_created","payload":{{"id":"{workspace}","title":"LaFraternita","repo_count":2}}}}"#
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::Repositories {
                workspace_id: workspace.clone(),
                repositories: vec![repo.clone()],
            })
            .unwrap(),
            format!(
                r#"{{"type":"repositories","payload":{{"workspace_id":"{workspace}","repositories":[{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}]}}}}"#,
                repo.id
            )
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::RepositoryAdded {
                workspace_id: workspace.clone(),
                repository: repo.clone(),
            })
            .unwrap(),
            format!(
                r#"{{"type":"repository_added","payload":{{"workspace_id":"{workspace}","repository":{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}}}}}"#,
                repo.id
            )
        );
        let thread = thread_id();
        assert_eq!(
            serde_json::to_string(&DaemonMessage::ThreadMigrated(snapshot_th())).unwrap(),
            format!(
                r#"{{"type":"thread_migrated","payload":{{"id":"{thread}","title":"Fix auth","repo_count":1,"parent_thread_id":null,"kind":"top","review_verdict":null,"model":null,"depth":0,"subagent":null}}}}"#
            )
        );
        roundtrip_eq(&repo);
    }

    #[test]
    fn approval_channel_json_shape() {
        assert_eq!(
            serde_json::to_string(&ClientMessage::ApprovalHook {
                token: "tok".into(),
                tool_name: "Bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
            })
            .unwrap(),
            r#"{"type":"approval_hook","payload":{"token":"tok","tool_name":"Bash","tool_input":{"command":"ls"}}}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonMessage::ApprovalHookDecision {
                decision: ApprovalDecision::AllowSession,
                message: None,
                interrupt: false,
            })
            .unwrap(),
            r#"{"type":"approval_hook_decision","payload":{"decision":"allow_session","message":null,"interrupt":false}}"#
        );
        let envelope = DaemonMessage::AgentEventEnvelope {
            thread_id: thread_id(),
            event: AgentEvent::ApprovalRequested {
                request_id: "apr-1".into(),
                tool_name: "Bash".into(),
                input_json: serde_json::json!({"command": "ls"}),
            },
        };
        roundtrip_json(&envelope);
    }

    #[test]
    fn app_dir_prefers_trama_data_dir_over_home() {
        // Single test touching these variables, so no cross-test races.
        let saved = (std::env::var_os(DATA_DIR_ENV), std::env::var_os("HOME"));
        unsafe {
            std::env::set_var("HOME", "/Users/someone");
            std::env::remove_var(DATA_DIR_ENV);
        }
        assert_eq!(
            default_app_dir(),
            Some(PathBuf::from(
                "/Users/someone/Library/Application Support/TramaWorkspace"
            ))
        );
        unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/trama-test") };
        assert_eq!(default_app_dir(), Some(PathBuf::from("/tmp/trama-test")));
        unsafe {
            match saved.0 {
                Some(value) => std::env::set_var(DATA_DIR_ENV, value),
                None => std::env::remove_var(DATA_DIR_ENV),
            }
            match saved.1 {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn protocol_version_is_ten() {
        assert_eq!(PROTOCOL_VERSION, 10);
    }

    #[test]
    fn turn_activity_contract_shape() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let stored = DaemonMessage::TurnActivity {
            thread_id: thread_id(),
            message_id: Some(MessageId::from("m1")),
            items: vec![
                ActivityItem::Thinking {
                    text: "plan".into(),
                    truncated: false,
                },
                ActivityItem::ToolCall {
                    tool_use_id: "toolu_1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    output: Some(serde_json::json!("a")),
                    is_error: false,
                    finished: true,
                    started_at: at,
                    finished_at: Some(at),
                    truncated: false,
                },
            ],
        };
        roundtrip_json(&stored);
        let json = serde_json::to_value(&stored).unwrap();
        assert_eq!(json["type"], "turn_activity");
        assert_eq!(json["payload"]["message_id"], "m1");
        assert_eq!(json["payload"]["items"][0]["kind"], "thinking");
        assert_eq!(json["payload"]["items"][1]["kind"], "tool_call");
        assert_eq!(
            json["payload"]["items"][1]["started_at"],
            "2026-01-01T00:00:00Z"
        );
        let in_flight: DaemonMessage = serde_json::from_value(serde_json::json!({
            "type": "turn_activity",
            "payload": {"thread_id": thread_id().to_string(), "message_id": null, "items": []}
        }))
        .unwrap();
        assert!(matches!(
            in_flight,
            DaemonMessage::TurnActivity {
                message_id: None,
                ..
            }
        ));
    }

    #[test]
    fn composer_contract_shapes() {
        let tid = thread_id();
        // Older clients omit the attachments; empty ones are not serialized.
        let plain: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"send_prompt","payload":{"thread_id": tid.to_string(), "text":"hi"}}))
        .unwrap();
        assert_eq!(
            plain,
            ClientMessage::SendPrompt {
                thread_id: tid.clone(),
                text: "hi".into(),
                mentions: vec![],
                terminals: vec![],
            }
        );
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"type":"send_prompt","payload":{"thread_id": tid.to_string(), "text":"hi"}})
        );
        let send = ClientMessage::SendPrompt {
            thread_id: tid.clone(),
            text: "look".into(),
            mentions: vec![PromptMention {
                kind: MentionKind::File,
                value: "repo1:src/lib.rs".into(),
            }],
            terminals: vec!["term-1".into()],
        };
        assert_eq!(
            serde_json::to_value(&send).unwrap()["payload"],
            serde_json::json!({"thread_id": tid.to_string(), "text":"look",
                "mentions":[{"kind":"file","value":"repo1:src/lib.rs"}],
                "terminals":["term-1"]})
        );
        let client = [
            send,
            ClientMessage::ListMentionables {
                thread_id: tid.clone(),
                query: "lib".into(),
                limit: Some(10),
            },
            ClientMessage::ListSkills {
                thread_id: tid.clone(),
            },
            ClientMessage::SetThreadModel {
                thread_id: tid.clone(),
                model: Some("claude-opus-5-5".into()),
            },
            ClientMessage::ListModels,
            ClientMessage::OpenTerminal {
                thread_id: tid.clone(),
                repository_id: None,
                command: Some("cargo test".into()),
                cols: 80,
                rows: 24,
            },
            ClientMessage::TerminalInput {
                terminal_id: "term-1".into(),
                data: "ls\r".into(),
            },
            ClientMessage::ResizeTerminal {
                terminal_id: "term-1".into(),
                cols: 120,
                rows: 40,
            },
            ClientMessage::CloseTerminal {
                terminal_id: "term-1".into(),
            },
            ClientMessage::ListTerminals {
                thread_id: tid.clone(),
            },
            ClientMessage::ReplayTerminal {
                terminal_id: "term-1".into(),
            },
        ];
        for msg in &client {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"list_mentionables","payload":{"thread_id": tid.to_string()}}))
        .unwrap();
        assert_eq!(
            minimal,
            ClientMessage::ListMentionables {
                thread_id: tid.clone(),
                query: String::new(),
                limit: None
            }
        );
        let open: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"open_terminal","payload":{"thread_id": tid.to_string(),"cols":80,"rows":24}}))
        .unwrap();
        assert!(matches!(
            open,
            ClientMessage::OpenTerminal {
                command: None,
                repository_id: None,
                ..
            }
        ));
        assert_eq!(
            serde_json::to_string(&ClientMessage::ListModels).unwrap(),
            r#"{"type":"list_models"}"#
        );

        let terminal = TerminalSnapshot {
            terminal_id: "term-1".into(),
            thread_id: tid.clone(),
            repository_id: RepositoryId::from("repo1"),
            cwd: PathBuf::from("/src/app/.trama/worktrees/t/c"),
            command: None,
            cols: 80,
            rows: 24,
            last_seq: 3,
            exit_status: Some(TerminalExitStatus {
                code: Some(0),
                signal: None,
            }),
        };
        let daemon = [
            DaemonMessage::Mentionables {
                thread_id: tid.clone(),
                query: "lib".into(),
                items: vec![Mentionable {
                    kind: MentionKind::Directory,
                    label: "src/".into(),
                    detail: Some("app".into()),
                    value: "repo1:src".into(),
                }],
            },
            DaemonMessage::Skills {
                thread_id: tid.clone(),
                items: vec![SkillInfo {
                    name: "land".into(),
                    description: Some("Land the branch".into()),
                    source: SkillSource::Project,
                    kind: SkillKind::Skill,
                }],
            },
            DaemonMessage::ThreadModelChanged {
                thread_id: tid.clone(),
                model: None,
            },
            DaemonMessage::Models {
                items: vec![ModelInfo {
                    id: "default".into(),
                    label: "Default".into(),
                }],
            },
            DaemonMessage::TerminalOpened {
                thread_id: tid.clone(),
                terminal: terminal.clone(),
            },
            DaemonMessage::TerminalOutput {
                thread_id: tid.clone(),
                terminal_id: "term-1".into(),
                data: "aGk=".into(),
                seq: 1,
                replay: false,
            },
            DaemonMessage::TerminalExited {
                thread_id: tid.clone(),
                terminal_id: "term-1".into(),
                status: TerminalExitStatus {
                    code: None,
                    signal: Some(9),
                },
            },
            DaemonMessage::Terminals {
                thread_id: tid.clone(),
                terminals: vec![terminal],
            },
        ];
        for msg in &daemon {
            roundtrip_json(msg);
        }
        assert_eq!(
            serde_json::to_value(&daemon[0]).unwrap(),
            serde_json::json!({"type":"mentionables","payload":{"thread_id": tid.to_string(),
                "query":"lib","items":[{"kind":"directory","label":"src/","detail":"app","value":"repo1:src"}]}})
        );
        assert_eq!(
            serde_json::to_value(&daemon[1]).unwrap()["payload"]["items"][0],
            serde_json::json!({"name":"land","description":"Land the branch","source":"project","kind":"skill"})
        );
        assert_eq!(
            serde_json::to_value(&daemon[5]).unwrap(),
            serde_json::json!({"type":"terminal_output","payload":{"thread_id": tid.to_string(),
                "terminal_id":"term-1","data":"aGk=","seq":1,"replay":false}})
        );
        assert_eq!(
            serde_json::to_value(&daemon[6]).unwrap()["payload"]["status"],
            serde_json::json!({"code":null,"signal":9})
        );
        let snapshot = serde_json::to_value(&daemon[4]).unwrap();
        assert_eq!(snapshot["payload"]["terminal"]["exit_status"]["code"], 0);
        assert_eq!(snapshot["payload"]["terminal"]["last_seq"], 3);
    }

    #[test]
    fn collab_contract_shapes() {
        let tid = thread_id();
        let client = [
            ClientMessage::ConnectCollab {
                server_url: "http://127.0.0.1:8787".into(),
                username: "alice".into(),
                password: "secret-password".into(),
                runner_token: None,
            },
            ClientMessage::DisconnectCollab { forget: true },
            ClientMessage::GetCollabStatus,
            ClientMessage::ShareThread {
                thread_id: tid.clone(),
                access: Some(CollabAccess::Link),
                invite: vec!["bob".into()],
            },
            ClientMessage::GetSharedThread {
                thread_id: tid.clone(),
            },
            ClientMessage::RemoveSharedThreadMember {
                thread_id: tid.clone(),
                username: "bob".into(),
            },
            ClientMessage::JoinSharedThread {
                workspace_id: workspace_id(),
                collab_thread_id: "ct1".into(),
                link_token: Some("tok".into()),
            },
            ClientMessage::ListSharedThreads,
            ClientMessage::UnlinkSharedThread {
                thread_id: tid.clone(),
            },
            ClientMessage::SetCollabDraft {
                thread_id: tid.clone(),
                text: "typing".into(),
            },
            ClientMessage::SetCollabPresence {
                thread_id: tid.clone(),
                state: CollabPresenceState::Idle,
            },
        ];
        for msg in &client {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(
            serde_json::to_value(&client[3]).unwrap(),
            serde_json::json!({"type":"share_thread","payload":{
                "thread_id": tid.to_string(), "access":"link", "invite":["bob"]}})
        );
        // Optional fields may be omitted.
        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"connect_collab","payload":{"server_url":"http://127.0.0.1:1",
            "username":"a","password":"p"}}))
        .unwrap();
        assert!(matches!(
            minimal,
            ClientMessage::ConnectCollab {
                runner_token: None,
                ..
            }
        ));
        assert_eq!(
            serde_json::from_value::<ClientMessage>(
                serde_json::json!({"type":"disconnect_collab","payload":{}})
            )
            .unwrap(),
            ClientMessage::DisconnectCollab { forget: false }
        );

        let shared = SharedThreadSnapshot {
            thread_id: tid.clone(),
            collab_thread_id: "ct1".into(),
            title: "Shared".into(),
            owner: "alice".into(),
            access: CollabAccess::Invited,
            members: vec![CollabMember {
                username: "bob".into(),
                via: CollabMemberVia::Invite,
            }],
            link_token: None,
        };
        let daemon = [
            DaemonMessage::CollabStatus(CollabStatusSnapshot {
                server_url: Some("http://127.0.0.1:8787".into()),
                username: Some("alice".into()),
                signed_in: true,
                runner: CollabRunnerState::Online,
                linked_threads: 1,
                last_error: None,
            }),
            DaemonMessage::SharedThread(shared.clone()),
            DaemonMessage::SharedThreadJoined {
                thread: snapshot_th(),
                shared,
            },
            DaemonMessage::SharedThreads(vec![SharedThreadListing {
                collab_thread_id: "ct1".into(),
                title: "Shared".into(),
                owner: "alice".into(),
                access: CollabAccess::Link,
                via: CollabMemberVia::Link,
                last_seq: 4,
                local_thread_id: None,
            }]),
            DaemonMessage::SharedThreadUnlinked {
                thread_id: tid.clone(),
            },
            DaemonMessage::CollabTurnQueued {
                thread_id: tid.clone(),
                command_id: "trd.x.y".into(),
            },
            DaemonMessage::CollabEvent {
                thread_id: tid.clone(),
                event: CollabEventEnvelope {
                    thread_id: "ct1".into(),
                    seq: 3,
                    kind: CollabEventKind::CommentCreated,
                    author: Some("bob".into()),
                    command_id: Some("c".into()),
                    created_at: 1,
                    payload: serde_json::json!({"body":"nit"}),
                },
            },
            DaemonMessage::CollabAgentEvent {
                thread_id: tid.clone(),
                author: "bob".into(),
                command_id: "c".into(),
                event: AgentEvent::TextDelta { text: "hi".into() },
            },
            DaemonMessage::CollabDraft {
                thread_id: tid.clone(),
                username: "bob".into(),
                text: "dra".into(),
            },
            DaemonMessage::CollabPresence {
                thread_id: tid.clone(),
                username: "bob".into(),
                state: CollabPresenceState::Online,
            },
            DaemonMessage::CollabAccessRevoked {
                thread_id: tid.clone(),
            },
        ];
        for msg in &daemon {
            roundtrip_json(msg);
        }
        let json = serde_json::to_value(&daemon[0]).unwrap();
        assert_eq!(json["type"], "collab_status");
        assert_eq!(json["payload"]["runner"], "online");
        assert_eq!(
            serde_json::to_value(&daemon[6]).unwrap()["payload"]["event"]["kind"],
            "comment_created"
        );
    }

    #[test]
    fn subthread_contract_shapes() {
        let parent = thread_id();
        let sub = ThreadId::from("01hsub000000000000000000001");
        let client = [
            ClientMessage::CreateSubthread {
                parent_thread_id: parent.clone(),
                kind: SubthreadKind::Review,
                title: Some("Review".into()),
                initial_prompt: None,
            },
            ClientMessage::ListSubthreads {
                parent_thread_id: parent.clone(),
            },
            ClientMessage::GetChanges {
                thread_id: sub.clone(),
                base: ChangeBase::LastTurn,
            },
            ClientMessage::PullChanges {
                subthread_id: sub.clone(),
            },
            ClientMessage::SubmitReviewVerdict {
                review_thread_id: sub.clone(),
                verdict: ReviewVerdict::RequestChanges {
                    summary: "add tests".into(),
                },
            },
        ];
        for msg in &client {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(
            serde_json::to_value(&client[0]).unwrap(),
            serde_json::json!({"type":"create_subthread","payload":{
                "parent_thread_id": parent.to_string(), "kind":"review",
                "title":"Review", "initial_prompt": null}})
        );
        // Optional fields and the default base may be omitted.
        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"get_changes","payload":{"thread_id": sub.to_string()}}))
        .unwrap();
        assert_eq!(
            minimal,
            ClientMessage::GetChanges {
                thread_id: sub.clone(),
                base: ChangeBase::ThreadBase
            }
        );
        assert_eq!(
            serde_json::to_value(&client[4]).unwrap()["payload"]["verdict"],
            serde_json::json!({"verdict":"request_changes","summary":"add tests"})
        );

        let child = ThreadSnapshot {
            id: sub.clone(),
            title: "Review".into(),
            repo_count: 1,
            parent_thread_id: Some(parent.clone()),
            kind: ThreadKind::Review,
            review_verdict: Some(ReviewVerdict::Approve),
            model: Some("claude-opus-5-5".into()),
            depth: 1,
            subagent: None,
        };
        let daemon = [
            DaemonMessage::SubthreadCreated(child.clone()),
            DaemonMessage::Subthreads {
                parent_thread_id: parent.clone(),
                threads: vec![child.clone()],
            },
            DaemonMessage::Changes {
                thread_id: sub.clone(),
                base: ChangeBase::ThreadBase,
                repos: vec![RepoChanges {
                    repository_id: RepositoryId::from("repo"),
                    title: "app".into(),
                    base_revision: "abc".into(),
                    diff: "diff --git a/x b/x\n".into(),
                    truncated: false,
                    files: vec![FileChange {
                        path: "x".into(),
                        old_path: None,
                        status: FileChangeStatus::Modified,
                        additions: Some(1),
                        deletions: Some(0),
                    }],
                }],
            },
            DaemonMessage::ChangesPulled {
                subthread_id: sub.clone(),
                parent_thread_id: parent.clone(),
                repos: vec![RepoPullResult {
                    repository_id: RepositoryId::from("repo"),
                    files: vec![PullFileResult {
                        path: "x".into(),
                        change: FileChangeStatus::Modified,
                        outcome: PullFileOutcome::Conflict {
                            markers_written: true,
                            reason: "both".into(),
                        },
                    }],
                }],
            },
            DaemonMessage::ReviewVerdictPosted {
                review_thread_id: sub.clone(),
                parent_thread_id: parent.clone(),
                verdict: ReviewVerdict::Approve,
            },
            DaemonMessage::LandResult {
                land_thread_id: sub,
                parent_thread_id: parent,
                outcome: LandOutcome::Landed,
                summary: "merged".into(),
            },
        ];
        for msg in &daemon {
            roundtrip_json(msg);
        }
        let json = serde_json::to_value(&daemon[0]).unwrap();
        assert_eq!(json["type"], "subthread_created");
        assert_eq!(json["payload"]["kind"], "review");
        assert_eq!(json["payload"]["review_verdict"]["verdict"], "approve");
        // Snapshots from older peers without the new fields decode as top-level.
        let legacy: ThreadSnapshot = serde_json::from_value(serde_json::json!({
            "id": "t", "title": "old", "repo_count": 1}))
        .unwrap();
        assert_eq!(legacy.kind, ThreadKind::Top);
        assert!(legacy.parent_thread_id.is_none());
        assert_eq!(legacy.depth, 0);
        assert!(legacy.subagent.is_none());
    }

    #[test]
    fn subagent_and_fork_contract_shapes() {
        let parent = thread_id();
        let child = ThreadId::from("01hchild00000000000000000001");
        let fork = ClientMessage::ForkThread {
            thread_id: parent.clone(),
            at_message_id: Some(MessageId::from("m7")),
            title: Some("Try B".into()),
            initial_prompt: None,
            kind: SubthreadKind::Plain,
        };
        roundtrip_eq(&fork);
        roundtrip_json(&fork);
        assert_eq!(
            serde_json::to_value(&fork).unwrap(),
            serde_json::json!({"type":"fork_thread","payload":{
                "thread_id": parent.to_string(), "at_message_id":"m7",
                "title":"Try B","initial_prompt":null,"kind":"plain"}})
        );
        // Only the thread id is required.
        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"fork_thread","payload":{"thread_id": parent.to_string()}}))
        .unwrap();
        assert_eq!(
            minimal,
            ClientMessage::ForkThread {
                thread_id: parent.clone(),
                at_message_id: None,
                title: None,
                initial_prompt: None,
                kind: SubthreadKind::Plain,
            }
        );
        let call = ClientMessage::McpToolCall {
            token: "tok".into(),
            tool: "spawn_subagent".into(),
            arguments: serde_json::json!({"title":"t","prompt":"p"}),
            tool_use_id: Some("toolu_1".into()),
        };
        roundtrip_eq(&call);
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            serde_json::json!({"type":"mcp_tool_call","payload":{"token":"tok",
                "tool":"spawn_subagent","arguments":{"title":"t","prompt":"p"},
                "tool_use_id":"toolu_1"}})
        );
        let result = DaemonMessage::McpToolResult {
            is_error: false,
            text: "ok".into(),
        };
        roundtrip_json(&result);
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({"type":"mcp_tool_result","payload":{"is_error":false,"text":"ok"}})
        );
        let status = DaemonMessage::SubagentStatusChanged {
            thread_id: child.clone(),
            parent_thread_id: parent.clone(),
            status: SubagentStatus::WaitingToSync,
            detail: None,
        };
        roundtrip_json(&status);
        assert_eq!(
            serde_json::to_value(&status).unwrap()["payload"]["status"],
            "waiting_to_sync"
        );
        let snapshot = ThreadSnapshot {
            id: child.clone(),
            title: "Worker".into(),
            repo_count: 1,
            parent_thread_id: Some(parent.clone()),
            kind: ThreadKind::Sub,
            review_verdict: None,
            model: None,
            depth: 3,
            subagent: Some(SubagentSnapshot {
                spawned_by: SpawnedBy {
                    parent_thread_id: parent.clone(),
                    tool_use_id: Some("toolu_1".into()),
                    profile: SubagentProfile::Worker,
                },
                status: SubagentStatus::Running,
                detail: None,
            }),
        };
        roundtrip_eq(&snapshot);
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["depth"], 3);
        assert_eq!(
            json["subagent"],
            serde_json::json!({"spawned_by":{"parent_thread_id": parent.to_string(),
                "tool_use_id":"toolu_1","profile":"worker"},"status":"running","detail":null})
        );
        let mut message = Message::new_user(child.clone(), "agent", "hi");
        message.annotation = Some(MessageAnnotation::AgentMessageReceived {
            from_thread_id: parent.clone(),
            from_title: "Main".into(),
        });
        let appended = DaemonMessage::MessageAppended {
            thread_id: child,
            message,
        };
        roundtrip_json(&appended);
        assert_eq!(
            serde_json::to_value(&appended).unwrap()["payload"]["message"]["annotation"]["kind"],
            "agent_message_received"
        );
    }

    #[test]
    fn comment_contract_shapes() {
        let tid = thread_id();
        let cid = CommentId::from("01hcomment00000000000000001");
        let code = CommentTarget::Code {
            anchor: CodeAnchor {
                repository_id: RepositoryId::from("repo"),
                path: PathBuf::from("src/lib.rs"),
                revision: 0,
                start_line: 3,
                end_line: 4,
                text_hash: None,
                context_hash: None,
            },
        };
        let client = [
            ClientMessage::CreateComment {
                thread_id: tid.clone(),
                target: code.clone(),
                body: "rename".into(),
                parent_comment_id: None,
            },
            ClientMessage::CreateComment {
                thread_id: tid.clone(),
                target: CommentTarget::MessageRange {
                    message_id: MessageId::from("m1"),
                    start_byte: 0,
                    end_byte: 5,
                },
                body: "why?".into(),
                parent_comment_id: Some(cid.clone()),
            },
            ClientMessage::ListComments {
                thread_id: tid.clone(),
            },
            ClientMessage::ResolveComment {
                comment_id: cid.clone(),
            },
            ClientMessage::ReopenComment {
                comment_id: cid.clone(),
            },
            ClientMessage::DeleteComment {
                comment_id: cid.clone(),
            },
            ClientMessage::SendComments {
                thread_id: tid.clone(),
            },
        ];
        for msg in &client {
            roundtrip_eq(msg);
            roundtrip_json(msg);
        }
        assert_eq!(
            serde_json::to_value(&client[0]).unwrap(),
            serde_json::json!({"type":"create_comment","payload":{
                "thread_id": tid.to_string(),
                "target": {"target":"code","anchor":{
                    "repository_id":"repo","path":"src/lib.rs","revision":0,
                    "start_line":3,"end_line":4,"text_hash":null,"context_hash":null}},
                "body":"rename","parent_comment_id":null}})
        );
        assert_eq!(
            serde_json::to_value(&client[6]).unwrap(),
            serde_json::json!({"type":"send_comments","payload":{"thread_id": tid.to_string()}})
        );
        // A UI may omit the parent, the revision and the hashes.
        let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"create_comment","payload":{
                "thread_id": tid.to_string(),
                "target":{"target":"diff_hunk","repository_id":"repo","path":"a.rs",
                    "base_revision":"abc","hunk_header":"@@ -1,2 +1,3 @@"},
                "body":"hm"}}))
        .unwrap();
        assert!(matches!(
            minimal,
            ClientMessage::CreateComment {
                parent_comment_id: None,
                target: CommentTarget::DiffHunk { .. },
                ..
            }
        ));
        let lean: ClientMessage = serde_json::from_value(serde_json::json!({
            "type":"create_comment","payload":{
                "thread_id": tid.to_string(),
                "target":{"target":"code","anchor":{"repository_id":"repo",
                    "path":"src/lib.rs","start_line":1,"end_line":1}},
                "body":"hm"}}))
        .unwrap();
        assert!(matches!(lean, ClientMessage::CreateComment { .. }));

        let created_at = "2026-10-12T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let delivery = CommentDelivery {
            message_id: MessageId::from("m2"),
            turn: 3,
            delivered_at: created_at,
            answer_message_id: Some(MessageId::from("m3")),
        };
        let snapshot = CommentSnapshot {
            id: cid.clone(),
            thread_id: tid.clone(),
            parent_comment_id: None,
            author: Actor::Human { id: "user".into() },
            target: code,
            body: "rename".into(),
            excerpt: Some("let a = 1;\nlet b = 2;".into()),
            created_at,
            state: CommentState::Delivered(delivery.clone()),
            anchor_status: Some(AnchorStatus::Resolved { line: 5 }),
        };
        let json = serde_json::to_value(DaemonMessage::CommentUpdated(snapshot.clone())).unwrap();
        assert_eq!(json["type"], "comment_updated");
        assert_eq!(
            json["payload"]["state"],
            serde_json::json!({"kind":"delivered","message_id":"m2","turn":3,
                "delivered_at":"2026-10-12T10:00:00Z","answer_message_id":"m3"})
        );
        assert_eq!(
            json["payload"]["anchor_status"],
            serde_json::json!({"resolved":{"line":5}})
        );
        assert_eq!(
            json["payload"]["author"],
            serde_json::json!({"kind":"human","id":"user"})
        );
        let mut pending = snapshot.clone();
        pending.state = CommentState::Pending;
        pending.anchor_status = Some(AnchorStatus::Orphaned);
        let json = serde_json::to_value(&pending).unwrap();
        assert_eq!(json["state"], serde_json::json!({"kind":"pending"}));
        assert_eq!(json["anchor_status"], "orphaned");
        let mut resolved = snapshot.clone();
        resolved.state = CommentState::Resolved {
            resolved_at: created_at,
            delivery: Some(delivery),
        };
        assert_eq!(
            serde_json::to_value(&resolved).unwrap()["state"]["kind"],
            "resolved"
        );
        for state in [&snapshot, &pending, &resolved] {
            roundtrip_eq(state);
        }
        let daemon = [
            DaemonMessage::CommentCreated(pending),
            DaemonMessage::CommentUpdated(resolved.clone()),
            DaemonMessage::Comments {
                thread_id: tid.clone(),
                comments: vec![snapshot, resolved],
            },
            DaemonMessage::CommentDeleted {
                thread_id: tid.clone(),
                comment_ids: vec![cid.clone()],
            },
        ];
        for msg in &daemon {
            roundtrip_json(msg);
        }
        assert_eq!(
            serde_json::to_value(&daemon[3]).unwrap(),
            serde_json::json!({"type":"comment_deleted","payload":{
                "thread_id": tid.to_string(), "comment_ids":[cid.to_string()]}})
        );
    }

    #[test]
    fn thread_checkout_discovery_contract_uses_snake_case_status() {
        let message = DaemonMessage::ThreadCheckouts {
            thread_id: ThreadId::from("thread-1"),
            checkouts: vec![CheckoutSnapshot {
                repository_id: RepositoryId::from("repo-1"),
                title: "backend".into(),
                source_path: PathBuf::from("/src/backend"),
                checkout_path: Some(PathBuf::from("/src/backend/.trama/worktrees/t/c")),
                status: CheckoutStatus::Ready,
                detail: None,
            }],
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "thread_checkouts");
        assert_eq!(json["payload"]["checkouts"][0]["status"], "ready");
        let decoded = serde_json::from_value::<DaemonMessage>(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), json);
    }
}
