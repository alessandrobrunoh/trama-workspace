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
//! adds anchored comments (create, list, reply, resolve, reopen, delete, send)
//! and delivers pending comments with `SendPrompt`. **7** adds the composer
//! backend: `@` mentionables, `/` skills, per-thread model selection, inline
//! terminals, and `SendPrompt.mentions` / `SendPrompt.terminals`. Each bump is
//! transport-only. Persisted domain ids and SQLite rows are not rewritten.
//!
//! A version mismatch fails the handshake (`Welcome.accepted = false`). Extra object
//! fields inside a known payload are ignored (serde default). Bump [`PROTOCOL_VERSION`]
//! again for further tag or payload shape changes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

pub use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource, UsageReport};
pub use trama_domain::comment::{AnchorStatus, CodeAnchor, CommentDelivery, CommentTarget};
pub use trama_domain::ids::{CommentId, MessageId, RepositoryId, ThreadId, WorkspaceId};
pub use trama_domain::message::{Actor, Message};
pub use trama_domain::subthread::{
    FileChange, FileChangeStatus, LandOutcome, PullFileOutcome, PullFileResult, RepoPullResult,
    ReviewVerdict,
};
pub use trama_domain::thread::ThreadKind;

/// Wire protocol version carried in `Hello` / `Welcome`.
///
/// Version 7 adds the composer backend: `ListMentionables`, `ListSkills`,
/// `SetThreadModel`, `ListModels`, inline terminals (`OpenTerminal`,
/// `TerminalInput`, `ResizeTerminal`, `CloseTerminal`, `ListTerminals`,
/// `ReplayTerminal`), `ThreadSnapshot.model`, and `SendPrompt` mentions and
/// terminal attachments.
/// Version 6 adds anchored comments: `CreateComment`, `ListComments`,
/// `ResolveComment`, `ReopenComment`, `DeleteComment`, `SendComments`, and
/// `SendPrompt` now delivers the thread's pending comments with the prompt.
/// Version 5 added subthreads (create/list, parent and kind in thread
/// snapshots), changes, pull changes, review verdicts and land results.
/// Version 4 added the Claude approval channel and 3 thread checkout
/// discovery. It does not migrate stored data; peers with a different version
/// are rejected at handshake time.
pub const PROTOCOL_VERSION: u32 = 7;

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
    /// Fork a subthread from a top-level thread: copies of the parent's
    /// checkouts (uncommitted and untracked changes included) and, when the
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
    Shutdown,
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
}

fn top_kind() -> ThreadKind {
    ThreadKind::Top
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
                r#"{{"type":"thread_migrated","payload":{{"id":"{thread}","title":"Fix auth","repo_count":1,"parent_thread_id":null,"kind":"top","review_verdict":null,"model":null}}}}"#
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
    fn protocol_version_is_seven() {
        assert_eq!(PROTOCOL_VERSION, 7);
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
