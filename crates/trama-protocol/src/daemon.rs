use super::*;

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
    /// Reply to `DiscoverCollabServer`.
    CollabServerInfo(CollabServerInfo),
    /// Reply to `SharePreflight`, and to a blocked `CreateShare`.
    SharePreflightReport(SharePreflightReport),
    /// Reply to `CreateShare`.
    ShareCreated(ShareSnapshot),
    /// Reply to `ListShares`.
    Shares(Vec<ShareSnapshot>),
    /// Reply to `RequestShareJoin`.
    ShareJoinRequested {
        share_id: String,
        device_id: String,
    },
    /// Reply to `ListShareJoinRequests`.
    ShareJoinRequests {
        share_id: String,
        requests: Vec<ShareJoinRequest>,
    },
    /// Reply to `ApproveShareJoin`.
    ShareJoinApproved {
        share_id: String,
        device_id: String,
        key_epoch: u64,
    },
    /// Reply to `CompleteShareJoin`.
    ShareJoined(ShareSnapshot),
    /// Reply to `PublishShareSnapshot`.
    ShareSnapshotPublished {
        share_id: String,
        version: u64,
    },
    /// Reply to `AcquireShareLease`.
    ShareLease(ShareLeaseSnapshot),
    /// Reply to `ReleaseShareLease`.
    ShareLeaseReleased {
        share_id: String,
        thread_id: ThreadId,
    },
    /// Reply to `ListShareLeases`.
    ShareLeases {
        share_id: String,
        leases: Vec<ShareLeaseSnapshot>,
    },
    /// A vNext durable share event that passed shape and replay checks,
    /// sent to the subscribers of `thread_id` (thread-scoped kinds) or to
    /// every connection (share-wide kinds). `cipher_payload` stays opaque.
    ShareEvent {
        thread_id: Option<ThreadId>,
        event: ShareEventEnvelope,
    },
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
    /// Reply to `GetRemoteChanges`, and sent to subscribers whenever another
    /// participant publishes a new snapshot. Newest first.
    RemoteChanges {
        thread_id: ThreadId,
        snapshots: Vec<RemoteChangesSnapshot>,
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
    /// Reply to `GetDiskUsage`: one item per checkout present on disk,
    /// largest private size first.
    DiskUsage {
        workspace_id: Option<WorkspaceId>,
        items: Vec<DiskUsageItem>,
    },
    /// Reply to `ArchiveThread`. `removed` lists the checkouts deleted from
    /// disk (empty when `delete_checkouts` was false).
    ThreadArchived {
        thread: ThreadSnapshot,
        removed: Vec<CheckoutRemoval>,
    },
    ThreadUnarchived(ThreadSnapshot),
    ThreadCheckoutsDeleted {
        thread_id: ThreadId,
        removed: Vec<CheckoutRemoval>,
    },
    /// `ArchiveThread` / `DeleteThreadCheckouts` without `force` found work
    /// that exists only in these checkouts. Nothing was changed.
    CheckoutRemovalRefused {
        thread_id: ThreadId,
        checkouts: Vec<CheckoutRemoval>,
    },
}
