use super::*;

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

/// Reply to `DiscoverCollabServer`. `discovery` is `None` for a service
/// that predates discovery (assumed collab protocol v2, no vNext
/// capabilities); `profile` is `None` when negotiation failed (`error`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollabServerInfo {
    pub server_url: String,
    pub discovery: Option<ServerDiscovery>,
    pub profile: Option<NegotiatedProfile>,
    pub error: Option<String>,
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
    /// Items this daemon has yet to publish (history backfill, comments,
    /// children); delivered in order as soon as the service is reachable.
    #[serde(default)]
    pub pending_publish: u32,
}

/// Another participant's diff of a shared thread, as their daemon published
/// it after a turn (or when the thread was shared). Bounded: `truncated`
/// marks a cut diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteChangesSnapshot {
    pub author: String,
    pub captured_at: DateTime<Utc>,
    pub base: ChangeBase,
    pub repos: Vec<RepoChanges>,
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
    /// Display name of the API provider for a qualified id
    /// (`provider::model`); `None` for Claude Code models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
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

pub(super) fn plain_kind() -> SubthreadKind {
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
