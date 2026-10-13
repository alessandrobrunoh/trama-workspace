mod commenting;
mod lifecycle;
mod panel;
mod rail;
mod render;
#[cfg(test)]
mod tests;
mod transcript;
#[allow(unused_imports)]
pub(crate) use self::commenting::*;
#[allow(unused_imports)]
pub(crate) use self::lifecycle::*;
#[allow(unused_imports)]
pub(crate) use self::panel::*;
#[allow(unused_imports)]
pub(crate) use self::rail::*;
#[allow(unused_imports)]
pub(crate) use self::render::*;
#[allow(unused_imports)]
pub(crate) use self::transcript::*;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    dock::{BasePanel, Panel, PanelEvent},
    h_flex,
    kbd::Kbd,
    spinner::Spinner,
    text::{RangeHighlight, RenderedText, TextView, TextViewState},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use trama_domain::agent::{AgentEvent, ApprovalDecision, BillingSource, UsageReport};
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_domain::message::{ContentBlock, Message, Role};
use trama_protocol::{ClientMessage, CommentTarget, DaemonMessage};

use crate::agent::{ChatAgent, FakeAgent};
use crate::comments::{
    self, CardHandlers, CommentDraft, CommentGroup, CommentStore, Phase, PromptBlock,
};
use crate::composer::{BillingBadge, Composer, ComposerFooter};
use crate::composer_popup::ComposerPopup;
use crate::doc_text::{BlockSelection, BlockTexts, Mark};
use crate::document::{self, AuthorKey, BlockItem};
use crate::model_picker::ModelPicker;
use crate::net::{DaemonClient, SendStatus};
use crate::sidebar::ThreadStatus;
use crate::snapshot;
use crate::terminal_view::{self, TerminalEvent, TerminalPanel};
use crate::theme::{self, Palette};
use crate::tools::{self, DocCtx, Item, PatchLine, PatchPick, ToolKind, ToolTiming, ToolViewState};
use crate::turns::{self, Bookmarks, Span};
use gpui_kit::component::input::{Enter, Escape, IndentInline, MoveDown, MoveUp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnLifecycle {
    Idle,
    Running,
    Stopping,
    AwaitingAssistant,
}

/// Where an inline approval card stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalState {
    /// Waiting for the user.
    Pending,
    /// Answer sent; waiting for the daemon's `ApprovalResolved`.
    Sending(ApprovalDecision),
    /// The daemon resolved it, by this answer or automatically (`reason`).
    Resolved {
        decision: ApprovalDecision,
        reason: Option<String>,
    },
    /// The turn ended before a resolution arrived.
    Expired,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalCard {
    pub request_id: String,
    pub tool_name: String,
    pub input_json: serde_json::Value,
    pub state: ApprovalState,
}

impl ApprovalCard {
    pub fn is_open(&self) -> bool {
        matches!(
            self.state,
            ApprovalState::Pending | ApprovalState::Sending(_)
        )
    }
}

/// What the agent reported about its session (`AgentEvent::SessionInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub billing: BillingSource,
    pub api_key_source: Option<String>,
    pub agent_version: Option<String>,
    pub permission_mode: Option<String>,
}

pub enum ChatEntry {
    /// A prompt. `text` is the stored message text once `message_id` is set
    /// (with any delivered `<review-comments>` block), the typed prompt
    /// before that.
    User {
        text: String,
        message_id: Option<MessageId>,
    },
    Approval(ApprovalCard),
    Assistant {
        markdown: String,
        streaming: bool,
        message_id: Option<MessageId>,
    },
    ToolCall {
        tool_use_id: String,
        name: String,
        input_json: serde_json::Value,
        finished: bool,
        is_error: bool,
        expanded: bool,
        output: Option<serde_json::Value>,
    },
    /// Extended thinking, streamed or stored.
    Thinking {
        text: String,
    },
    /// A stored message that links another thread (subagent started or
    /// finished, agent message sent).
    Annotation(crate::subagents::AnnotationRow),
}

