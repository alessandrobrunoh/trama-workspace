use super::*;

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
    /// Fetch `GET /.well-known/trama-collab` from `server_url` (or the
    /// connected service when `None`) and negotiate a capability profile.
    /// Needs no credential. Answered with `CollabServerInfo`.
    DiscoverCollabServer {
        #[serde(default)]
        server_url: Option<String>,
    },
    /// Check whether `thread_id` can be shared with `scope`: perimeter
    /// (scouts outside the share are refused), Git conflicts and secrets in
    /// files and Git objects to export. Answered with `SharePreflightReport`.
    SharePreflight {
        thread_id: ThreadId,
        scope: ShareScope,
    },
    /// Run the preflight again and, when clean, register the share on the
    /// connected service, create its MLS group, grant `members` and publish
    /// the first snapshot. Answered with `ShareCreated`, or
    /// `SharePreflightReport` when blocked.
    CreateShare {
        thread_id: ThreadId,
        scope: ShareScope,
        #[serde(default)]
        members: Vec<ShareInvite>,
    },
    /// Answered with `Shares`.
    ListShares,
    /// Joiner: publish this device's key package for `share_id` (the user
    /// must already be granted). Answered with `ShareJoinRequested`.
    RequestShareJoin {
        share_id: String,
    },
    /// Owner: devices waiting for admission. Answered with
    /// `ShareJoinRequests`.
    ListShareJoinRequests {
        share_id: String,
    },
    /// Owner: add the device to the share's MLS group (Welcome to it,
    /// commit to the members). Answered with `ShareJoinApproved`.
    ApproveShareJoin {
        share_id: String,
        device_id: String,
    },
    /// Joiner: once approved, join from the Welcome, fetch the manifests
    /// and create the local threads in `workspace_id`. Answered with
    /// `ShareJoined`.
    CompleteShareJoin {
        share_id: String,
        workspace_id: WorkspaceId,
    },
    /// Publish the next snapshot (manifests) of a share this device owns or
    /// writes. Answered with `ShareSnapshotPublished`.
    PublishShareSnapshot {
        share_id: String,
    },
    /// Exclusive writer lease on a shared thread. Answered with
    /// `ShareLease`.
    AcquireShareLease {
        share_id: String,
        thread_id: ThreadId,
        #[serde(default = "default_lease_ttl")]
        ttl_secs: u64,
    },
    /// Answered with `ShareLeaseReleased`.
    ReleaseShareLease {
        share_id: String,
        thread_id: ThreadId,
    },
    /// Answered with `ShareLeases`.
    ListShareLeases {
        share_id: String,
    },
    /// Owner only: revokes `username`'s grant, removes every device of
    /// theirs from the MLS group (a `membership_changed` commit each) and
    /// republishes the snapshot at the new epoch. Answered with
    /// `ShareMemberRemoved`.
    RemoveShareMember {
        share_id: String,
        username: String,
    },
    /// Members, roles and devices. Answered with `ShareMembers`.
    ListShareMembers {
        share_id: String,
    },
    /// Per-message share state of a local thread (only messages with a
    /// recorded state). Answered with `ShareHistoryStatus`.
    GetShareHistoryStatus {
        thread_id: ThreadId,
    },
    /// Which share (if any) a local thread belongs to, with this device's
    /// role and the current writer lease. Answered with `ThreadShare`.
    GetThreadShare {
        thread_id: ThreadId,
    },
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
    /// The changes snapshots other participants published for a shared
    /// thread (their diff, not this machine's). Answered with
    /// `RemoteChanges`.
    GetRemoteChanges {
        thread_id: ThreadId,
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
    /// Disk used by the managed checkouts of `workspace_id` (every workspace
    /// when `None`). Answered with `DiskUsage`.
    GetDiskUsage {
        #[serde(default)]
        workspace_id: Option<WorkspaceId>,
    },
    /// Archive a thread: it leaves the active list, its conversation and
    /// history stay in TramaDB. With `delete_checkouts` its checkouts are
    /// removed from disk after their state (commits, uncommitted and
    /// untracked files) is saved as a snapshot outside them; they are
    /// recreated from it on `UnarchiveThread`. Refused while a turn runs, and
    /// (unless `force`) when a checkout holds work that is not in the source
    /// repository: answered with `CheckoutRemovalRefused` listing it.
    /// Otherwise answered with `ThreadArchived`.
    ArchiveThread {
        thread_id: ThreadId,
        #[serde(default)]
        delete_checkouts: bool,
        #[serde(default)]
        force: bool,
    },
    /// Bring an archived thread back, recreating removed checkouts.
    /// Answered with `ThreadUnarchived`.
    UnarchiveThread {
        thread_id: ThreadId,
    },
    /// Remove a thread's checkouts to free disk without archiving it (same
    /// rules as `ArchiveThread { delete_checkouts: true }`). They are
    /// recreated the next time the thread needs them. Answered with
    /// `ThreadCheckoutsDeleted` or `CheckoutRemovalRefused`.
    DeleteThreadCheckouts {
        thread_id: ThreadId,
        #[serde(default)]
        force: bool,
    },
    Shutdown,
}