/// Requests the chat panel makes of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// Select this thread (⌘-click on a subagent line).
    OpenThread(ThreadId),
    /// Show this subagent's transcript next to this one (a click on an
    /// "Agent spawned" line).
    OpenInPane(ThreadId),
    /// Fork the selected thread at a message (hover action, ⌘K).
    Fork(crate::forks::ForkTarget),
}

/// The turn rail's interaction state.
#[derive(Default)]
struct RailState {
    /// Turn whose tick the pointer is over.
    hovered: Option<usize>,
    /// Scroll child index where each turn starts, from the last render.
    turn_rows: Vec<usize>,
    /// ⌥+scroll: trackpad distance not yet turned into a jump.
    pending: f32,
    last_jump: Option<std::time::Instant>,
}

/// Turn extents measured from the last layout, in content pixels.
struct RailGeometry {
    spans: Vec<Span>,
    content_height: f32,
    view: Span,
}

/// One contiguous slice of a persisted message, rendered selectable. A
/// message is split after each commented Markdown block so its threads can
/// render right under the passage.
struct Segment {
    /// Byte range of the message's rendered source.
    range: Range<usize>,
    state: Entity<TextViewState>,
    /// Highlights last handed to the state, and the parse they indexed.
    applied: Option<(RenderedText, Highlights)>,
}

/// Background colors over ranges of a segment's source.
type Highlights = Vec<(Range<usize>, Hsla)>;

/// A persisted message whose selections map back to `Message::text()`.
struct MessageView {
    /// The Markdown rendered (the message text after the comment block).
    source: String,
    segments: Vec<Segment>,
}

/// The commentable selection seen in the last render.
#[derive(Debug, Clone)]
enum SelectionHit {
    /// Text of a stored message: a byte range of `Message::text()`.
    Message {
        message_id: MessageId,
        range: Range<usize>,
    },
    /// Text of another block (activity, thinking, output, a subagent line).
    Block(BlockSelection),
}

/// Where the open comment composer sits in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DraftAnchor {
    /// A new comment, right under the Markdown block holding `range`
    /// (bytes of `Message::text()`), which stays highlighted meanwhile.
    Passage {
        message_id: MessageId,
        range: Range<usize>,
    },
    /// A new comment on a passage of a block that is not a message, under
    /// the block drawing text `key`.
    Block {
        key: String,
        kind: &'static str,
        excerpt: String,
        turn: usize,
    },
    /// A new comment on picked lines of an edit's diff.
    Patch {
        tool_use_id: String,
    },
    Reply(CommentId),
}

/// What the comment cards of one frame read.
struct CardCtx {
    groups: Vec<CommentGroup>,
    handlers: CardHandlers,
    /// First sentence of each stored answer, for "↩ answer" lines.
    answers: HashMap<MessageId, String>,
    /// This machine's user, for their avatar.
    me: Option<String>,
    /// A reply composer is drawn once, under the first card of its thread.
    reply_drawn: Cell<bool>,
}

/// Where the comment threads of one frame go.
#[derive(Default)]
struct Placement {
    /// Threads on each turn's blocks, in creation order.
    by_turn: HashMap<usize, Vec<usize>>,
    /// Passages of block comments (and of the open draft) to find in each
    /// turn's block texts.
    marks: HashMap<usize, Vec<Mark>>,
    /// Commented lines and threads under each edit, by tool use id.
    patches: HashMap<String, PatchComments>,
}

#[derive(Default)]
struct PatchComments {
    lines: HashSet<(usize, usize)>,
    groups: Vec<usize>,
}

struct ChatDraft {
    anchor: DraftAnchor,
    draft: CommentDraft,
}

pub struct ChatPanel {
    entries: Vec<ChatEntry>,
    comments: Entity<CommentStore>,
    message_views: HashMap<MessageId, MessageView>,
    /// Text length of persisted assistant messages whose entry shows exactly
    /// `Message::text()`; only those map selections to byte offsets.
    stored_len: HashMap<MessageId, usize>,
    selection: Option<SelectionHit>,
    /// Where the last selection gesture ended, in window coordinates.
    selection_point: Option<Point<Pixels>>,
    draft: Option<ChatDraft>,
    /// Threads the user opened or closed, overriding the default.
    thread_open: HashMap<CommentId, bool>,
    /// User messages whose delivered comment block is expanded.
    open_blocks: HashSet<MessageId>,
    /// Scroll to this message on the next render.
    reveal: Option<MessageId>,
    /// Briefly tinted after a reveal.
    flash: Option<MessageId>,
    _flash_task: Option<Task<()>>,
    _comments_observer: Subscription,
    composer: Composer,
    agent: Box<dyn ChatAgent>,
    client: DaemonClient,
    connected: bool,
    selected_thread: Option<ThreadId>,
    drafts: HashMap<ThreadId, String>,
    restore_thread_draft: bool,
    lifecycle: TurnLifecycle,
    final_assistant_persisted: bool,
    pending_prompt: Option<String>,
    seen_message_ids: HashSet<MessageId>,
    connection_error: Option<String>,
    pub status: AgentStatus,
    pub usage: Option<UsageReport>,
    pub cost_usd_micros: Option<u64>,
    pub turns: Option<u32>,
    pub model: Option<String>,
    pub session_info: Option<SessionInfo>,
    /// Bumped whenever a turn reaches a terminal event; observers use it to
    /// refresh views that depend on the checkout (the Changes drawer).
    pub completed_turns: u64,
    last_failed: bool,
    scroll: ScrollHandle,
    tool_views: ToolViewState,
    rail: RailState,
    bookmarks: Bookmarks,
    /// When each stored prompt was sent.
    prompt_times: HashMap<MessageId, chrono::DateTime<chrono::Utc>>,
    /// Threads the sidebar lists, for subagent links.
    known_threads: Vec<(ThreadId, String)>,
    /// Titles, subagent profiles and spawning tool calls of known threads.
    thread_index: crate::subagents::ThreadIndex,
    /// Where the stored messages can be forked at.
    fork_index: crate::forks::ForkIndex,
    /// Prompts another thread's agent sent, by message.
    received: HashMap<MessageId, crate::subagents::AnnotationRow>,
    /// Shared-thread state (drafts, presence, remote turns).
    collab: Option<Entity<crate::collab::CollabStore>>,
    /// Mirrored messages of other participants: author and role.
    remote_authors: HashMap<MessageId, (String, Role)>,
    /// `@` / `/` / `!` popups over the composer.
    popup: Entity<ComposerPopup>,
    model_picker: Entity<ModelPicker>,
    terminals: Entity<TerminalPanel>,
    /// Expanded terminal snapshots in sent prompts (message, block index).
    open_snapshots: HashSet<(MessageId, usize)>,
    /// Selectable text of the blocks that are not stored messages.
    block_texts: BlockTexts,
    /// Diff lines picked in an edit, and whether the mouse still drags.
    patch_pick: Option<PatchPick>,
    patch_dragging: bool,
    /// Takes keys while diff lines are picked (type to comment).
    doc_focus: FocusHandle,
    /// Turn comment groups opened or closed by hand.
    turn_comments_open: HashMap<usize, bool>,
    /// The selected thread's checkouts (repository id, root), to anchor
    /// comments on diff lines to code.
    checkouts: Vec<(String, std::path::PathBuf)>,
    /// Drawn in the side pane: no terminals, a subagent's transcript.
    pane_mode: bool,
    /// A scripted transcript (`TRAMA_DEV_DEMO`): comments show offline.
    demo: bool,
    _composer_extras: Vec<Subscription>,
    _enter: Subscription,
    _event_task: Option<Task<()>>,
}

impl EventEmitter<PanelEvent> for ChatPanel {}
impl EventEmitter<ChatEvent> for ChatPanel {}

impl Focusable for ChatPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.composer.input.focus_handle(cx)
    }
}

impl BasePanel for ChatPanel {
    fn panel_name(&self) -> &'static str {
        "Chat"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }
}

impl Panel for ChatPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Chat"
    }
}
