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
    _composer_extras: Vec<Subscription>,
    _enter: Subscription,
    _event_task: Option<Task<()>>,
}

impl ChatPanel {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        client: DaemonClient,
        comments: Entity<CommentStore>,
    ) -> Self {
        let composer = Composer::new(window, cx);
        let enter = composer.subscribe_enter(window, cx, |this, window, cx| {
            this.submit(window, cx);
        });
        let comments_observer = cx.observe(&comments, |_, _, cx| cx.notify());
        let popup_input = composer.input.clone();
        let popup_client = client.clone();
        let popup = cx.new(|cx| ComposerPopup::new(popup_client, popup_input, cx));
        let picker_client = client.clone();
        let model_picker = cx.new(|_| ModelPicker::new(picker_client));
        let terminal_client = client.clone();
        let terminals = cx.new(|cx| TerminalPanel::new(terminal_client, cx));
        let composer_extras = vec![
            cx.observe(&popup, |_, _, cx| cx.notify()),
            cx.observe(&model_picker, |_, _, cx| cx.notify()),
            cx.observe(&terminals, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &terminals,
                window,
                |this, _, event, window, cx| match event {
                    TerminalEvent::Hidden => this.focus_composer(window, cx),
                },
            ),
        ];

        Self {
            entries: Vec::new(),
            comments,
            message_views: HashMap::new(),
            stored_len: HashMap::new(),
            selection: None,
            selection_point: None,
            draft: None,
            thread_open: HashMap::new(),
            open_blocks: HashSet::new(),
            reveal: None,
            flash: None,
            _flash_task: None,
            _comments_observer: comments_observer,
            composer,
            agent: Box::new(FakeAgent),
            client,
            connected: false,
            selected_thread: None,
            drafts: HashMap::new(),
            restore_thread_draft: false,
            lifecycle: TurnLifecycle::Idle,
            final_assistant_persisted: false,
            pending_prompt: None,
            seen_message_ids: HashSet::new(),
            connection_error: None,
            status: AgentStatus::Idle,
            usage: None,
            cost_usd_micros: None,
            turns: None,
            model: None,
            session_info: None,
            completed_turns: 0,
            last_failed: false,
            scroll: ScrollHandle::new(),
            tool_views: ToolViewState::default(),
            rail: RailState::default(),
            bookmarks: Bookmarks::load(),
            prompt_times: HashMap::new(),
            known_threads: Vec::new(),
            thread_index: Default::default(),
            fork_index: Default::default(),
            received: HashMap::new(),
            collab: None,
            remote_authors: HashMap::new(),
            popup,
            model_picker,
            terminals,
            open_snapshots: HashSet::new(),
            block_texts: BlockTexts::default(),
            patch_pick: None,
            patch_dragging: false,
            doc_focus: cx.focus_handle(),
            turn_comments_open: HashMap::new(),
            checkouts: Vec::new(),
            pane_mode: false,
            _composer_extras: composer_extras,
            _enter: enter,
            _event_task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.lifecycle != TurnLifecycle::Idle
    }

    pub fn set_collab(&mut self, collab: Entity<crate::collab::CollabStore>) {
        self.collab = Some(collab);
    }

    /// The composer's text state, for relaying live drafts.
    pub fn composer_input(&self) -> Entity<gpui_kit::component::input::TextareaState> {
        self.composer.input.clone()
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.input.focus_handle(cx).focus(window, cx);
    }

    /// Approvals still waiting for the user, oldest first.
    pub fn open_approvals(&self) -> impl Iterator<Item = &ApprovalCard> {
        self.entries.iter().filter_map(|entry| match entry {
            ChatEntry::Approval(card) if card.state == ApprovalState::Pending => Some(card),
            _ => None,
        })
    }

    fn has_open_approval(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, ChatEntry::Approval(card) if card.is_open()))
    }

    /// Answer the oldest pending approval (keyboard shortcuts).
    pub fn answer_oldest_approval(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        let request_id = self
            .open_approvals()
            .next()
            .map(|card| card.request_id.clone());
        if let Some(request_id) = request_id {
            self.answer_approval(&request_id, decision, cx);
        }
    }

    pub fn answer_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.selected_thread.clone() else {
            return;
        };
        let Some(message) = answer_approval(&mut self.entries, &thread_id, request_id, decision)
        else {
            return;
        };
        if self.client.send(message) == SendStatus::Rejected {
            set_approval_state(&mut self.entries, request_id, ApprovalState::Pending);
            self.connection_error =
                Some("The daemon is not connected; the approval was not sent.".into());
        }
        cx.notify();
    }

    /// Take the composer draft (for "send to subthread").
    pub fn take_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
        let draft = self.composer.take_prompt(window, cx);
        cx.notify();
        draft
    }

    /// Dot shown next to the selected thread in the sidebar.
    pub fn thread_status(&self) -> ThreadStatus {
        if self.has_open_approval() {
            ThreadStatus::NeedsAttention
        } else if self.is_running() {
            ThreadStatus::Running
        } else if self.last_failed {
            ThreadStatus::Error
        } else {
            ThreadStatus::Idle
        }
    }

    /// Short label and color for the composer status pill.
    fn status_pill(&self, p: &Palette) -> (&'static str, Hsla) {
        if self.has_open_approval() {
            return ("Waiting for approval", p.warning);
        }
        match self.lifecycle {
            TurnLifecycle::Running => ("Running", p.accent),
            TurnLifecycle::Stopping => ("Stopping", p.warning),
            TurnLifecycle::AwaitingAssistant => ("Finishing", p.accent),
            TurnLifecycle::Idle if self.last_failed => ("Failed", p.danger),
            TurnLifecycle::Idle if !self.connected => ("Offline", p.text_faint),
            TurnLifecycle::Idle => ("Ready", p.success),
        }
    }

    pub fn set_connected(&mut self, connected: bool, cx: &mut Context<Self>) {
        let was_connected = self.connected;
        self.connected = connected;
        if connected && !was_connected && !self.is_running() {
            // Re-subscribe replay (stored tool activity included) will
            // refill; drop anything streamed while offline.
            self.clear_transcript();
        }
        if !connected && was_connected {
            self.connection_error =
                Some("Daemon disconnected; waiting to reconcile this turn.".into());
        }
        if connected {
            self.connection_error = None;
        }
        if connected && !was_connected {
            self.popup.update(cx, |popup, _| popup.reconnected());
            self.model_picker
                .update(cx, |picker, _| picker.reconnected());
            self.terminals
                .update(cx, |terminals, cx| terminals.reconnected(cx));
        }
        self.model_picker
            .update(cx, |picker, _| picker.enabled = connected);
        cx.notify();
    }

    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.selected_thread == thread {
            return;
        }
        if let Some(current_thread) = &self.selected_thread {
            let mut draft = self.composer.value(cx);
            if let Some(prompt) = self.pending_prompt.take()
                && !prompt.is_empty()
            {
                draft = if draft.is_empty() {
                    prompt
                } else {
                    format!("{prompt}\n{draft}")
                };
            }
            if !draft.is_empty() {
                self.drafts.insert(current_thread.clone(), draft);
            }
        }
        self.selected_thread = thread;
        self.restore_thread_draft = self.selected_thread.is_some();
        let (thread, connected) = (self.selected_thread.clone(), self.connected);
        self.popup
            .update(cx, |popup, cx| popup.set_thread(thread.clone(), cx));
        // The side pane only talks to its subagent: no terminals, and the
        // model is the parent's choice.
        if !self.pane_mode {
            self.model_picker
                .update(cx, |picker, cx| picker.set_thread(thread.clone(), cx));
            self.terminals.update(cx, |terminals, cx| {
                terminals.set_thread(thread, connected, cx)
            });
        }
        self.open_snapshots.clear();
        self.clear_transcript();
        self.draft = None;
        self.thread_open.clear();
        self.open_blocks.clear();
        self.connection_error = None;
        cx.notify();
    }

    fn clear_transcript(&mut self) {
        self.entries.clear();
        self.message_views.clear();
        self.stored_len.clear();
        self.selection = None;
        self.selection_point = None;
        self.reveal = None;
        self.flash = None;
        self.seen_message_ids.clear();
        self.lifecycle = TurnLifecycle::Idle;
        self.final_assistant_persisted = false;
        self.pending_prompt = None;
        self.status = AgentStatus::Idle;
        self.usage = None;
        self.cost_usd_micros = None;
        self.turns = None;
        self.model = None;
        self.session_info = None;
        self.last_failed = false;
        self._event_task = None;
        self.tool_views.clear();
        self.rail = RailState::default();
        self.prompt_times.clear();
        self.remote_authors.clear();
        self.fork_index.clear();
        self.received.clear();
        self.block_texts.clear();
        self.patch_pick = None;
        self.patch_dragging = false;
        self.turn_comments_open.clear();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let (lifecycle, message) = stop_transition(self.lifecycle, self.selected_thread.as_ref());
        if self.lifecycle == lifecycle {
            return;
        }
        if let Some(message) = message {
            self.client.send(message);
        }
        self.lifecycle = lifecycle;
        cx.notify();
    }

    pub fn restore_prompt(
        &mut self,
        thread_id: &ThreadId,
        prompt: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.thread_matches(thread_id) {
            return;
        }
        self.stage_prompt(thread_id, prompt);
        let restored = self.drafts.remove(thread_id).unwrap_or_default();
        self.composer.set_prompt(&restored, window, cx);
        self.restore_thread_draft = false;
        if self.pending_prompt.as_deref() == Some(prompt) {
            self.pending_prompt = None;
            if let Some(ChatEntry::User {
                text,
                message_id: None,
            }) = self.entries.last()
                && text == prompt
            {
                self.entries.pop();
            }
            self.lifecycle = TurnLifecycle::Idle;
            self.final_assistant_persisted = false;
            self.status = AgentStatus::Idle;
        }
        self.connection_error = Some("Message was not sent; the draft was restored.".into());
        cx.notify();
    }

    fn stage_prompt(&mut self, thread_id: &ThreadId, prompt: &str) {
        let current = self.drafts.remove(thread_id).unwrap_or_default();
        let restored = if current.trim().is_empty() {
            prompt.to_string()
        } else if current == prompt {
            current
        } else {
            format!("{prompt}\n{current}")
        };
        self.drafts.insert(thread_id.clone(), restored);
        self.restore_thread_draft = true;
    }

    pub fn handle_daemon(&mut self, message: DaemonMessage, cx: &mut Context<Self>) {
        if self.popup.update(cx, |popup, cx| popup.apply(&message, cx))
            || !self.pane_mode
                && (self
                    .model_picker
                    .update(cx, |picker, cx| picker.apply(&message, cx))
                    || self
                        .terminals
                        .update(cx, |terminals, cx| terminals.apply(&message, cx)))
        {
            return;
        }
        if let DaemonMessage::AgentEventEnvelope {
            thread_id,
            event: AgentEvent::CommandsReported { .. },
        } = &message
            && self.thread_matches(thread_id)
        {
            self.popup.update(cx, |popup, _| popup.refresh_skills());
        }
        match message {
            DaemonMessage::MessageAppended { thread_id, message } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                if !self.seen_message_ids.insert(message.id.clone()) {
                    return;
                }
                self.fork_index.note(&message, &thread_id);
                if let Some(row) = crate::subagents::annotation_row(&message) {
                    if crate::subagents::is_standalone(&message) {
                        // Shown even mid-turn: a subagent starts while its
                        // parent's turn is still running.
                        self.entries.push(ChatEntry::Annotation(row));
                        self.scroll.scroll_to_bottom();
                        cx.notify();
                        return;
                    }
                    self.received.insert(message.id.clone(), row);
                }
                if let crate::collab::Speaker::Remote(author) =
                    crate::collab::Speaker::of(&message.actor)
                {
                    self.append_remote_message(message, author);
                    self.scroll.scroll_to_bottom();
                    cx.notify();
                    return;
                }
                let is_assistant = message.role == Role::Assistant;
                self.append_stored_message(message);
                if is_assistant && self.lifecycle != TurnLifecycle::Idle {
                    self.final_assistant_persisted = true;
                }
                if is_assistant && self.lifecycle == TurnLifecycle::AwaitingAssistant {
                    self.lifecycle = stored_assistant_transition(self.lifecycle);
                    self.status = AgentStatus::Idle;
                }
                self.scroll.scroll_to_bottom();
                cx.notify();
            }
            DaemonMessage::AgentEventEnvelope { thread_id, event } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                if let Some(prompt) = take_rejected_prompt(&event, &mut self.pending_prompt) {
                    self.stage_prompt(&thread_id, &prompt);
                    if matches!(
                        self.entries.last(),
                        Some(ChatEntry::User { text, message_id: None }) if text == &prompt
                    ) {
                        self.entries.pop();
                    }
                }
                self.apply_event(event);
                self.scroll.scroll_to_bottom();
                cx.notify();
            }
            DaemonMessage::TurnActivity {
                thread_id,
                message_id,
                items,
            } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                self.apply_turn_activity(message_id, &items);
                cx.notify();
            }
            DaemonMessage::UsageSnapshot { thread_id, totals } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                self.model = Some(totals.model.clone());
                self.usage = Some(totals);
                cx.notify();
            }
            DaemonMessage::Error { message, .. } => {
                // Errors are not correlated to a thread or turn. Show the
                // message, but don't let a stale error terminate this turn.
                let (lifecycle, banner) = unscoped_error_transition(self.lifecycle, message);
                self.lifecycle = lifecycle;
                self.connection_error = Some(banner);
                cx.notify();
            }
            _ => {}
        }
    }

    fn thread_matches(&self, thread_id: &ThreadId) -> bool {
        self.selected_thread.as_ref() == Some(thread_id)
    }

    fn uses_daemon(&self) -> bool {
        self.connected && self.selected_thread.is_some()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !can_submit(self.lifecycle) {
            return;
        }
        if self.connected && self.selected_thread.is_none() {
            return;
        }
        if self.run_bang_command(window, cx) {
            return;
        }
        let Some((prompt, mentions)) = self.composer.take_content(window, cx) else {
            return;
        };

        if let (true, Some(thread_id)) = (self.connected, self.selected_thread.clone()) {
            let terminals: Vec<String> = self
                .terminals
                .read(cx)
                .attachments()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            if self.client.send(ClientMessage::SendPrompt {
                thread_id: thread_id.clone(),
                text: prompt.clone(),
                mentions,
                terminals,
            }) == SendStatus::Rejected
            {
                self.restore_prompt(&thread_id, &prompt, window, cx);
                return;
            }
            self.terminals
                .update(cx, |terminals, cx| terminals.clear_attachments(cx));
            self.pending_prompt = Some(prompt.clone());
            self._event_task = None;
        } else {
            self.pending_prompt = None;
            let rx = self.agent.send(prompt.clone());
            self._event_task = Some(cx.spawn(async move |this, cx| {
                while let Ok(event) = rx.recv().await {
                    let alive = this.update(cx, |this, cx| {
                        this.apply_event(event);
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }));
        }
        self.entries.push(ChatEntry::User {
            text: prompt,
            message_id: None,
        });
        self.last_failed = false;
        self.scroll.scroll_to_bottom();
        self.connection_error = None;
        self.lifecycle = TurnLifecycle::Running;
        self.final_assistant_persisted = false;
        self.status = AgentStatus::Running;
        self.composer.set_disabled(true, cx);
        cx.notify();
    }

    fn append_stored_message(&mut self, message: Message) {
        let at = message.created_at.timestamp_millis();
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    self.tool_views
                        .timings
                        .entry(id.clone())
                        .or_insert(ToolTiming {
                            started_ms: at,
                            finished_ms: None,
                        });
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    if let Some(timing) = self.tool_views.timings.get_mut(tool_use_id) {
                        timing.finished_ms.get_or_insert(at);
                    }
                }
                _ => {}
            }
        }
        if message.role == Role::User {
            self.prompt_times
                .insert(message.id.clone(), message.created_at);
        }
        match message.role {
            Role::User => {
                append_stored_user(&mut self.entries, &mut self.pending_prompt, &message);
            }
            Role::Assistant => {
                append_stored_assistant(&mut self.entries, self.lifecycle, &message);
                let text_len = message.text().len();
                let persisted = self.entries.iter().rev().any(|entry| {
                    matches!(
                        entry,
                        ChatEntry::Assistant { message_id: Some(id), markdown, .. }
                            if id == &message.id && markdown.len() == text_len
                    )
                });
                if persisted && text_len > 0 {
                    self.stored_len.insert(message.id.clone(), text_len);
                }
            }
            Role::Tool | Role::System => {
                if !self.is_running() {
                    self.expand_message_blocks(&message);
                }
            }
        }
    }

    /// Another participant's prompt, their Claude's answer, or a notice
    /// (comment, verdict, change summary). Kept apart from this view's own
    /// turn lifecycle: a remote message never ends or adopts a local turn.
    fn append_remote_message(&mut self, message: Message, author: String) {
        let text = message.text();
        if text.trim().is_empty() {
            return;
        }
        self.remote_authors
            .insert(message.id.clone(), (author, message.role));
        match message.role {
            Role::User => {
                self.prompt_times
                    .insert(message.id.clone(), message.created_at);
                self.entries.push(ChatEntry::User {
                    text,
                    message_id: Some(message.id),
                });
            }
            _ => {
                if message.role == Role::Assistant {
                    self.stored_len.insert(message.id.clone(), text.len());
                }
                self.entries.push(ChatEntry::Assistant {
                    markdown: text,
                    streaming: false,
                    message_id: Some(message.id),
                });
            }
        }
    }

    fn remote_author(&self, id: &MessageId) -> Option<&str> {
        self.remote_authors.get(id).map(|(name, _)| name.as_str())
    }

    fn expand_message_blocks(&mut self, message: &Message) {
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => {
                    if text.is_empty() {
                        continue;
                    }
                    match self.entries.last_mut() {
                        Some(ChatEntry::Assistant {
                            markdown,
                            streaming,
                            ..
                        }) => {
                            markdown.push_str(text);
                            *streaming = false;
                        }
                        _ => self.entries.push(ChatEntry::Assistant {
                            markdown: text.clone(),
                            streaming: false,
                            message_id: None,
                        }),
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                        *streaming = false;
                    }
                    self.entries.push(ChatEntry::ToolCall {
                        tool_use_id: id.clone(),
                        name: name.clone(),
                        input_json: input.clone(),
                        finished: false,
                        is_error: false,
                        expanded: false,
                        output: None,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                } => {
                    self.finish_tool(tool_use_id, *is_error, Some(content.clone()));
                }
                ContentBlock::Thinking { text } => push_thinking(&mut self.entries, text),
            }
        }
    }

    fn finish_tool(
        &mut self,
        tool_use_id: &str,
        is_error: bool,
        result: Option<serde_json::Value>,
    ) {
        if let Some(call) = self.entries.iter_mut().rev().find_map(|entry| match entry {
            ChatEntry::ToolCall {
                tool_use_id: id, ..
            } if id == tool_use_id => Some(entry),
            _ => None,
        }) {
            if let ChatEntry::ToolCall {
                finished,
                is_error: err,
                output,
                ..
            } = call
            {
                *finished = true;
                *err = is_error;
                if result.is_some() {
                    *output = result;
                }
            }
        }
    }

    fn apply_event(&mut self, event: AgentEvent) {
        // Session metadata and approvals are keyed, not turn-ordered: the
        // daemon replays them on subscribe while this view is idle.
        match event {
            AgentEvent::SessionInfo {
                billing,
                api_key_source,
                agent_version,
                permission_mode,
            } => {
                self.session_info = Some(SessionInfo {
                    billing,
                    api_key_source,
                    agent_version,
                    permission_mode,
                });
                return;
            }
            AgentEvent::ApprovalRequested {
                request_id,
                tool_name,
                input_json,
            } => {
                self.adopt_remote_turn();
                push_approval(&mut self.entries, request_id, tool_name, input_json);
                return;
            }
            AgentEvent::ApprovalResolved {
                request_id,
                decision,
                reason,
            } => {
                set_approval_state(
                    &mut self.entries,
                    &request_id,
                    ApprovalState::Resolved { decision, reason },
                );
                return;
            }
            _ => {}
        }
        if starts_remote_turn(self.lifecycle, &event) {
            self.adopt_remote_turn();
        }
        if !accepts_agent_events(self.lifecycle) {
            return;
        }
        match event {
            AgentEvent::SessionStarted { model, .. } => {
                if model.is_some() {
                    self.model = model;
                }
            }
            AgentEvent::TextDelta { text } => match self.entries.last_mut() {
                Some(ChatEntry::Assistant {
                    markdown,
                    streaming: true,
                    ..
                }) => markdown.push_str(&text),
                _ => self.entries.push(ChatEntry::Assistant {
                    markdown: text,
                    streaming: true,
                    message_id: None,
                }),
            },
            AgentEvent::ToolCallStarted {
                tool_use_id,
                name,
                input_json,
            } => {
                if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                    *streaming = false;
                }
                self.tool_views.timings.insert(
                    tool_use_id.clone(),
                    ToolTiming {
                        started_ms: tools::now_ms(),
                        finished_ms: None,
                    },
                );
                self.entries.push(ChatEntry::ToolCall {
                    tool_use_id,
                    name,
                    input_json,
                    finished: false,
                    is_error: false,
                    expanded: false,
                    output: None,
                });
            }
            AgentEvent::ToolCallFinished {
                tool_use_id,
                output_json,
                is_error,
            } => {
                if let Some(timing) = self.tool_views.timings.get_mut(&tool_use_id) {
                    timing.finished_ms.get_or_insert(tools::now_ms());
                }
                self.finish_tool(&tool_use_id, is_error, Some(output_json));
            }
            AgentEvent::ReasoningDelta { text } => push_thinking(&mut self.entries, &text),
            AgentEvent::UsageReported { report } => {
                self.model = Some(report.model.clone());
                self.usage = Some(report);
            }
            AgentEvent::TurnCompleted {
                num_turns,
                cost_usd_micros,
            } => {
                if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                    *streaming = false;
                }
                self.turns = Some(num_turns);
                self.cost_usd_micros = cost_usd_micros;
                self.finish_turn();
            }
            AgentEvent::Failed { message } => {
                self.entries.push(ChatEntry::Assistant {
                    markdown: format!("**Failed:** {message}"),
                    streaming: false,
                    message_id: None,
                });
                self.final_assistant_persisted = true;
                self.last_failed = true;
                self.finish_turn();
            }
            AgentEvent::ApprovalRequested { .. }
            | AgentEvent::SessionInfo { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. }
            | AgentEvent::CommandsReported { .. }
            | AgentEvent::TranscriptEntry { .. } => {}
        }
    }

    /// A turn this view did not start (a subthread's initial prompt, a
    /// review or land run, another client) is streaming: follow it.
    fn adopt_remote_turn(&mut self) {
        if self.lifecycle == TurnLifecycle::Idle && self.uses_daemon() {
            self.lifecycle = TurnLifecycle::Running;
            self.final_assistant_persisted = false;
            self.status = AgentStatus::Running;
            self.last_failed = false;
        }
    }

    fn finish_turn(&mut self) {
        expire_open_approvals(&mut self.entries);
        self.completed_turns += 1;
        self.lifecycle = terminal_transition(self.uses_daemon(), self.final_assistant_persisted);
        let waiting_for_assistant = self.lifecycle == TurnLifecycle::AwaitingAssistant;
        self.status = if waiting_for_assistant {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        };
    }

    /// Expand the n-th tool call (dev hook for screenshots).
    pub fn expand_tool_call(&mut self, nth: usize, cx: &mut Context<Self>) {
        let index = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches!(entry, ChatEntry::ToolCall { .. }))
            .nth(nth)
            .map(|(index, _)| index);
        if let Some(index) = index {
            self.toggle_tool(index, cx);
        }
    }

    pub(crate) fn toggle_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(ChatEntry::ToolCall { expanded, .. }) = self.entries.get_mut(index) {
            *expanded = !*expanded;
            cx.notify();
        }
    }

    pub fn toggle_group(&mut self, start: usize, cx: &mut Context<Self>) {
        if !self.tool_views.open_groups.remove(&start) {
            self.tool_views.open_groups.insert(start);
        }
        cx.notify();
    }

    /// Flip an edit row; `open` is what it showed (possibly by default).
    pub fn toggle_edit(&mut self, tool_use_id: &str, open: bool, cx: &mut Context<Self>) {
        self.tool_views
            .edit_open
            .insert(tool_use_id.to_string(), !open);
        cx.notify();
    }

    pub fn toggle_output(&mut self, tool_use_id: &str, cx: &mut Context<Self>) {
        if !self.tool_views.full_output.remove(tool_use_id) {
            self.tool_views.full_output.insert(tool_use_id.to_string());
        }
        cx.notify();
    }

    pub fn open_thread(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        cx.emit(ChatEvent::OpenThread(thread));
    }

    /// Threads the sidebar lists (id, title), for subagent links.
    pub fn set_known_threads(&mut self, threads: Vec<(ThreadId, String)>, cx: &mut Context<Self>) {
        if self.known_threads != threads {
            self.known_threads = threads;
            cx.notify();
        }
    }

    /// Each thread's model override (`ThreadSnapshot.model`).
    pub fn set_thread_models(
        &mut self,
        models: Vec<(ThreadId, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.model_picker
            .update(cx, |picker, cx| picker.set_overrides(models, cx));
    }

    /// ⌃` and the composer's terminal button.
    pub fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.uses_daemon() || self.pane_mode {
            return;
        }
        self.terminals
            .update(cx, |terminals, cx| terminals.toggle(window, cx));
        if !self.terminals.read(cx).is_open() {
            self.focus_composer(window, cx);
        }
    }

    /// `!command` in the composer: run it in a new terminal instead of
    /// sending a prompt. `true` when it did.
    fn run_bang_command(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.uses_daemon() || self.pane_mode {
            return false;
        }
        let Some(command) = self.popup.read(cx).bang_command() else {
            return false;
        };
        self.composer.set_prompt("", window, cx);
        let command = (!command.is_empty()).then_some(command);
        self.terminals
            .update(cx, |terminals, cx| terminals.open_terminal(command, cx));
        true
    }

    /// Keys the composer popups take before the textarea sees them.
    fn composer_key_handlers(&self, element: Div, cx: &mut Context<Self>) -> Div {
        element
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.popup.read(cx).is_open() {
                    this.popup
                        .update(cx, |popup, cx| popup.move_selection(-1, cx));
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.popup.read(cx).is_open() {
                    this.popup
                        .update(cx, |popup, cx| popup.move_selection(1, cx));
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if this
                    .popup
                    .update(cx, |popup, cx| popup.is_open() && popup.accept(window, cx))
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                if action.shift {
                    return;
                }
                if this
                    .popup
                    .update(cx, |popup, cx| popup.is_open() && popup.accept(window, cx))
                    || this.run_bang_command(window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.popup.update(cx, |popup, cx| popup.dismiss(cx)) {
                    cx.stop_propagation();
                }
            }))
    }

    /// The composer's terminal button (footer) and attached-terminal chips
    /// (above the text).
    fn composer_extras(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<AnyElement>) {
        let terminals = self.terminals.read(cx);
        let running = terminals.running_count();
        let open = terminals.is_open();
        let attachments = terminals.attachments();
        let mut tools = Vec::new();
        if self.uses_daemon() && !self.pane_mode {
            tools.push(
                h_flex()
                    .id("composer-terminal")
                    .h(px(22.))
                    .px(theme::sp(1.5))
                    .gap(theme::sp(1.))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_xs())
                    .text_color(if open { p.accent_text } else { p.text_muted })
                    .when(open, |this| this.bg(p.accent_soft))
                    .hover(|style| style.bg(p.hover))
                    .tooltip(|window, cx| {
                        Tooltip::new("Terminal \u{2014} or start a message with ! (\u{2303}`)")
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx)))
                    .child(Icon::new(IconName::SquareTerminal).with_size(px(13.)))
                    .when(running > 0 && !open, |this| {
                        this.child(format!("{running} running"))
                    })
                    .into_any_element(),
            );
        }
        let header = attachments
            .into_iter()
            .enumerate()
            .map(|(ix, (id, label))| {
                let terminals = self.terminals.clone();
                terminal_view::attachment_chip(
                    ("attached-terminal", ix),
                    &label,
                    p,
                    move |_, _, cx| {
                        terminals.update(cx, |terminals, cx| terminals.detach(&id, cx));
                    },
                )
            })
            .collect();
        (tools, header)
    }

    pub fn toggle_snapshot(&mut self, key: (MessageId, usize), cx: &mut Context<Self>) {
        if !self.open_snapshots.remove(&key) {
            self.open_snapshots.insert(key);
        }
        cx.notify();
    }

    /// Titles and subagent records of the listed threads, for annotation
    /// rows and links from `spawn_subagent` calls to their child.
    pub fn set_thread_index(
        &mut self,
        index: crate::subagents::ThreadIndex,
        cx: &mut Context<Self>,
    ) {
        if self.thread_index != index {
            self.thread_index = index;
            cx.notify();
        }
    }

    /// A stored turn's (or the running turn's) replayed tool calls and
    /// thinking. They arrive right before the message that ends the turn;
    /// the running turn's come after the history, and its later steps
    /// stream as agent events.
    fn apply_turn_activity(
        &mut self,
        message_id: Option<MessageId>,
        items: &[trama_protocol::ActivityItem],
    ) {
        if message_id
            .as_ref()
            .is_some_and(|id| self.seen_message_ids.contains(id))
        {
            return;
        }
        let in_flight = message_id.is_none();
        // Already following this turn (a reconnect mid-turn): only add the
        // tool calls it missed; its thinking is on screen already.
        let following = in_flight && self.is_running();
        if in_flight {
            // Subscribed mid-turn: follow it like a turn started elsewhere.
            self.adopt_remote_turn();
        }
        let shown = crate::activity::shown_tool_ids(&self.entries);
        let mut replayed = crate::activity::replay_entries(items, in_flight, &shown);
        if following {
            replayed
                .entries
                .retain(|entry| matches!(entry, ChatEntry::ToolCall { .. }));
        }
        for (id, timing) in replayed.timings {
            self.tool_views.timings.entry(id).or_insert(timing);
        }
        if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
            *streaming = false;
        }
        self.entries.extend(replayed.entries);
    }

    /// Ask the workspace to fork at `message_id` (the hover action).
    pub fn request_fork(&mut self, message_id: MessageId, cx: &mut Context<Self>) {
        let Some(thread_id) = self.selected_thread.clone() else {
            return;
        };
        if self.fork_index.state(&message_id) != crate::forks::ForkState::Ready {
            return;
        }
        let label = self
            .entries
            .iter()
            .find_map(|entry| match entry {
                ChatEntry::User {
                    text,
                    message_id: Some(id),
                } if id == &message_id => Some(comments::prompt_body(text).to_string()),
                ChatEntry::Assistant {
                    markdown,
                    message_id: Some(id),
                    ..
                } if id == &message_id => Some(markdown.clone()),
                _ => None,
            })
            .map(|text| crate::forks::preview(&text))
            .unwrap_or_default();
        cx.emit(ChatEvent::Fork(crate::forks::ForkTarget {
            thread_id,
            message_id,
            label,
        }));
    }

    /// "Fork from here" (⌘K): the turn under the rail's pointer, else the
    /// turn at the top of the view, when it can be forked.
    pub fn fork_here_target(&self) -> Option<MessageId> {
        if !self.uses_daemon() {
            return None;
        }
        let turn_list = turns::segment_turns(&self.entries);
        let turn = self.rail.hovered.or_else(|| {
            let geometry = self.rail_geometry()?;
            geometry
                .spans
                .iter()
                .rposition(|span| span.top <= geometry.view.top + 1.)
                .or(Some(0))
        })?;
        let range = turn_list.get(turn)?.entries.clone();
        let message = crate::forks::turn_fork_message(&self.entries[range])?;
        (self.fork_index.state(&message) == crate::forks::ForkState::Ready).then_some(message)
    }
}

/// Append streamed or stored thinking, merging with thinking right before.
fn push_thinking(entries: &mut Vec<ChatEntry>, text: &str) {
    if text.is_empty() {
        return;
    }
    match entries.last_mut() {
        Some(ChatEntry::Thinking { text: existing }) => existing.push_str(text),
        last => {
            if let Some(ChatEntry::Assistant { streaming, .. }) = last {
                *streaming = false;
            }
            entries.push(ChatEntry::Thinking {
                text: text.to_string(),
            });
        }
    }
}

// -- comments -------------------------------------------------------------

impl ChatPanel {
    /// Comment on the selected transcript text or the picked diff lines
    /// (⌘⌥M and the floating button).
    pub fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(hit) = self.selection.clone() {
            self.open_selection_draft(hit, None, window, cx);
        } else if let Some(pick) = self.patch_pick.clone() {
            self.open_patch_draft(pick, None, window, cx);
        } else if self.uses_daemon() {
            self.connection_error = Some(
                "Select text anywhere in the transcript, then type your comment (or press \u{2318}\u{2325}M)."
                    .into(),
            );
            cx.notify();
        }
    }

    /// Whether a selection or a diff pick waits for a comment.
    pub fn has_comment_target(&self) -> bool {
        self.selection.is_some() || self.patch_pick.is_some()
    }

    /// Type to comment: a printable key while transcript text is selected
    /// (or diff lines are picked) opens the composer under the passage with
    /// that text already in it.
    fn type_to_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selection.is_none() && self.patch_pick.is_none() {
            return;
        }
        if self
            .composer
            .input
            .focus_handle(cx)
            .contains_focused(window, cx)
            || self
                .draft
                .as_ref()
                .is_some_and(|open| open.draft.is_focused(window, cx))
        {
            return;
        }
        if event.keystroke.key == "escape" && self.patch_pick.is_some() && self.selection.is_none()
        {
            self.patch_pick = None;
            cx.notify();
            return;
        }
        let Some(text) = comments::typed_text(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        window.prevent_default();
        match self.selection.clone() {
            Some(hit) => self.open_selection_draft(hit, Some(text), window, cx),
            None => {
                if let Some(pick) = self.patch_pick.clone() {
                    self.open_patch_draft(pick, Some(text), window, cx);
                }
            }
        }
    }

    /// The hover action on a message: the selection inside it, else the
    /// whole message.
    fn comment_on_message(
        &mut self,
        message_id: MessageId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hit = match &self.selection {
            Some(hit @ SelectionHit::Message { message_id: id, .. }) if id == &message_id => {
                Some(hit.clone())
            }
            _ => self.whole_message_hit(&message_id),
        };
        if let Some(hit) = hit {
            self.open_selection_draft(hit, None, window, cx);
        }
    }

    fn whole_message_hit(&self, message_id: &MessageId) -> Option<SelectionHit> {
        let view = self.message_views.get(message_id)?;
        let base = self.entries.iter().find_map(|entry| match entry {
            ChatEntry::User {
                text,
                message_id: Some(id),
            } if id == message_id => Some(user_body_start(text)),
            ChatEntry::Assistant {
                message_id: Some(id),
                ..
            } if id == message_id => Some(0),
            _ => None,
        })?;
        let range = comments::selection_to_message_range(&view.source, base, 0..view.source.len())?;
        Some(SelectionHit::Message {
            message_id: message_id.clone(),
            range,
        })
    }

    /// Forget the selection the composer is about to take over.
    fn drop_selection(&mut self, cx: &mut Context<Self>) {
        for view in self.message_views.values() {
            for segment in &view.segments {
                segment
                    .state
                    .update(cx, |state, cx| state.clear_selection(cx));
            }
        }
        if let Some(SelectionHit::Block(selection)) = &self.selection {
            self.block_texts.clear_selection(&selection.block.key, cx);
        }
        self.selection = None;
        self.selection_point = None;
        self.connection_error = None;
    }

    fn open_selection_draft(
        &mut self,
        hit: SelectionHit,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.uses_daemon() {
            return;
        }
        let (target, anchor, quote) = match &hit {
            SelectionHit::Message { message_id, range } => (
                CommentTarget::MessageRange {
                    message_id: message_id.clone(),
                    start_byte: range.start,
                    end_byte: range.end,
                },
                DraftAnchor::Passage {
                    message_id: message_id.clone(),
                    range: range.clone(),
                },
                None,
            ),
            SelectionHit::Block(selection) => {
                let Some(target) = self.block_target(selection.block.turn) else {
                    self.connection_error =
                        Some("This turn is not saved yet; comment once it has an answer.".into());
                    cx.notify();
                    return;
                };
                (
                    target,
                    DraftAnchor::Block {
                        key: selection.block.key.clone(),
                        kind: selection.block.kind,
                        excerpt: selection.text.clone(),
                        turn: selection.block.turn,
                    },
                    Some((selection.block.kind, selection.text.clone())),
                )
            }
        };
        self.drop_selection(cx);
        self.patch_pick = None;
        let draft = CommentDraft::new(
            target,
            None,
            initial,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        let draft = match quote {
            Some((kind, excerpt)) => draft.with_quote(kind, excerpt),
            None => draft,
        };
        self.draft = Some(ChatDraft { anchor, draft });
        cx.notify();
    }

    /// Where a comment on a block of `turn` is stored: the first line of
    /// the turn's stored answer, else of its stored prompt. The block's
    /// passage goes in the comment's body ([`comments::proxy_body`]).
    fn block_target(&self, turn: usize) -> Option<CommentTarget> {
        let turn_list = turns::segment_turns(&self.entries);
        let range = turn_list.get(turn)?.entries.clone();
        let entries = &self.entries[range];
        let answer = entries.iter().rev().find_map(|entry| match entry {
            ChatEntry::Assistant {
                markdown,
                streaming: false,
                message_id: Some(id),
            } if self.stored_len.get(id) == Some(&markdown.len()) => {
                comments::proxy_anchor(markdown).map(|range| (id.clone(), range))
            }
            _ => None,
        });
        let prompt = || {
            entries.iter().find_map(|entry| match entry {
                ChatEntry::User {
                    text,
                    message_id: Some(id),
                } => {
                    let base = user_body_start(text);
                    comments::proxy_anchor(&text[base..])
                        .map(|range| (id.clone(), range.start + base..range.end + base))
                }
                _ => None,
            })
        };
        let (message_id, range) = answer.or_else(prompt)?;
        Some(CommentTarget::MessageRange {
            message_id,
            start_byte: range.start,
            end_byte: range.end,
        })
    }

    /// A comment on picked diff lines: on the code they wrote when the file
    /// is in a known checkout, else a block comment quoting the lines.
    fn open_patch_draft(
        &mut self,
        pick: PatchPick,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.uses_daemon() {
            return;
        }
        let found = self
            .entries
            .iter()
            .enumerate()
            .find_map(|(ix, entry)| match entry {
                ChatEntry::ToolCall { tool_use_id, .. } if tool_use_id == &pick.tool_use_id => {
                    Some((ix, self.tool_views.file_edit(entry)?))
                }
                _ => None,
            });
        let Some((ix, edit)) = found else {
            return;
        };
        let Some(hunk) = edit.hunks.get(pick.hunk) else {
            return;
        };
        let quoted: Vec<String> = hunk
            .lines
            .get(pick.lines())
            .unwrap_or_default()
            .iter()
            .map(tools::quoted_line)
            .collect();
        let (target, quote) =
            match comments::patch_target(&self.checkouts, &edit.path, hunk, pick.lines()) {
                Some(target) => (target, None),
                None => {
                    let turn = turns::segment_turns(&self.entries)
                        .iter()
                        .position(|turn| turn.entries.contains(&ix))
                        .unwrap_or(0);
                    let Some(target) = self.block_target(turn) else {
                        self.connection_error = Some(
                            "This turn is not saved yet; comment once it has an answer.".into(),
                        );
                        cx.notify();
                        return;
                    };
                    (target, Some((comments::KIND_PATCH, quoted.join("\n"))))
                }
            };
        self.drop_selection(cx);
        let draft = CommentDraft::new(
            target,
            None,
            initial,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        let draft = match quote {
            Some((kind, excerpt)) => draft.with_quote(kind, excerpt),
            None => draft,
        };
        self.draft = Some(ChatDraft {
            anchor: DraftAnchor::Patch {
                tool_use_id: pick.tool_use_id.clone(),
            },
            draft,
        });
        cx.notify();
    }

    /// Pick a diff line (shift extends within the hunk) and take the keys,
    /// so typing comments on the pick.
    pub(crate) fn press_patch_line(
        &mut self,
        at: PatchLine,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !self.uses_daemon() {
            return;
        }
        self.doc_focus.focus(window, cx);
        self.patch_pick = match self.patch_pick.take() {
            Some(mut pick)
                if extend && pick.tool_use_id == at.tool_use_id && pick.hunk == at.hunk =>
            {
                pick.cursor = at.line;
                Some(pick)
            }
            _ => Some(PatchPick {
                tool_use_id: at.tool_use_id,
                hunk: at.hunk,
                anchor: at.line,
                cursor: at.line,
            }),
        };
        self.patch_dragging = true;
        cx.notify();
    }

    pub(crate) fn drag_patch_line(&mut self, at: PatchLine, cx: &mut Context<Self>) {
        if let Some(pick) = self.patch_pick.as_mut()
            && self.patch_dragging
            && pick.tool_use_id == at.tool_use_id
            && pick.hunk == at.hunk
            && pick.cursor != at.line
        {
            pick.cursor = at.line;
            cx.notify();
        }
    }

    fn open_reply(&mut self, root: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .comments
            .read(cx)
            .groups()
            .into_iter()
            .find(|group| group.id() == &root)
            .map(|group| group.root.target);
        let Some(target) = target else {
            return;
        };
        self.thread_open.insert(root.clone(), true);
        let draft = CommentDraft::new(
            target,
            Some(root.clone()),
            None,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        self.draft = Some(ChatDraft {
            anchor: DraftAnchor::Reply(root),
            draft,
        });
        cx.notify();
    }

    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.draft else {
            return;
        };
        if open.draft.submit(&self.comments, cx) {
            if matches!(open.anchor, DraftAnchor::Patch { .. }) {
                self.patch_pick = None;
            }
            self.draft = None;
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.draft.as_ref().map(|open| &open.anchor),
            Some(DraftAnchor::Patch { .. })
        ) {
            self.patch_pick = None;
        }
        self.draft = None;
        self.focus_composer(window, cx);
        cx.notify();
    }

    fn toggle_thread(&mut self, id: CommentId, cx: &mut Context<Self>) {
        let default = self
            .comments
            .read(cx)
            .groups()
            .iter()
            .find(|group| group.id() == &id)
            .is_some_and(CommentGroup::open_by_default);
        let open = self.thread_open.get(&id).copied().unwrap_or(default);
        self.thread_open.insert(id, !open);
        cx.notify();
    }

    /// Scroll the transcript to a message and tint it briefly.
    pub fn reveal_message(&mut self, message_id: MessageId, cx: &mut Context<Self>) {
        self.reveal = Some(message_id.clone());
        self.flash = Some(message_id);
        self._flash_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.flash = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Deliver the pending comments as a turn of their own.
    fn send_comments_only(&mut self, cx: &mut Context<Self>) {
        if !can_submit(self.lifecycle) || !self.uses_daemon() {
            return;
        }
        if self.comments.update(cx, |store, cx| store.send_pending(cx)) != SendStatus::Enqueued {
            return;
        }
        self.pending_prompt = None;
        self._event_task = None;
        self.last_failed = false;
        self.connection_error = None;
        self.lifecycle = TurnLifecycle::Running;
        self.final_assistant_persisted = false;
        self.status = AgentStatus::Running;
        self.composer.set_disabled(true, cx);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn card_handlers(&self, cx: &mut Context<Self>) -> CardHandlers {
        let toggle = cx.entity().downgrade();
        let reply = toggle.clone();
        let answer = toggle.clone();
        CardHandlers {
            store: self.comments.clone(),
            on_toggle: Rc::new(move |id, _, cx| {
                let _ = toggle.update(cx, |this, cx| this.toggle_thread(id, cx));
            }),
            on_reply: Rc::new(move |id, window, cx| {
                let _ = reply.update(cx, |this, cx| this.open_reply(id, window, cx));
            }),
            on_answer: Rc::new(move |id, _, cx| {
                let _ = answer.update(cx, |this, cx| this.reveal_message(id, cx));
            }),
        }
    }

    /// The selectable body of a persisted message, split after each
    /// commented block with the block's threads (and the open composer)
    /// right under it. Also highlights anchored passages and reports a
    /// selection inside the message. Returns the body and the message's
    /// threads.
    #[allow(clippy::too_many_arguments)]
    fn message_body(
        &mut self,
        message_id: &MessageId,
        source: &str,
        base: usize,
        strong: bool,
        ctx: &CardCtx,
        hits: &mut Vec<SelectionHit>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let groups = &ctx.groups;
        let (threads, anchors) = comments::message_anchors(groups, message_id, base, source);

        // Threads go under the block their passage ends in; threads whose
        // range no longer fits go after the message.
        let mut at_cut: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut trailing = Vec::new();
        for &group in &threads {
            match anchors.iter().find(|anchor| anchor.group == group) {
                Some(anchor) => {
                    let cut = comments::block_end(source, anchor.range.end);
                    match at_cut.iter_mut().find(|(at, _)| *at == cut) {
                        Some((_, list)) => list.push(group),
                        None => at_cut.push((cut, vec![group])),
                    }
                }
                None => trailing.push(group),
            }
        }
        let draft_range = match self.draft.as_ref().map(|open| &open.anchor) {
            Some(DraftAnchor::Passage {
                message_id: id,
                range,
            }) if id == message_id && range.start >= base && range.end - base <= source.len() => {
                Some(range.start - base..range.end - base)
            }
            _ => None,
        };
        let draft_cut = draft_range
            .as_ref()
            .map(|range| comments::block_end(source, range.end));
        let mut cuts: Vec<usize> = at_cut
            .iter()
            .map(|(cut, _)| *cut)
            .chain(draft_cut)
            .filter(|cut| *cut > 0 && *cut < source.len())
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut bounds = vec![0];
        bounds.extend(cuts);
        bounds.push(source.len());
        let ranges: Vec<Range<usize>> = bounds.windows(2).map(|w| w[0]..w[1]).collect();

        let view = self
            .message_views
            .entry(message_id.clone())
            .or_insert_with(|| MessageView {
                source: source.to_string(),
                segments: Vec::new(),
            });
        if view.source != source {
            view.source = source.to_string();
            view.segments.clear();
        }
        let mut old = std::mem::take(&mut view.segments);
        for range in &ranges {
            let segment = match old.iter().position(|segment| &segment.range == range) {
                Some(index) => old.remove(index),
                None => Segment {
                    range: range.clone(),
                    state: cx.new(|cx| {
                        TextViewState::markdown(&source[range.clone()], cx).selectable(true)
                    }),
                    applied: None,
                },
            };
            view.segments.push(segment);
        }

        // One highlight per passage (later overlapping ones are skipped);
        // the passage being commented on stays highlighted too.
        let mut marks: Vec<(Range<usize>, Hsla)> = anchors
            .iter()
            .filter_map(|anchor| {
                comments::passage_color(&groups[anchor.group].phase(), p)
                    .map(|color| (anchor.range.clone(), color))
            })
            .chain(draft_range.map(|range| (range, p.accent.opacity(0.3))))
            .collect();
        marks.sort_by_key(|(range, _)| (range.start, range.end));
        let mut desired: Vec<(Range<usize>, Hsla)> = Vec::new();
        for (range, color) in marks {
            if desired
                .last()
                .is_some_and(|(last, _)| range.start < last.end)
            {
                continue;
            }
            desired.push((range, color));
        }

        let mut selected: Option<Range<usize>> = None;
        for segment in &mut view.segments {
            let local: Vec<(Range<usize>, Hsla)> = desired
                .iter()
                .filter_map(|(range, color)| {
                    let start = range.start.max(segment.range.start);
                    let end = range.end.min(segment.range.end);
                    (start < end).then(|| {
                        (
                            start - segment.range.start..end - segment.range.start,
                            *color,
                        )
                    })
                })
                .collect();
            let rendered = segment.state.read(cx).rendered_text();
            let key = (rendered.clone(), local);
            if segment.applied.as_ref() != Some(&key) {
                let highlights: Vec<RangeHighlight> = key
                    .1
                    .iter()
                    .filter_map(|(range, color)| {
                        rendered
                            .range_for_source(range.clone())
                            .map(|range| RangeHighlight::new(range, *color))
                    })
                    .collect();
                segment.state.update(cx, |state, cx| {
                    let _ = state.set_range_highlights(highlights, cx);
                });
                segment.applied = Some(key);
            }
            // A drag across blocks selects in several parts: join them.
            if let Some(range) = segment.state.read(cx).selected_source_range()
                && range.start < range.end
            {
                let range = range.start + segment.range.start..range.end + segment.range.start;
                selected = Some(match selected {
                    Some(seen) => seen.start.min(range.start)..seen.end.max(range.end),
                    None => range,
                });
            }
        }
        if let Some(selected) = selected
            && let Some(range) = comments::selection_to_message_range(source, base, selected)
        {
            hits.push(SelectionHit::Message {
                message_id: message_id.clone(),
                range,
            });
        }

        let states: Vec<(Range<usize>, Entity<TextViewState>)> = view
            .segments
            .iter()
            .map(|segment| (segment.range.clone(), segment.state.clone()))
            .collect();
        let last = source.len();
        let mut body = v_flex().w_full().gap(theme::sp(2.));
        for (range, state) in states {
            body = body.child(
                div()
                    .w_full()
                    .font_family(theme::PROSE_FONT)
                    .text_size(theme::text_prose())
                    .line_height(relative(1.6))
                    .text_color(p.text)
                    .when(strong, |this| this.font_weight(FontWeight::MEDIUM))
                    .child(TextView::new(&state).selectable(true)),
            );
            let ends_here = |cut: usize| cut == range.end || (range.end == last && cut >= last);
            let mut under: Vec<usize> = at_cut
                .iter()
                .filter(|(cut, _)| ends_here(*cut))
                .flat_map(|(_, list)| list.iter().copied())
                .collect();
            if range.end == last {
                under.extend(trailing.iter().copied());
            }
            for group in under {
                body = body.child(self.inline_card(ctx, group, "inline", p, cx));
            }
            if draft_cut.is_some_and(ends_here)
                && let Some(open) = &self.draft
            {
                body = body.child(self.render_open_draft("chat-draft", open, p, cx));
            }
        }
        body.into_any_element()
    }

    /// A thread as a card in the document, with the reply composer when
    /// it is open on this thread (drawn under the first card only).
    fn inline_card(
        &self,
        ctx: &CardCtx,
        index: usize,
        place: &str,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = &ctx.groups[index];
        let answer = match group.phase() {
            Phase::Delivered(delivery) | Phase::Resolved(Some(delivery)) => delivery
                .answer_message_id
                .and_then(|id| ctx.answers.get(&id).map(|text| (id.clone(), text.clone()))),
            _ => None,
        };
        let reply = if ctx.reply_drawn.get() {
            None
        } else {
            self.draft
                .as_ref()
                .filter(|open| open.anchor == DraftAnchor::Reply(group.id().clone()))
                .map(|open| {
                    ctx.reply_drawn.set(true);
                    self.render_open_draft("chat-reply", open, p, cx)
                })
        };
        comments::render_inline(
            format!("chat-{place}-{}", group.id()),
            group,
            answer,
            reply,
            ctx.me.as_deref(),
            &ctx.handlers,
            p,
        )
    }

    fn render_open_draft(
        &self,
        id: &str,
        open: &ChatDraft,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let submit = cx.listener(|this, _, window, cx| this.submit_draft(window, cx));
        let weak = cx.entity().downgrade();
        let cancel: comments::CancelDraft = Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.cancel_draft(window, cx));
        });
        comments::render_draft(id.to_string(), &open.draft, submit, cancel, p, cx)
    }
    /// "Sent N comments" above a delivered prompt; expands to the block.
    fn sent_block(
        &self,
        message_id: &MessageId,
        text: &str,
        block: &PromptBlock,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open_blocks.contains(message_id);
        let id = message_id.clone();
        let label = if block.entries == 0 {
            "Sent comments".to_string()
        } else {
            format!(
                "Sent {}",
                comments::plural(block.entries, "comment", "comments")
            )
        };
        v_flex()
            .w_full()
            .gap(theme::sp(1.5))
            .child(
                h_flex().child(
                    h_flex()
                        .id(SharedString::from(format!("sent-block-{message_id}")))
                        .h(px(24.))
                        .px(theme::sp(2.))
                        .gap(theme::sp(1.5))
                        .items_center()
                        .rounded(theme::radius())
                        .border_1()
                        .border_color(p.border)
                        .bg(p.elevated)
                        .cursor_pointer()
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .hover(|style| style.bg(p.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.open_blocks.remove(&id) {
                                this.open_blocks.insert(id.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(IconName::MessageSquareText)
                                .with_size(px(12.))
                                .text_color(p.accent_text),
                        )
                        .child(div().text_color(p.text).child(label))
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .with_size(px(11.))
                            .text_color(p.text_faint),
                        ),
                ),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .w_full()
                        .px(theme::sp(2.5))
                        .py(theme::sp(2.))
                        .rounded(theme::radius())
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.border)
                        .font_family(theme::MONO_FONT)
                        .text_size(theme::text_xs())
                        .line_height(relative(1.5))
                        .text_color(p.text_muted)
                        .child(text[block.block.clone()].to_string()),
                )
            })
            .into_any_element()
    }

    /// Pending comments above the composer, with "Send comments only".
    fn pending_strip(
        &self,
        groups: &[CommentGroup],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pending: Vec<&CommentGroup> = groups
            .iter()
            .filter(|group| group.phase() == Phase::Pending)
            .collect();
        let notice = self.comments.read(cx).notice.clone();
        if pending.is_empty() && notice.is_none() {
            return None;
        }
        let total = comments::pending_total(groups);
        let can_send = can_submit(self.lifecycle) && self.uses_daemon();
        let store = self.comments.clone();
        let chip = |group: &CommentGroup| {
            let root = &group.root;
            let reveal = match &root.target {
                CommentTarget::MessageRange { message_id, .. } => Some(message_id.clone()),
                _ => None,
            };
            let icon = match &root.target {
                CommentTarget::MessageRange { .. } => IconName::MessageSquare,
                CommentTarget::Code { .. } => IconName::FileText,
                CommentTarget::DiffHunk { .. } => IconName::FileDiff,
            };
            let mut tip = comments::target_label(&root.target);
            if let Some(excerpt) = &root.excerpt {
                tip = format!("{tip}\n{}", comments::one_line(excerpt, 120));
            }
            let tooltip: SharedString = tip.into();
            let delete = comments::can_delete(root).then(|| {
                let store = store.clone();
                let comment_id = root.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "pending-chip-delete-{}",
                        root.id
                    )))
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(theme::radius_sm())
                    .cursor_pointer()
                    .hover(|style| style.bg(p.hover))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        store.update(cx, |store, cx| store.delete(comment_id.clone(), cx));
                    })
                    .child(
                        Icon::new(IconName::Close)
                            .with_size(px(10.))
                            .text_color(p.text_faint),
                    )
            });
            h_flex()
                .id(SharedString::from(format!("pending-chip-{}", root.id)))
                .h(px(24.))
                .max_w(px(280.))
                .pl(theme::sp(2.))
                .pr(theme::sp(1.))
                .gap(theme::sp(1.5))
                .items_center()
                .rounded(theme::radius())
                .border_1()
                .border_color(p.accent.opacity(0.35))
                .bg(p.accent_soft)
                .text_size(theme::text_xs())
                .text_color(p.text)
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .when_some(reveal, |this, message_id| {
                    this.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reveal_message(message_id.clone(), cx);
                        }))
                })
                .child(Icon::new(icon).with_size(px(11.)).text_color(p.accent_text))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(comments::one_line(&root.body, 48)),
                )
                .when(
                    group
                        .replies
                        .iter()
                        .any(|reply| reply.state == trama_protocol::CommentState::Pending),
                    |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .text_color(p.text_faint)
                                .child("reply"),
                        )
                    },
                )
                .children(delete)
        };
        Some(
            v_flex()
                .w_full()
                .pb(theme::sp(2.))
                .gap(theme::sp(1.5))
                .when(!pending.is_empty(), |this| {
                    this.child(
                        h_flex()
                            .w_full()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_xs())
                            .text_color(p.text_muted)
                            .child(
                                Icon::new(IconName::MessageSquare)
                                    .with_size(px(12.))
                                    .text_color(p.accent_text),
                            )
                            .child(div().flex_1().child(format!(
                                "{} will be sent with your next message",
                                comments::plural(total, "comment", "comments")
                            )))
                            .child(
                                Button::new("send-comments-only")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::ArrowUp)
                                    .label("Send comments only")
                                    .tooltip("Deliver the comments now, without a message")
                                    .disabled(!can_send)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.send_comments_only(cx)),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap(theme::sp(1.5))
                            .children(pending.iter().map(|group| chip(group))),
                    )
                })
                .when_some(notice, |this, notice| {
                    this.child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.warning)
                            .child(notice),
                    )
                })
                .into_any_element(),
        )
    }
}

/// Where the person's own text starts in a stored user message.
fn user_body_start(text: &str) -> usize {
    comments::split_prompt_block(text)
        .map(|block| block.body_start)
        .unwrap_or(0)
}

impl Render for ChatPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.restore_thread_draft {
            if let Some(thread_id) = &self.selected_thread {
                let draft = self.drafts.remove(thread_id).unwrap_or_default();
                self.composer.set_prompt(&draft, window, cx);
            }
            self.restore_thread_draft = false;
        }
        let p = theme::palette(cx);
        let running = self.is_running();
        let stoppable = matches!(
            self.lifecycle,
            TurnLifecycle::Running | TurnLifecycle::Stopping
        );
        self.composer.set_disabled(running, cx);
        let uses_daemon = self.uses_daemon();
        let show_composer = uses_daemon || !self.connected;
        let groups = if self.comments_enabled() {
            self.comments.read(cx).groups()
        } else {
            Vec::new()
        };
        let handlers = self.card_handlers(cx);
        let shared = match (&self.collab, &self.selected_thread) {
            (Some(collab), Some(thread)) if uses_daemon => collab.read(cx).thread_view(thread),
            _ => Default::default(),
        };
        let me = self
            .collab
            .as_ref()
            .and_then(|collab| collab.read(cx).username().map(str::to_string));
        let pad = if self.pane_mode {
            theme::sp(4.)
        } else {
            theme::sp(6.)
        };

        // Each block is one child of the scroll container, so a message can
        // be scrolled to by its row index.
        let row = |content: AnyElement| doc_row(None, content, pad);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut row_of: HashMap<MessageId, usize> = HashMap::new();
        let mut hits: Vec<SelectionHit> = Vec::new();

        if let Some(error) = self.connection_error.clone() {
            rows.push(row(
                notice(&p, IconName::TriangleAlert, p.warning, error).into_any_element()
            ));
        }
        if shared.revoked {
            rows.push(row(crate::presence::revoked_banner(&p)));
        }
        if !self.connected && !self.pane_mode {
            rows.push(row(notice(
                &p,
                IconName::Info,
                p.text_muted,
                "Offline demo: replies are scripted and nothing is sent to an agent.".into(),
            )
            .into_any_element()));
        }

        // Moved out while rendering so blocks can borrow `self` mutably.
        let entries = std::mem::take(&mut self.entries);
        if entries.is_empty() {
            rows.push(row(if uses_daemon || !self.connected {
                empty_state(
                    &p,
                    IconName::MessageSquare,
                    "Start the conversation",
                    "Describe the task. The agent works in this thread's checkout and streams its progress here.",
                    &[("enter", "Send"), ("shift-enter", "New line")],
                )
            } else {
                empty_state(
                    &p,
                    IconName::MessageSquare,
                    "No thread selected",
                    "Pick a thread in the sidebar or start a new one.",
                    &[
                        ("cmd-n", "New thread"),
                        ("cmd-k", "Jump to thread"),
                        ("cmd-/", "Shortcuts"),
                    ],
                )
            }));
        }

        let turn_list = turns::segment_turns(&entries);
        let hidden = self.hidden_entries(&entries);
        let items: Vec<Item> = tools::layout_items(&entries)
            .into_iter()
            .filter(|item| !matches!(item, Item::Single(ix) if hidden.contains(ix)))
            .collect();
        let answers: HashMap<MessageId, String> = entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::Assistant {
                    markdown,
                    message_id: Some(id),
                    ..
                } => turns::first_sentence(markdown, 140).map(|line| (id.clone(), line)),
                _ => None,
            })
            .collect();
        let placement = self.place_comments(&entries, &turn_list, &groups, &p);
        let card_ctx = CardCtx {
            groups,
            handlers,
            answers,
            me,
            reply_drawn: Cell::new(false),
        };
        let blocks = document::document(
            &items,
            &turn_list,
            |turn| placement.by_turn.contains_key(&turn),
            |item| self.entry_author(&entries[item.start()]),
        );

        self.block_texts.begin_frame();
        let mut turn_rows: Vec<usize> = Vec::with_capacity(turn_list.len());
        let mut last_author: Option<AuthorKey> = None;
        for block in &blocks {
            while turn_rows.len() <= block.turn && turn_rows.len() < turn_list.len() {
                turn_rows.push(rows.len());
            }
            let index = rows.len();
            let content = match &block.item {
                BlockItem::Comments(turn) => {
                    let list = placement.by_turn.get(turn).cloned().unwrap_or_default();
                    self.turn_comments(*turn, &list, &card_ctx, &p, cx)
                }
                BlockItem::Entries(item) => self.render_item(
                    item,
                    &entries,
                    block.turn,
                    &placement,
                    &card_ctx,
                    &mut hits,
                    &mut row_of,
                    index,
                    &p,
                    cx,
                ),
            };
            let avatar = block
                .avatar
                .then(|| self.avatar(&block.author, card_ctx.me.as_deref(), index, &p));
            rows.push(doc_row(avatar, content, pad));
            last_author = Some(block.author.clone());
        }
        self.entries = entries;
        let rows_changed = self.rail.turn_rows != turn_rows;
        self.rail.turn_rows = turn_rows;

        // A selection inside exactly one block can be commented on.
        hits.extend(
            self.block_texts
                .selections(cx)
                .into_iter()
                .map(SelectionHit::Block),
        );
        self.selection = if hits.len() == 1 { hits.pop() } else { None };
        if let Some(id) = self.reveal.take() {
            match row_of.get(&id) {
                Some(&index) => self.scroll.scroll_to_top_of_item(index),
                None => self.flash = None,
            }
        }
        if let Some(flash) = &self.flash
            && let Some(&index) = row_of.get(flash)
            && let Some(slot) = rows.get_mut(index)
        {
            let element = std::mem::replace(slot, div().into_any_element());
            *slot = div()
                .w_full()
                .flex_shrink_0()
                .bg(p.accent_soft)
                .child(element)
                .into_any_element();
        }

        for turn in &shared.remote_turns {
            rows.push(row(crate::presence::remote_turn(&p, turn)));
            last_author = None;
        }

        if running {
            let label = match self.lifecycle {
                TurnLifecycle::Stopping => "Stopping…",
                TurnLifecycle::AwaitingAssistant => "Finishing…",
                _ if self.has_open_approval() => "Waiting for approval…",
                _ if shared.queued && shared.runner_online => "Queued · starting on this machine…",
                _ if shared.queued => "Queued · runs when your runner is online…",
                _ => "Working…",
            };
            let avatar = (last_author != Some(AuthorKey::Agent))
                .then(|| self.avatar(&AuthorKey::Agent, None, rows.len(), &p));
            rows.push(doc_row(
                avatar,
                h_flex()
                    .min_h(px(22.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .text_size(theme::text_ui())
                    .text_color(p.text_muted)
                    .child(Spinner::new().small().color(p.accent))
                    .child(label)
                    .into_any_element(),
                pad,
            ));
        }

        let row_count = rows.len();
        let transcript = v_flex()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .track_focus(&self.doc_focus)
            .pt(theme::sp(8.))
            .pb(theme::sp(6.))
            .gap(theme::sp(2.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    // A diff line press stops before this: anything else
                    // drops the picked lines.
                    if this.patch_pick.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.selection_point = Some(event.position);
                    this.patch_dragging = false;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                if this.selection_point.take().is_some() {
                    cx.notify();
                }
            }))
            .children(rows);

        // The rail reads the last layout; measure again once this one lands.
        if rows_changed || self.scroll.children_count() != row_count {
            cx.on_next_frame(window, |_, _, cx| cx.notify());
        }
        let (rail, rail_card) = match self.rail_geometry() {
            Some(geometry) if turn_list.len() > 1 => {
                self.render_rail(&turn_list, &geometry, &p, cx)
            }
            _ => (None, None),
        };
        let weak = cx.entity().downgrade();
        let option_scroll = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture
                        || !event.modifiers.alt
                        || !bounds.contains(&event.position)
                    {
                        return;
                    }
                    let delta = event.delta.pixel_delta(window.line_height()).y;
                    let precise = event.delta.precise();
                    let handled = weak
                        .update(cx, |this, cx| this.scroll_by_marker(delta, precise, cx))
                        .unwrap_or(false);
                    if handled {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .size_full();

        let floating = self
            .has_comment_target()
            .then_some(self.selection_point)
            .flatten()
            .filter(|_| uses_daemon && self.draft.is_none())
            .map(|at| {
                deferred(
                    anchored()
                        .position(at)
                        .offset(point(px(-48.), px(-40.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            h_flex()
                                .id("selection-comment")
                                .h(px(28.))
                                .px(theme::sp(2.5))
                                .gap(theme::sp(1.5))
                                .items_center()
                                .rounded(theme::radius())
                                .border_1()
                                .border_color(p.border_strong)
                                .bg(p.elevated)
                                .shadow_md()
                                .cursor_pointer()
                                .text_size(theme::text_sm())
                                .text_color(p.text)
                                .hover(|style| style.bg(p.hover))
                                // Act on press: the release would clear the
                                // text selection first.
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.comment_on_selection(window, cx);
                                    }),
                                )
                                .child(
                                    Icon::new(IconName::MessageSquarePlus)
                                        .with_size(px(13.))
                                        .text_color(p.accent_text),
                                )
                                .child("Comment")
                                .child(
                                    div()
                                        .text_size(theme::text_xs())
                                        .text_color(p.text_faint)
                                        .child("or just type"),
                                ),
                        ),
                )
                .with_priority(1)
            });

        let session_model = self.model.clone();
        self.model_picker
            .update(cx, |picker, _| picker.session_model = session_model);
        let (composer_tools, composer_header) = self.composer_extras(&p, cx);
        let footer = ComposerFooter {
            model: self.model.clone(),
            status_label: self.status_pill(&p).0,
            status_color: self.status_pill(&p).1,
            usage: self.usage.clone(),
            cost_usd_micros: self.cost_usd_micros,
            turns: self.turns,
            billing: self.session_info.as_ref().map(billing_badge),
            model_picker: (uses_daemon && !self.pane_mode)
                .then(|| self.model_picker.clone().into_any_element()),
            tools: composer_tools,
            header: composer_header,
        };
        let terminals_open = uses_daemon && !self.pane_mode && self.terminals.read(cx).is_open();
        let strip = if show_composer && uses_daemon {
            self.pending_strip(&card_ctx.groups, &p, cx)
        } else {
            None
        };
        let live_drafts = crate::presence::drafts_block(&p, &shared.drafts);
        let shared_footer = (shared.linked && !shared.revoked)
            .then(|| crate::presence::shared_footer(&p, shared.runner_online));

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.type_to_comment(event, window, cx);
            }))
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(transcript)
                    .child(option_scroll)
                    .children(rail),
            )
            .children(rail_card)
            .children(floating)
            .when(show_composer, |this| {
                this.child(
                    h_flex().w_full().justify_center().child(
                        v_flex()
                            .w_full()
                            .max_w(theme::transcript_width())
                            .px(theme::sp(6.))
                            .pb(theme::sp(4.))
                            .children(live_drafts)
                            .children(strip)
                            .when(terminals_open, |this| this.child(self.terminals.clone()))
                            .child(self.composer_key_handlers(div().w_full(), cx).child(
                                self.composer.render(
                                    running,
                                    stoppable,
                                    footer,
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    cx.listener(|this, _, _, cx| this.stop(cx)),
                                    window,
                                    cx,
                                ),
                            ))
                            .child(self.popup.clone())
                            .children(shared_footer),
                    ),
                )
            })
    }
}

// -- document blocks ----------------------------------------------------------

/// Width of the avatar gutter left of the text column.
const GUTTER: f32 = 34.;
/// Avatar size in the gutter.
const AVATAR: f32 = 22.;

/// One block of the document: the author's avatar in the gutter (only
/// where their run starts) and the block in the text column.
fn doc_row(avatar: Option<AnyElement>, content: AnyElement, pad: Pixels) -> AnyElement {
    h_flex()
        .w_full()
        .flex_shrink_0()
        .justify_center()
        .child(
            h_flex()
                .w_full()
                .max_w(theme::transcript_width() + px(GUTTER))
                .px(pad)
                .items_start()
                .child(div().w(px(GUTTER)).flex_shrink_0().children(avatar))
                .child(div().flex_1().min_w_0().child(content)),
        )
        .into_any_element()
}

/// A round glyph avatar (the agent, a person without a known name).
fn glyph_avatar(icon: IconName, bg: Hsla, fg: Hsla) -> Div {
    div()
        .size(px(AVATAR))
        .flex_shrink_0()
        .rounded_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).with_size(px(13.)).text_color(fg))
}

impl ChatPanel {
    /// Comments can be read and written here.
    fn comments_enabled(&self) -> bool {
        self.uses_daemon()
    }

    /// Show a subagent's transcript in the side pane (rather than as the
    /// selected thread).
    pub fn set_pane_mode(&mut self, pane: bool) {
        self.pane_mode = pane;
    }

    /// The selected thread's checkouts, for code comments on diff lines.
    pub fn set_checkouts(
        &mut self,
        thread_id: &ThreadId,
        checkouts: &[trama_protocol::CheckoutSnapshot],
        cx: &mut Context<Self>,
    ) {
        if !self.thread_matches(thread_id) {
            return;
        }
        self.checkouts = checkouts
            .iter()
            .map(|checkout| {
                (
                    checkout.repository_id.to_string(),
                    checkout
                        .checkout_path
                        .clone()
                        .unwrap_or_else(|| checkout.source_path.clone()),
                )
            })
            .collect();
        cx.notify();
    }

    /// Whether the text of block `key` is selected (a click on it then
    /// ends a selection instead of opening the block).
    pub(crate) fn text_selected(&self, key: &str, cx: &App) -> bool {
        self.block_texts.has_selection(key, cx)
    }

    pub(crate) fn toggle_thought(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.tool_views.open_thoughts.remove(&index) {
            self.tool_views.open_thoughts.insert(index);
        }
        cx.notify();
    }

    /// A subagent line was clicked: show it next to this transcript, or
    /// select it (`main`, ⌘-click).
    pub(crate) fn open_agent(&mut self, thread: ThreadId, main: bool, cx: &mut Context<Self>) {
        cx.emit(if main {
            ChatEvent::OpenThread(thread)
        } else {
            ChatEvent::OpenInPane(thread)
        });
    }

    fn entry_author(&self, entry: &ChatEntry) -> AuthorKey {
        document::entry_author(
            entry,
            |id| self.remote_authors.get(id).cloned(),
            |id| self.received.get(id).map(|row| row.thread_id.clone()),
        )
    }

    /// "Subagent started" notes of subagents whose spawning call is shown:
    /// the call's "Agent spawned" line says it already.
    fn hidden_entries(&self, entries: &[ChatEntry]) -> HashSet<usize> {
        let spawned: HashSet<&ThreadId> = entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::ToolCall {
                    tool_use_id, name, ..
                } if tools::tool_kind(name) == ToolKind::Subthread => {
                    self.thread_index.spawned_by_tool(tool_use_id)
                }
                _ => None,
            })
            .collect();
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                matches!(
                    entry,
                    ChatEntry::Annotation(row)
                        if matches!(row.kind, crate::subagents::AnnotationKind::SubagentStarted(_))
                            && spawned.contains(&row.thread_id)
                )
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Which turn and block each comment thread goes under.
    fn place_comments(
        &self,
        entries: &[ChatEntry],
        turn_list: &[turns::Turn],
        groups: &[CommentGroup],
        p: &Palette,
    ) -> Placement {
        let mut placement = Placement::default();
        let turn_of = |index: usize| {
            turn_list
                .iter()
                .rposition(|turn| turn.entries.start <= index)
                .unwrap_or(0)
        };
        let mut message_turn: HashMap<&MessageId, usize> = HashMap::new();
        let mut edits: Vec<(usize, &str, Rc<tools::FileEdit>)> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match entry {
                ChatEntry::User {
                    message_id: Some(id),
                    ..
                }
                | ChatEntry::Assistant {
                    message_id: Some(id),
                    ..
                } => {
                    message_turn.insert(id, turn_of(index));
                }
                ChatEntry::ToolCall {
                    tool_use_id, name, ..
                } if tools::tool_kind(name) == ToolKind::Edit => {
                    if let Some(edit) = self.tool_views.file_edit(entry) {
                        edits.push((turn_of(index), tool_use_id.as_str(), edit));
                    }
                }
                _ => {}
            }
        }
        for (index, group) in groups.iter().enumerate() {
            let color =
                comments::passage_color(&group.phase(), p).unwrap_or(gpui_kit::transparent_black());
            match &group.root.target {
                CommentTarget::MessageRange { message_id, .. } => {
                    let Some(&turn) = message_turn.get(message_id) else {
                        continue;
                    };
                    placement.by_turn.entry(turn).or_default().push(index);
                    let Some(proxy) = comments::parse_proxy(&group.root.body) else {
                        continue;
                    };
                    if proxy.kind == comments::KIND_PATCH {
                        let wanted: Vec<&str> = proxy
                            .excerpt
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .collect();
                        for (edit_turn, id, edit) in &edits {
                            if *edit_turn != turn {
                                continue;
                            }
                            let lines = tools::patch_lines_matching(edit, &wanted);
                            if !lines.is_empty() {
                                let slot = placement.patches.entry(id.to_string()).or_default();
                                slot.lines.extend(lines);
                                slot.groups.push(index);
                                break;
                            }
                        }
                    } else {
                        placement.marks.entry(turn).or_default().push(Mark {
                            group: Some(index),
                            kind: proxy.kind,
                            excerpt: proxy.excerpt,
                            color,
                        });
                    }
                }
                CommentTarget::Code { anchor } => {
                    for (turn, id, edit) in &edits {
                        if !comments::code_on_edit(anchor, &edit.path, &tools::new_lines(edit)) {
                            continue;
                        }
                        let slot = placement.patches.entry(id.to_string()).or_default();
                        slot.lines.extend(tools::patch_lines_in(
                            edit,
                            anchor.start_line..=anchor.end_line,
                        ));
                        slot.groups.push(index);
                        placement.by_turn.entry(*turn).or_default().push(index);
                        break;
                    }
                }
                CommentTarget::DiffHunk { .. } => {}
            }
        }
        if let Some(DraftAnchor::Block {
            kind,
            excerpt,
            turn,
            ..
        }) = self.draft.as_ref().map(|open| &open.anchor)
        {
            placement.marks.entry(*turn).or_default().push(Mark {
                group: None,
                kind,
                excerpt: excerpt.clone(),
                color: p.accent.opacity(0.3),
            });
        }
        placement
    }

    /// An author's round avatar for the gutter.
    fn avatar(&self, author: &AuthorKey, me: Option<&str>, row: usize, p: &Palette) -> AnyElement {
        let (element, tip): (AnyElement, String) = match author {
            AuthorKey::Me => (
                match me {
                    Some(name) => {
                        crate::presence::avatar(p, name, AVATAR, false).into_any_element()
                    }
                    None => {
                        glyph_avatar(IconName::User, p.selected, p.text_muted).into_any_element()
                    }
                },
                "You".into(),
            ),
            AuthorKey::Person(name) => (
                crate::presence::avatar(p, name, AVATAR, false).into_any_element(),
                name.clone(),
            ),
            AuthorKey::Agent => (
                glyph_avatar(IconName::Bot, p.accent_soft, p.accent_text).into_any_element(),
                "Claude".into(),
            ),
            AuthorKey::PersonAgent(name) => (
                div()
                    .relative()
                    .child(crate::presence::avatar(p, name, AVATAR, false))
                    .child(
                        div()
                            .absolute()
                            .bottom(px(-3.))
                            .right(px(-3.))
                            .size(px(12.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(p.accent_soft)
                            .border_1()
                            .border_color(p.bg)
                            .child(
                                Icon::new(IconName::Bot)
                                    .with_size(px(8.))
                                    .text_color(p.accent_text),
                            ),
                    )
                    .into_any_element(),
                format!("{name}'s Claude"),
            ),
            AuthorKey::ThreadAgent(thread) => (
                glyph_avatar(IconName::Bot, p.selected, p.text_muted).into_any_element(),
                format!(
                    "Agent of {}",
                    self.thread_index.title(thread).unwrap_or("another thread")
                ),
            ),
        };
        let tip: SharedString = tip.into();
        div()
            .id(("doc-avatar", row))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .child(element)
            .into_any_element()
    }

    /// One layout item as a block, with the threads on its passages and
    /// the composer of a new one under it.
    #[allow(clippy::too_many_arguments)]
    fn render_item(
        &mut self,
        item: &Item,
        entries: &[ChatEntry],
        turn: usize,
        placement: &Placement,
        ctx: &CardCtx,
        hits: &mut Vec<SelectionHit>,
        row_of: &mut HashMap<MessageId, usize>,
        row: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let drawn_from = self.block_texts.drawn_len();
        let no_marks = Vec::new();
        let marks = placement.marks.get(&turn).unwrap_or(&no_marks);
        let content = match item {
            Item::Group(range) => {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_activity(range.clone(), entries, &mut doc, cx)
            }
            Item::Edits(range) => {
                self.render_edits(range.clone(), entries, turn, placement, ctx, marks, p, cx)
            }
            Item::Single(ix) => {
                self.render_single(*ix, entries, turn, ctx, marks, hits, row_of, row, p, cx)
            }
        };
        let claimed = self.block_texts.take_claimed();
        let draft_here = match self.draft.as_ref().map(|open| &open.anchor) {
            Some(DraftAnchor::Block { key, .. }) => self.block_texts.drawn_since(drawn_from, key),
            _ => false,
        };
        if claimed.is_empty() && !draft_here {
            return content;
        }
        let mut column = v_flex().w_full().gap(theme::sp(1.5)).child(content);
        for group in claimed {
            column = column.child(self.inline_card(ctx, group, "inline", p, cx));
        }
        if draft_here && let Some(open) = &self.draft {
            column = column.child(self.render_open_draft("chat-block-draft", open, p, cx));
        }
        column.into_any_element()
    }

    /// File edits: an "Apply patch" line and diff each, with the threads
    /// on their lines.
    #[allow(clippy::too_many_arguments)]
    fn render_edits(
        &mut self,
        range: Range<usize>,
        entries: &[ChatEntry],
        turn: usize,
        placement: &Placement,
        ctx: &CardCtx,
        marks: &[Mark],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let empty = PatchComments::default();
        let mut column = v_flex().w_full().gap(theme::sp(1.5));
        for ix in range {
            let entry = &entries[ix];
            let ChatEntry::ToolCall { tool_use_id, .. } = entry else {
                continue;
            };
            let on_lines = placement.patches.get(tool_use_id).unwrap_or(&empty);
            let element = {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                match tools::render_patch(
                    ix,
                    entry,
                    &mut doc,
                    self.patch_pick.as_ref(),
                    &on_lines.lines,
                    cx,
                ) {
                    Some(element) => element,
                    None => tools::render_activity(ix..ix + 1, entries, &mut doc, cx),
                }
            };
            column = column.child(element);
            for &group in &on_lines.groups {
                column = column.child(self.inline_card(ctx, group, "inline", p, cx));
            }
            if let Some(open) = &self.draft
                && matches!(&open.anchor, DraftAnchor::Patch { tool_use_id: id } if id == tool_use_id)
            {
                column = column.child(self.render_open_draft("chat-patch-draft", open, p, cx));
            }
        }
        column.into_any_element()
    }

    /// One entry as a block.
    #[allow(clippy::too_many_arguments)]
    fn render_single(
        &mut self,
        ix: usize,
        entries: &[ChatEntry],
        turn: usize,
        ctx: &CardCtx,
        marks: &[Mark],
        hits: &mut Vec<SelectionHit>,
        row_of: &mut HashMap<MessageId, usize>,
        row: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = &entries[ix];
        match entry {
            ChatEntry::User { text, message_id } => {
                self.render_prompt(ix, text, message_id.as_ref(), ctx, hits, row_of, row, p, cx)
            }
            ChatEntry::Approval(card) => render_approval(card, p, cx),
            ChatEntry::Assistant {
                markdown,
                message_id: Some(id),
                ..
            } if self.remote_authors.contains_key(id) => {
                let (name, role) = self.remote_authors[id].clone();
                if role == Role::Assistant && self.comments_enabled() {
                    let body = self.message_body(id, markdown, 0, false, ctx, hits, p, cx);
                    row_of.insert(id.clone(), row);
                    self.message_row(id, body, p, cx)
                } else {
                    crate::presence::remote_notice(p, &name, markdown)
                }
            }
            ChatEntry::Assistant {
                markdown,
                streaming: false,
                message_id: Some(id),
            } if self.comments_enabled() && self.stored_len.get(id) == Some(&markdown.len()) => {
                let body = self.message_body(id, markdown, 0, false, ctx, hits, p, cx);
                row_of.insert(id.clone(), row);
                self.message_row(id, body, p, cx)
            }
            ChatEntry::Assistant {
                markdown,
                streaming,
                ..
            } => div()
                .w_full()
                .font_family(theme::PROSE_FONT)
                .text_size(theme::text_prose())
                .line_height(relative(1.6))
                .text_color(p.text)
                .child(
                    TextView::markdown(("assistant-md", ix), markdown.clone())
                        .stream_fade(*streaming),
                )
                .into_any_element(),
            ChatEntry::ToolCall {
                tool_use_id,
                name,
                input_json,
                output,
                finished,
                is_error,
                ..
            } if tools::tool_kind(name) == ToolKind::Subthread => {
                let mut link =
                    tools::subthread_link(name, input_json, output.as_ref(), &self.known_threads);
                crate::subagents::link_spawned_child(&mut link, tool_use_id, &self.thread_index);
                let (label, detail) = if link.message {
                    ("Message to agent:", link.title.clone())
                } else {
                    let child = link.thread.as_ref();
                    (
                        "Agent spawned:",
                        tools::agent_detail(
                            &link.title,
                            child
                                .and_then(|id| self.thread_index.profile(id))
                                .map(|profile| profile.label()),
                            child.and_then(|id| self.thread_index.model(id)),
                        ),
                    )
                };
                let trailing = match link
                    .thread
                    .as_ref()
                    .and_then(|id| self.thread_index.status(id))
                {
                    Some(status) => vec![crate::subagents::status_glyph(status, 12., p)],
                    None if !*finished || *is_error => {
                        vec![tools::status_icon(*finished, *is_error, p)]
                    }
                    None => Vec::new(),
                };
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_agent(
                    ("agent-call", ix),
                    label,
                    &detail,
                    format!("agent-{tool_use_id}"),
                    link.thread,
                    trailing,
                    None,
                    &mut doc,
                    cx,
                )
            }
            ChatEntry::Annotation(note) => self.render_note(ix, note, turn, marks, p, cx),
            ChatEntry::ToolCall { name, .. } if tools::tool_kind(name) == ToolKind::Edit => {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                let empty = HashSet::new();
                tools::render_patch(ix, entry, &mut doc, self.patch_pick.as_ref(), &empty, cx)
                    .unwrap_or_else(|| tools::render_activity(ix..ix + 1, entries, &mut doc, cx))
            }
            ChatEntry::ToolCall { .. } | ChatEntry::Thinking { .. } => {
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_activity(ix..ix + 1, entries, &mut doc, cx)
            }
        }
    }

    /// A prompt as plain document text, a little stronger than prose: the
    /// comments it delivered, attached terminal snapshots, then the text.
    #[allow(clippy::too_many_arguments)]
    fn render_prompt(
        &mut self,
        ix: usize,
        text: &str,
        message_id: Option<&MessageId>,
        ctx: &CardCtx,
        hits: &mut Vec<SelectionHit>,
        row_of: &mut HashMap<MessageId, usize>,
        row: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut block = v_flex().w_full().gap(theme::sp(1.5));
        if let Some(note) = message_id.and_then(|id| self.received.get(id)).cloned() {
            block = block.child(crate::subagents::agent_message_header(
                ix,
                &note,
                &self.thread_index,
                p,
                cx,
            ));
        }
        match message_id {
            Some(id) if self.comments_enabled() => {
                let split = comments::split_prompt_block(text);
                let base = split.as_ref().map_or(0, |split| split.body_start);
                if let Some(split) = &split {
                    block = block.child(self.sent_block(id, text, split, p, cx));
                }
                let context = snapshot::split_context(&text[base..]);
                if !context.is_empty() {
                    block = block.child(snapshot::render_context(
                        id,
                        &context,
                        &self.open_snapshots,
                        p,
                        cx,
                    ));
                }
                let base = base + context.text.start;
                let body = &text[base..base + context.text.len()];
                if !body.trim().is_empty() {
                    let content = self.message_body(id, body, base, true, ctx, hits, p, cx);
                    block = block.child(self.message_row(id, content, p, cx));
                }
                row_of.insert(id.clone(), row);
            }
            _ => {
                block = block.child(
                    div()
                        .font_family(theme::PROSE_FONT)
                        .text_size(theme::text_prose())
                        .line_height(relative(1.6))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child(comments::prompt_body(text).to_string()),
                );
            }
        }
        block.into_any_element()
    }

    /// A stored note linking another thread: "Agent spawned: …", "Agent
    /// completed: …", or an agent message sent or received.
    fn render_note(
        &mut self,
        ix: usize,
        note: &crate::subagents::AnnotationRow,
        turn: usize,
        marks: &[Mark],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::subagents::AnnotationKind;
        let known = self.thread_index.title(&note.thread_id).is_some();
        let thread = known.then(|| note.thread_id.clone());
        let title = self
            .thread_index
            .title(&note.thread_id)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&note.title)
            .to_string();
        let key = format!("note-{}", note.message_id);
        match &note.kind {
            AnnotationKind::SubagentStarted(profile) => {
                let detail = tools::agent_detail(
                    &title,
                    Some(profile.label()),
                    self.thread_index.model(&note.thread_id),
                );
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_agent(
                    ("agent-note", ix),
                    tools::agent_label(None, true),
                    &detail,
                    key,
                    thread,
                    Vec::new(),
                    Some(crate::subagents::row_text(note, &self.thread_index)),
                    &mut doc,
                    cx,
                )
            }
            AnnotationKind::SubagentFinished(status) => {
                let detail = match &note.body {
                    Some(body) => format!("{title} \u{2014} {body}"),
                    None => title,
                };
                let mut doc = DocCtx {
                    p,
                    state: &self.tool_views,
                    texts: &mut self.block_texts,
                    marks,
                    turn,
                };
                tools::render_agent(
                    ("agent-note", ix),
                    tools::agent_label(Some(*status), false),
                    &detail,
                    key,
                    thread,
                    vec![crate::subagents::status_glyph(*status, 12., p)],
                    Some(crate::subagents::row_text(note, &self.thread_index)),
                    &mut doc,
                    cx,
                )
            }
            AnnotationKind::Sent | AnnotationKind::Received => {
                let header =
                    crate::subagents::agent_message_header(ix, note, &self.thread_index, p, cx);
                let body = note.body.clone().map(|body| {
                    let view = self.block_texts.view(
                        &key,
                        &crate::doc_text::escape_markdown(&body),
                        &body,
                        comments::KIND_SUBAGENT,
                        turn,
                        marks,
                        cx,
                    );
                    div()
                        .w_full()
                        .pl(theme::sp(3.))
                        .border_l_2()
                        .border_color(p.border_strong)
                        .text_size(theme::text_ui())
                        .line_height(relative(1.5))
                        .text_color(p.text_muted)
                        .child(view)
                });
                v_flex()
                    .w_full()
                    .gap(theme::sp(1.))
                    .child(header)
                    .children(body)
                    .into_any_element()
            }
        }
    }

    /// A stored message with its hover actions (comment, fork) right of
    /// the text column.
    fn message_row(
        &self,
        message_id: &MessageId,
        content: AnyElement,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hover: SharedString = format!("msg-{message_id}").into();
        let id = message_id.clone();
        let add = div()
            .id(SharedString::from(format!("comment-add-{message_id}")))
            .size(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::radius_sm())
            .cursor_pointer()
            .hover(|style| style.bg(p.hover))
            .tooltip(|window, cx| {
                Tooltip::new(
                    "Comment on the selection or the whole message. Tip: select text anywhere and start typing.",
                )
                .build(window, cx)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.comment_on_message(id.clone(), window, cx);
            }))
            .child(
                Icon::new(IconName::MessageSquarePlus)
                    .with_size(px(13.))
                    .text_color(p.text_faint),
            );
        div()
            .relative()
            .w_full()
            .group(hover.clone())
            .child(content)
            .when(self.uses_daemon(), |this| {
                let fork =
                    crate::forks::fork_button(message_id, self.fork_index.state(message_id), p, cx);
                this.child(
                    v_flex()
                        .absolute()
                        .top_0()
                        .right(px(-30.))
                        .gap(theme::sp(0.5))
                        .opacity(0.)
                        .group_hover(hover, |style| style.opacity(1.))
                        .child(add)
                        .child(fork),
                )
            })
            .into_any_element()
    }

    /// "N comments ⌄" at the end of a turn: the passage of each thread on
    /// it, quoted, with the thread's comments and replies.
    fn turn_comments(
        &self,
        turn: usize,
        list: &[usize],
        ctx: &CardCtx,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let default_open = list
            .iter()
            .any(|&group| ctx.groups[group].phase() == Phase::Pending);
        let open = self
            .turn_comments_open
            .get(&turn)
            .copied()
            .unwrap_or(default_open);
        let total: usize = list.iter().map(|&group| ctx.groups[group].len()).sum();
        let header = h_flex()
            .id(("turn-comments", turn))
            .min_h(px(22.))
            .gap(theme::sp(1.5))
            .items_center()
            .cursor_pointer()
            .text_size(theme::text_ui())
            .text_color(p.text_muted)
            .hover(|style| style.text_color(p.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.turn_comments_open.insert(turn, !open);
                cx.notify();
            }))
            .child(comments::plural(total, "comment", "comments"))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .with_size(px(12.))
                .text_color(p.text_faint),
            );
        let mut column = v_flex().w_full().gap(theme::sp(2.)).child(header);
        if open {
            for &index in list {
                let group = &ctx.groups[index];
                let quote = match comments::parse_proxy(&group.root.body) {
                    Some(proxy) => proxy.excerpt,
                    None => group
                        .root
                        .excerpt
                        .clone()
                        .unwrap_or_else(|| comments::target_label(&group.root.target)),
                };
                column = column.child(
                    v_flex()
                        .w_full()
                        .p(theme::sp(2.))
                        .gap(theme::sp(2.))
                        .rounded(theme::radius())
                        .border_1()
                        .border_color(p.border)
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .px(theme::sp(1.))
                                .rounded(theme::radius_sm())
                                .bg(comments::mark_color(p).opacity(0.14))
                                .text_size(theme::text_ui())
                                .line_height(relative(1.6))
                                .text_color(p.text)
                                .child(comments::one_line(&quote, 160)),
                        )
                        .child(self.inline_card(ctx, index, "turn", p, cx)),
                );
            }
        }
        column.into_any_element()
    }
}

// -- turn rail --------------------------------------------------------------

/// Rail width, inset from the top and bottom of the transcript, the
/// dashes' distance from the right edge, and the hover card's width.
const RAIL_WIDTH: f32 = 26.;
const RAIL_INSET: f32 = 10.;
const RAIL_DASH_RIGHT: f32 = 8.;
const RAIL_CARD_WIDTH: f32 = 320.;

impl ChatPanel {
    /// Turn extents from the transcript's last layout.
    fn rail_geometry(&self) -> Option<RailGeometry> {
        let count = self.scroll.children_count();
        if count == 0 || self.rail.turn_rows.is_empty() {
            return None;
        }
        let bounds = self.scroll.bounds();
        let view_height = f32::from(bounds.size.height);
        if view_height <= 0. {
            return None;
        }
        let top = |row: usize| {
            self.scroll
                .bounds_for_item(row)
                .map(|item| f32::from(item.top() - bounds.top()))
        };
        let starts: Option<Vec<f32>> = self.rail.turn_rows.iter().map(|&row| top(row)).collect();
        let starts = starts?;
        let content_height = view_height + f32::from(self.scroll.max_offset().y).max(0.);
        let content_end = self
            .scroll
            .bounds_for_item(count - 1)
            .map(|item| f32::from(item.bottom() - bounds.top()))
            .unwrap_or(content_height);
        let view_top = -f32::from(self.scroll.offset().y);
        Some(RailGeometry {
            spans: turns::turn_spans(&starts, content_end),
            content_height,
            view: Span::new(view_top, view_top + view_height),
        })
    }

    fn scroll_to_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        if let Some(&row) = self.rail.turn_rows.get(turn) {
            self.scroll.scroll_to_top_of_item(row);
            cx.notify();
        }
    }

    /// Click a tick: scroll to its turn; ⌘+click bookmarks it instead.
    fn click_turn(&mut self, turn: usize, bookmark: bool, cx: &mut Context<Self>) {
        if !bookmark {
            self.scroll_to_turn(turn, cx);
            return;
        }
        let message = turns::segment_turns(&self.entries)
            .get(turn)
            .and_then(|turn| turn.message_id.clone());
        if let (Some(thread), Some(message)) = (self.selected_thread.clone(), message) {
            self.bookmarks.toggle(&thread, &message);
            cx.notify();
        }
    }

    /// ⌥+scroll: move one turn per notch (or per stretch of a trackpad
    /// swipe). Returns whether the event was used.
    fn scroll_by_marker(&mut self, delta: Pixels, precise: bool, cx: &mut Context<Self>) -> bool {
        let Some(geometry) = self.rail_geometry() else {
            return false;
        };
        if geometry.spans.len() < 2 {
            return false;
        }
        let delta = f32::from(delta);
        if delta == 0. {
            return true;
        }
        if precise {
            if self.rail.pending.signum() != delta.signum() {
                self.rail.pending = 0.;
            }
            self.rail.pending += delta;
            if self.rail.pending.abs() < 40. {
                return true;
            }
        }
        let cooldown = if precise { 350 } else { 90 };
        if self
            .rail
            .last_jump
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(cooldown))
        {
            self.rail.pending = 0.;
            return true;
        }
        self.rail.pending = 0.;
        // Content moves up (negative delta) when scrolling down.
        let down = delta < 0.;
        if let Some(turn) = turns::marker_target(&geometry.spans, geometry.view.top, down) {
            self.rail.last_jump = Some(std::time::Instant::now());
            self.scroll_to_turn(turn, cx);
        }
        true
    }

    /// The rail on the transcript's right edge and the hovered turn's card.
    fn render_rail(
        &self,
        turn_list: &[turns::Turn],
        geometry: &RailGeometry,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (Option<AnyElement>, Option<AnyElement>) {
        let bounds = self.scroll.bounds();
        let rail_height = f32::from(bounds.size.height) - 2. * RAIL_INSET;
        if rail_height < 40. || geometry.spans.len() != turn_list.len() {
            return (None, None);
        }
        let current = turns::current_turn(&geometry.spans, geometry.view, geometry.content_height);
        let dashes = turns::rail_dashes(&geometry.spans, geometry.view, current, rail_height);
        let marked = |turn: &turns::Turn| match (&self.selected_thread, &turn.message_id) {
            (Some(thread), Some(message)) => self.bookmarks.contains(thread, message),
            _ => false,
        };
        let pitch = match dashes.as_slice() {
            [first, second, ..] => second.top - first.top,
            _ => turns::DASH_PITCH,
        };

        let mut rail = div().relative().size_full();
        for (index, dash) in dashes.iter().enumerate() {
            let hovered = self.rail.hovered == Some(index);
            let bookmarked = marked(&turn_list[index]);
            let color = if bookmarked {
                p.accent
            } else if dash.current || hovered {
                p.text
            } else if dash.in_view {
                p.text_muted
            } else {
                p.text_faint
            };
            let length = if dash.current || hovered {
                turns::CURRENT_DASH_LENGTH
            } else {
                turns::DASH_LENGTH
            };
            // The hit area fills the pitch so dense rails stay easy to hit.
            let slot = pitch.max(turns::DASH_THICKNESS + 2.);
            let slop = (slot - turns::DASH_THICKNESS) / 2.;
            rail = rail.child(
                div()
                    .id(("rail-dash", index))
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(dash.top - slop))
                    .h(px(slot))
                    .cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.rail.hovered = Some(index);
                        } else if this.rail.hovered == Some(index) {
                            this.rail.hovered = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.click_turn(index, event.modifiers().platform, cx);
                    }))
                    .child(
                        div()
                            .absolute()
                            .right(px(RAIL_DASH_RIGHT))
                            .top(px(slop))
                            .h(px(turns::DASH_THICKNESS))
                            .w(px(length))
                            .rounded(px(1.))
                            .bg(color),
                    ),
            );
        }
        let rail = div()
            .absolute()
            .top(px(RAIL_INSET))
            .bottom(px(RAIL_INSET))
            .right_0()
            .w(px(RAIL_WIDTH))
            .child(rail)
            .into_any_element();

        let card = self
            .rail
            .hovered
            .filter(|index| *index < turn_list.len())
            .map(|index| {
                let at = point(
                    bounds.right() - px(RAIL_WIDTH + 6.),
                    bounds.top() + px(RAIL_INSET + dashes[index].top - 10.),
                );
                let card = self.turn_card(index, turn_list, marked(&turn_list[index]), p);
                deferred(
                    anchored()
                        .anchor(Anchor::TopRight)
                        .position(at)
                        .snap_to_window_with_margin(px(8.))
                        .child(card),
                )
                .with_priority(2)
                .into_any_element()
            });
        (Some(rail), card)
    }

    /// The hover card: question, answer summary, author, model, time and
    /// the rail's modifier hints.
    fn turn_card(
        &self,
        index: usize,
        turn_list: &[turns::Turn],
        bookmarked: bool,
        p: &Palette,
    ) -> AnyElement {
        let turn = &turn_list[index];
        let question = turns::question_preview(&turn.question, 3, 220);
        let at = turn
            .message_id
            .as_ref()
            .and_then(|id| self.prompt_times.get(id))
            .copied();
        let model = self
            .model
            .as_deref()
            .map(|model| format!("{} via Claude Code", turns::model_label(model)))
            .unwrap_or_else(|| "Claude Code".into());
        let remote = turn
            .message_id
            .as_ref()
            .and_then(|id| self.remote_author(id))
            .map(str::to_string);
        let (author, avatar_icon, avatar_bg, avatar_fg) = if turn.question.is_empty() {
            ("Claude", IconName::Bot, p.accent_soft, p.accent_text)
        } else {
            ("You", IconName::User, p.selected, p.text_muted)
        };
        let author: SharedString = remote.clone().unwrap_or_else(|| author.into()).into();
        let key = |label: &'static str| {
            div()
                .px(px(4.))
                .min_w(px(16.))
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::radius_sm())
                .border_1()
                .border_color(p.border_strong)
                .bg(p.bg)
                .text_color(p.text_muted)
                .child(label)
        };
        let hint = |keys: &'static str, label: &'static str| {
            h_flex()
                .gap(theme::sp(1.))
                .items_center()
                .child(key(keys))
                .child(format!("+ {label}"))
        };
        v_flex()
            .w(px(RAIL_CARD_WIDTH))
            .rounded(theme::radius_lg())
            .border_1()
            .border_color(p.border_strong)
            .bg(p.elevated)
            .shadow_md()
            .overflow_hidden()
            .child(
                v_flex()
                    .px(theme::sp(3.))
                    .pt(theme::sp(2.5))
                    .pb(theme::sp(3.))
                    .gap(theme::sp(2.))
                    .child(
                        h_flex()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(format!("Turn {} of {}", index + 1, turn_list.len()))
                            .when(bookmarked, |this| {
                                this.child(div().flex_1()).child(
                                    h_flex()
                                        .gap(theme::sp(1.))
                                        .items_center()
                                        .text_color(p.accent_text)
                                        .child(Icon::new(IconName::Bookmark).with_size(px(11.)))
                                        .child("Bookmarked"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(theme::text_ui())
                            .line_height(relative(1.4))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if question.is_empty() {
                                p.text_muted
                            } else {
                                p.text
                            })
                            .child(if question.is_empty() {
                                "Turn started by the agent".to_string()
                            } else {
                                question
                            }),
                    )
                    .when_some(turn.summary.clone(), |this, summary| {
                        this.child(
                            div()
                                .w_full()
                                .truncate()
                                .text_size(theme::text_sm())
                                .text_color(p.text_muted)
                                .child(summary),
                        )
                    })
                    .child(
                        h_flex()
                            .pt(theme::sp(1.))
                            .gap(theme::sp(2.))
                            .items_center()
                            .child(match &remote {
                                Some(name) => crate::presence::avatar(p, name, 22., false),
                                None => div()
                                    .size(px(22.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(avatar_bg)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(avatar_icon)
                                            .with_size(px(12.))
                                            .text_color(avatar_fg),
                                    ),
                            })
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(theme::text_sm())
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text)
                                            .child(author),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(theme::text_xs())
                                            .text_color(p.text_faint)
                                            .child(model),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .flex_shrink_0()
                                    .items_end()
                                    .text_size(theme::text_xs())
                                    .child(div().text_color(p.text_muted).child(match at {
                                        Some(at) => comments::relative_time(
                                            at.timestamp(),
                                            comments::now_secs(),
                                        ),
                                        None => "just now".into(),
                                    }))
                                    .when_some(at, |this, at| {
                                        this.child(
                                            div()
                                                .text_color(p.text_faint)
                                                .child(turns::absolute_time(at)),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px(theme::sp(3.))
                    .py(theme::sp(1.5))
                    .gap(theme::sp(4.))
                    .items_center()
                    .border_t_1()
                    .border_color(p.border)
                    .bg(p.panel)
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child(hint("\u{2318}", "click to bookmark"))
                    .child(hint("\u{2325}", "scroll by marker")),
            )
            .into_any_element()
    }
}

fn notice(p: &Palette, icon: IconName, color: Hsla, text: String) -> impl IntoElement {
    h_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .text_size(theme::text_sm())
        .text_color(p.text_muted)
        .child(Icon::new(icon).with_size(px(14.)).text_color(color))
        .child(div().flex_1().child(text))
}

pub fn empty_state(
    p: &Palette,
    icon: IconName,
    title: &'static str,
    body: &'static str,
    keys: &[(&'static str, &'static str)],
) -> AnyElement {
    v_flex()
        .w_full()
        .pt(theme::sp(24.))
        .items_center()
        .gap(theme::sp(3.))
        .child(
            div()
                .size(px(44.))
                .rounded(theme::radius_lg())
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).with_size(px(20.)).text_color(p.text_muted)),
        )
        .child(
            div()
                .text_size(theme::text_title())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(title),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_size(theme::text_ui())
                .line_height(relative(1.5))
                .text_color(p.text_muted)
                .child(body),
        )
        .when(!keys.is_empty(), |this| {
            this.child(
                h_flex()
                    .pt(theme::sp(2.))
                    .gap(theme::sp(4.))
                    .children(keys.iter().map(|(keys, label)| {
                        h_flex()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_sm())
                            .text_color(p.text_faint)
                            .children(
                                Keystroke::parse(keys)
                                    .ok()
                                    .map(|stroke| Kbd::new(stroke).outline()),
                            )
                            .child(*label)
                    })),
            )
        })
        .into_any_element()
}

/// One line describing what a tool call does, from its input.
pub fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input.get(key).and_then(|value| value.as_str());
    let summary = match name {
        "Bash" => field("command").map(str::to_string),
        "Grep" => field("pattern").map(|pattern| match field("path") {
            Some(path) => format!("{pattern}  in {path}"),
            None => pattern.to_string(),
        }),
        "Glob" => field("pattern").map(str::to_string),
        "WebFetch" => field("url").map(str::to_string),
        "WebSearch" => field("query").map(str::to_string),
        "Task" | "Agent" => field("description").map(str::to_string),
        "TodoWrite" => input
            .get("todos")
            .and_then(|todos| todos.as_array())
            .map(|todos| format!("{} items", todos.len())),
        _ => None,
    };
    let summary = summary
        .or_else(|| {
            [
                "file_path",
                "path",
                "notebook_path",
                "command",
                "pattern",
                "url",
                "query",
            ]
            .iter()
            .find_map(|key| field(key).map(str::to_string))
        })
        .or_else(|| {
            input.as_object().and_then(|map| {
                map.values()
                    .find_map(|value| value.as_str().map(str::to_string))
            })
        })
        .unwrap_or_default();
    let line = summary.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 || summary.lines().nth(1).is_some() {
        out.push('…');
    }
    out
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

fn stop_message(running: bool, selected_thread: Option<&ThreadId>) -> Option<ClientMessage> {
    if !running {
        return None;
    }
    selected_thread.map(|thread_id| ClientMessage::Interrupt {
        thread_id: thread_id.clone(),
    })
}

fn stop_transition(
    lifecycle: TurnLifecycle,
    selected_thread: Option<&ThreadId>,
) -> (TurnLifecycle, Option<ClientMessage>) {
    match lifecycle {
        TurnLifecycle::Running => (TurnLifecycle::Stopping, stop_message(true, selected_thread)),
        TurnLifecycle::Stopping => (TurnLifecycle::Stopping, None),
        TurnLifecycle::AwaitingAssistant => (TurnLifecycle::AwaitingAssistant, None),
        TurnLifecycle::Idle => (TurnLifecycle::Idle, None),
    }
}

fn accepts_agent_events(lifecycle: TurnLifecycle) -> bool {
    // The protocol has no turn ID, so events are fenced by the current
    // turn's terminal/reconciliation boundary and rely on daemon ordering.
    matches!(lifecycle, TurnLifecycle::Running | TurnLifecycle::Stopping)
}

fn can_submit(lifecycle: TurnLifecycle) -> bool {
    lifecycle == TurnLifecycle::Idle
}

fn terminal_transition(uses_daemon: bool, final_assistant_persisted: bool) -> TurnLifecycle {
    if uses_daemon && !final_assistant_persisted {
        TurnLifecycle::AwaitingAssistant
    } else {
        TurnLifecycle::Idle
    }
}

fn unscoped_error_transition(lifecycle: TurnLifecycle, message: String) -> (TurnLifecycle, String) {
    (lifecycle, message)
}

fn stored_assistant_transition(lifecycle: TurnLifecycle) -> TurnLifecycle {
    if lifecycle == TurnLifecycle::AwaitingAssistant {
        TurnLifecycle::Idle
    } else {
        lifecycle
    }
}

fn record_message_id(seen: &mut HashSet<MessageId>, id: MessageId) -> bool {
    seen.insert(id)
}

fn last_unpersisted_assistant(entries: &[ChatEntry]) -> Option<usize> {
    let last_user = entries
        .iter()
        .rposition(|entry| matches!(entry, ChatEntry::User { .. }))
        .unwrap_or(0);
    entries
        .iter()
        .enumerate()
        .skip(last_user + 1)
        .rev()
        .find_map(|(index, entry)| match entry {
            ChatEntry::Assistant {
                message_id: None, ..
            } => Some(index),
            _ => None,
        })
}

fn reconcile_stored_assistant(
    entries: &mut [ChatEntry],
    lifecycle: TurnLifecycle,
    message: &Message,
) -> bool {
    if !matches!(
        lifecycle,
        TurnLifecycle::Running | TurnLifecycle::Stopping | TurnLifecycle::AwaitingAssistant
    ) {
        return false;
    }
    let text = message.text();
    if let Some(index) = last_unpersisted_assistant(entries) {
        if let ChatEntry::Assistant {
            markdown,
            streaming,
            message_id,
        } = &mut entries[index]
        {
            if markdown.is_empty() || (!text.is_empty() && *markdown != text) {
                *markdown = text;
            }
            *streaming = false;
            *message_id = Some(message.id.clone());
            return true;
        }
    }
    false
}

/// Add a stored user message, or adopt the optimistic entry the composer
/// pushed for it. The stored text may start with the `<review-comments>`
/// block of the comments delivered with the prompt, so the typed prompt is
/// compared with the text after it.
fn append_stored_user(
    entries: &mut Vec<ChatEntry>,
    pending_prompt: &mut Option<String>,
    message: &Message,
) {
    let text = message.text();
    let body = comments::prompt_body(&text);
    if pending_prompt.as_deref() == Some(body) {
        *pending_prompt = None;
    }
    if let Some(ChatEntry::User {
        text: existing,
        message_id: message_id @ None,
    }) = entries.last_mut()
        && (existing.as_str() == body || existing == &text)
    {
        *existing = text;
        *message_id = Some(message.id.clone());
        return;
    }
    entries.push(ChatEntry::User {
        text,
        message_id: Some(message.id.clone()),
    });
}

fn append_stored_assistant(
    entries: &mut Vec<ChatEntry>,
    lifecycle: TurnLifecycle,
    message: &Message,
) {
    if !reconcile_stored_assistant(entries, lifecycle, message) {
        let text = message.text();
        if !text.is_empty() {
            entries.push(ChatEntry::Assistant {
                markdown: text,
                streaming: false,
                message_id: Some(message.id.clone()),
            });
        }
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                entries.push(ChatEntry::ToolCall {
                    tool_use_id: id.clone(),
                    name: name.clone(),
                    input_json: input.clone(),
                    finished: false,
                    is_error: false,
                    expanded: false,
                    output: None,
                });
            }
        }
    }
}

/// Add an approval card unless the daemon is replaying one already shown.
fn push_approval(
    entries: &mut Vec<ChatEntry>,
    request_id: String,
    tool_name: String,
    input_json: serde_json::Value,
) {
    let known = entries
        .iter()
        .any(|entry| matches!(entry, ChatEntry::Approval(card) if card.request_id == request_id));
    if !known {
        if let Some(ChatEntry::Assistant { streaming, .. }) = entries.last_mut() {
            *streaming = false;
        }
        entries.push(ChatEntry::Approval(ApprovalCard {
            request_id,
            tool_name,
            input_json,
            state: ApprovalState::Pending,
        }));
    }
}

fn set_approval_state(entries: &mut [ChatEntry], request_id: &str, state: ApprovalState) -> bool {
    for entry in entries.iter_mut().rev() {
        if let ChatEntry::Approval(card) = entry
            && card.request_id == request_id
        {
            card.state = state;
            return true;
        }
    }
    false
}

/// Move a pending card to `Sending` and build the reply. `None` when the
/// card is unknown or already answered (double clicks, stale shortcuts).
fn answer_approval(
    entries: &mut [ChatEntry],
    thread_id: &ThreadId,
    request_id: &str,
    decision: ApprovalDecision,
) -> Option<ClientMessage> {
    let card = entries.iter_mut().find_map(|entry| match entry {
        ChatEntry::Approval(card) if card.request_id == request_id => Some(card),
        _ => None,
    })?;
    if card.state != ApprovalState::Pending {
        return None;
    }
    card.state = ApprovalState::Sending(decision);
    Some(ClientMessage::RespondApproval {
        thread_id: thread_id.clone(),
        request_id: request_id.to_string(),
        decision,
    })
}

/// A finished turn cannot still be waiting for an approval.
fn expire_open_approvals(entries: &mut [ChatEntry]) {
    for entry in entries.iter_mut() {
        if let ChatEntry::Approval(card) = entry
            && card.is_open()
        {
            card.state = ApprovalState::Expired;
        }
    }
}

/// Events that only appear inside a running turn. Seen while idle, they mean
/// a turn was started elsewhere (daemon-run subthread prompt, review, land).
fn starts_remote_turn(lifecycle: TurnLifecycle, event: &AgentEvent) -> bool {
    lifecycle == TurnLifecycle::Idle
        && matches!(
            event,
            AgentEvent::SessionStarted { .. }
                | AgentEvent::TextDelta { .. }
                | AgentEvent::ToolCallStarted { .. }
        )
}

fn billing_badge(info: &SessionInfo) -> BillingBadge {
    BillingBadge {
        billing: info.billing,
        api_key_source: info.api_key_source.clone(),
        agent_version: info.agent_version.clone(),
        permission_mode: info.permission_mode.clone(),
    }
}

fn decision_label(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "Allowed once",
        ApprovalDecision::AllowSession => "Allowed for this session",
        ApprovalDecision::Deny => "Denied",
    }
}

/// Compact one-line JSON of a tool input, capped for the approval card.
pub fn approval_preview(input: &serde_json::Value) -> String {
    let text = match input {
        serde_json::Value::Object(map) if map.is_empty() => String::new(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };
    const LIMIT: usize = 280;
    if text.chars().count() > LIMIT {
        let mut out: String = text.chars().take(LIMIT).collect();
        out.push('…');
        out
    } else {
        text
    }
}

fn render_approval(card: &ApprovalCard, p: &Palette, cx: &mut Context<ChatPanel>) -> AnyElement {
    use gpui_kit::component::Disableable as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};

    let summary = tool_summary(&card.tool_name, &card.input_json);
    let preview = approval_preview(&card.input_json);
    let open = card.state == ApprovalState::Pending;
    let header = h_flex()
        .w_full()
        .gap(theme::sp(2.))
        .items_center()
        .text_size(theme::text_sm())
        .child(
            Icon::new(if card.is_open() {
                IconName::ShieldAlert
            } else {
                IconName::ShieldCheck
            })
            .with_size(px(14.))
            .text_color(if card.is_open() {
                p.warning
            } else {
                p.text_faint
            }),
        )
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::MEDIUM)
                .text_color(p.text)
                .child(if card.is_open() {
                    format!("Allow {}?", card.tool_name)
                } else {
                    card.tool_name.clone()
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme::MONO_FONT)
                .text_size(theme::text_xs())
                .text_color(p.text_faint)
                .child(summary),
        );

    let footer: AnyElement = match &card.state {
        ApprovalState::Pending | ApprovalState::Sending(_) => {
            let sending = match card.state {
                ApprovalState::Sending(decision) => Some(decision),
                _ => None,
            };
            let button = |id: &'static str, label: &'static str, keys: &'static str, decision| {
                let request_id = card.request_id.clone();
                let mut button =
                    Button::new(SharedString::from(format!("{id}-{}", card.request_id)))
                        .small()
                        .label(label)
                        .tooltip(keys)
                        .loading(sending == Some(decision))
                        .disabled(!open)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.answer_approval(&request_id, decision, cx);
                        }));
                button = match decision {
                    ApprovalDecision::AllowOnce => button.primary(),
                    ApprovalDecision::AllowSession => button.ghost(),
                    ApprovalDecision::Deny => button.ghost(),
                };
                button
            };
            h_flex()
                .gap(theme::sp(1.5))
                .items_center()
                .child(button(
                    "approve-once",
                    "Allow once",
                    "\u{2318}Y",
                    ApprovalDecision::AllowOnce,
                ))
                .child(button(
                    "approve-session",
                    "Allow for session",
                    "\u{2318}\u{21e7}Y",
                    ApprovalDecision::AllowSession,
                ))
                .child(button(
                    "approve-deny",
                    "Deny",
                    "\u{2318}\u{21e7}N",
                    ApprovalDecision::Deny,
                ))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(
                            "\u{2318}Y allow · \u{2318}\u{21e7}Y session · \u{2318}\u{21e7}N deny",
                        ),
                )
                .into_any_element()
        }
        ApprovalState::Resolved { decision, reason } => {
            let color = match decision {
                ApprovalDecision::Deny => p.danger,
                _ => p.success,
            };
            h_flex()
                .gap(theme::sp(1.5))
                .items_center()
                .text_size(theme::text_xs())
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(
                    div()
                        .text_color(p.text_muted)
                        .child(decision_label(*decision)),
                )
                .when_some(reason.clone(), |this, reason| {
                    this.child(div().text_color(p.text_faint).child(format!("· {reason}")))
                })
                .into_any_element()
        }
        ApprovalState::Expired => div()
            .text_size(theme::text_xs())
            .text_color(p.text_faint)
            .child("No longer pending: the turn ended.")
            .into_any_element(),
    };

    div()
        .w_full()
        .pl(theme::sp(7.))
        .child(
            v_flex()
                .w_full()
                .px(theme::sp(3.))
                .py(theme::sp(2.5))
                .gap(theme::sp(2.))
                .rounded(theme::radius())
                .border_1()
                .border_color(if card.is_open() {
                    p.warning.opacity(0.5)
                } else {
                    p.border
                })
                .bg(if card.is_open() {
                    p.warning.opacity(0.06)
                } else {
                    p.elevated
                })
                .child(header)
                .when(card.is_open() && !preview.is_empty(), |this| {
                    this.child(
                        div()
                            .w_full()
                            .px(theme::sp(2.5))
                            .py(theme::sp(1.5))
                            .rounded(theme::radius_sm())
                            .bg(p.bg)
                            .border_1()
                            .border_color(p.border)
                            .font_family(theme::MONO_FONT)
                            .text_size(theme::text_xs())
                            .line_height(relative(1.5))
                            .text_color(p.text_muted)
                            .child(preview),
                    )
                })
                .child(footer),
        )
        .into_any_element()
}

fn take_rejected_prompt(event: &AgentEvent, pending: &mut Option<String>) -> Option<String> {
    if matches!(event, AgentEvent::Failed { .. }) {
        pending.take()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChatEntry, TurnLifecycle, accepts_agent_events, append_stored_assistant, can_submit,
        reconcile_stored_assistant, record_message_id, stop_transition,
        stored_assistant_transition, terminal_transition, unscoped_error_transition,
    };
    use std::collections::HashSet;
    use trama_domain::ids::{AgentSessionId, MessageId, ThreadId};
    use trama_domain::message::{ContentBlock, Message};
    use trama_protocol::ClientMessage;

    fn thread(id: &str) -> ThreadId {
        id.into()
    }

    #[test]
    fn failed_unacknowledged_prompt_is_restored_once_but_accepted_prompt_is_not_retried() {
        let failure = trama_domain::agent::AgentEvent::Failed {
            message: "thread needs migration".into(),
        };
        let mut pending = Some("keep my prompt".into());
        assert_eq!(
            super::take_rejected_prompt(&failure, &mut pending),
            Some("keep my prompt".into())
        );
        assert_eq!(super::take_rejected_prompt(&failure, &mut pending), None);
        let mut accepted = None; // Cleared by MessageAppended(User).
        assert_eq!(super::take_rejected_prompt(&failure, &mut accepted), None);
    }

    #[test]
    fn stopping_keeps_the_composer_busy_until_a_terminal_event() {
        let (state, interrupt) = stop_transition(TurnLifecycle::Running, Some(&thread("thread-1")));
        assert_eq!(
            (state, interrupt),
            (
                TurnLifecycle::Stopping,
                Some(ClientMessage::Interrupt {
                    thread_id: thread("thread-1")
                })
            )
        );
        assert_eq!(
            stop_transition(TurnLifecycle::Stopping, Some(&thread("thread-1"))),
            (TurnLifecycle::Stopping, None)
        );
        assert_eq!(
            stop_transition(TurnLifecycle::Running, None),
            (TurnLifecycle::Stopping, None)
        );
        assert!(accepts_agent_events(state));
        assert!(!can_submit(state));
        assert_ne!(state, TurnLifecycle::Idle);
    }

    #[test]
    fn stored_assistant_after_turn_completed_reconciles_only_current_stream() {
        let stored = Message::new_assistant(
            thread("thread-1"),
            AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "The answer".into(),
            }],
        );
        let mut entries = vec![
            ChatEntry::User {
                text: "prompt".into(),
                message_id: None,
            },
            ChatEntry::Assistant {
                markdown: "The answer".into(),
                streaming: true,
                message_id: None,
            },
        ];
        let mut seen = HashSet::new();
        assert_eq!(
            terminal_transition(true, false),
            TurnLifecycle::AwaitingAssistant
        );
        assert!(record_message_id(&mut seen, stored.id.clone()));
        assert!(reconcile_stored_assistant(
            &mut entries,
            TurnLifecycle::AwaitingAssistant,
            &stored,
        ));
        assert_eq!(entries.len(), 2);

        assert!(matches!(
            entries.as_slice(),
            [
                ChatEntry::User { .. },
                ChatEntry::Assistant {
                    markdown,
                    streaming: false,
                    message_id: Some(id),
                }
            ] if markdown == "The answer" && id == &stored.id
        ));
        assert!(!record_message_id(&mut seen, stored.id.clone()));
    }

    #[test]
    fn distinct_stored_message_ids_with_identical_text_are_preserved() {
        let first = Message::new_assistant(
            thread("thread-1"),
            AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "Same answer".into(),
            }],
        );
        let second = Message::new_assistant(
            thread("thread-1"),
            AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "Same answer".into(),
            }],
        );
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for message in [&first, &second, &first] {
            if record_message_id(&mut seen, message.id.clone()) {
                append_stored_assistant(&mut entries, TurnLifecycle::Idle, message);
            }
        }

        assert!(matches!(
            entries.as_slice(),
            [
                ChatEntry::Assistant { markdown: first_text, message_id: Some(first_id), .. },
                ChatEntry::Assistant { markdown: second_text, message_id: Some(second_id), .. }
            ] if first_text == "Same answer"
                && second_text == "Same answer"
                && first_id == &first.id
                && second_id == &second.id
        ));
    }

    #[test]
    fn late_events_are_ignored_until_reconciliation_allows_a_new_turn() {
        let stored = Message::new_assistant(
            thread("thread-1"),
            AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        );
        let mut entries = vec![ChatEntry::User {
            text: "first".into(),
            message_id: None,
        }];
        let mut lifecycle = terminal_transition(true, false);
        assert_eq!(lifecycle, TurnLifecycle::AwaitingAssistant);
        assert!(!can_submit(lifecycle));
        assert!(!accepts_agent_events(lifecycle));
        append_stored_assistant(&mut entries, lifecycle, &stored);
        lifecycle = stored_assistant_transition(lifecycle);
        assert_eq!(lifecycle, TurnLifecycle::Idle);
        assert!(can_submit(lifecycle));

        entries.push(ChatEntry::User {
            text: "second".into(),
            message_id: None,
        });
        lifecycle = TurnLifecycle::Running;
        assert!(accepts_agent_events(lifecycle));
        assert_eq!(
            record_message_id(&mut HashSet::new(), MessageId::from("same-id")),
            true,
        );
    }

    #[test]
    fn approval_cards_move_from_pending_to_sending_to_resolved() {
        use super::{
            ApprovalState, answer_approval, expire_open_approvals, push_approval,
            set_approval_state,
        };
        use trama_domain::agent::ApprovalDecision;

        let mut entries = vec![ChatEntry::User {
            text: "go".into(),
            message_id: None,
        }];
        let input = serde_json::json!({"command": "rm -rf target"});
        push_approval(&mut entries, "req-1".into(), "Bash".into(), input.clone());
        push_approval(&mut entries, "req-1".into(), "Bash".into(), input);
        assert_eq!(entries.len(), 2, "replayed requests are not duplicated");

        let thread_id = thread("t");
        let reply = answer_approval(
            &mut entries,
            &thread_id,
            "req-1",
            ApprovalDecision::AllowOnce,
        );
        assert_eq!(
            reply,
            Some(ClientMessage::RespondApproval {
                thread_id: thread_id.clone(),
                request_id: "req-1".into(),
                decision: ApprovalDecision::AllowOnce,
            })
        );
        assert_eq!(
            answer_approval(&mut entries, &thread_id, "req-1", ApprovalDecision::Deny),
            None,
            "a card in flight cannot be answered twice"
        );
        assert_eq!(
            answer_approval(&mut entries, &thread_id, "unknown", ApprovalDecision::Deny),
            None
        );

        assert!(set_approval_state(
            &mut entries,
            "req-1",
            ApprovalState::Resolved {
                decision: ApprovalDecision::AllowOnce,
                reason: None,
            },
        ));
        let ChatEntry::Approval(card) = &entries[1] else {
            panic!("approval card expected");
        };
        assert!(!card.is_open());

        push_approval(
            &mut entries,
            "req-2".into(),
            "Edit".into(),
            serde_json::json!({}),
        );
        expire_open_approvals(&mut entries);
        let ChatEntry::Approval(card) = &entries[2] else {
            panic!("approval card expected");
        };
        assert_eq!(card.state, ApprovalState::Expired);
    }

    #[test]
    fn idle_view_follows_turns_started_elsewhere() {
        use super::starts_remote_turn;
        use trama_domain::agent::AgentEvent;

        let text = AgentEvent::TextDelta { text: "hi".into() };
        let done = AgentEvent::TurnCompleted {
            num_turns: 1,
            cost_usd_micros: None,
        };
        assert!(starts_remote_turn(TurnLifecycle::Idle, &text));
        assert!(!starts_remote_turn(TurnLifecycle::Idle, &done));
        assert!(!starts_remote_turn(TurnLifecycle::Running, &text));
        assert!(
            !starts_remote_turn(TurnLifecycle::AwaitingAssistant, &text),
            "late events of a finished turn stay fenced"
        );
    }

    #[test]
    fn approval_preview_is_compact_and_capped() {
        use super::approval_preview;
        assert_eq!(approval_preview(&serde_json::json!({})), "");
        assert_eq!(
            approval_preview(&serde_json::json!({"command": "ls"})),
            r#"{"command":"ls"}"#
        );
        let long = approval_preview(&serde_json::json!({"content": "x".repeat(1000)}));
        assert_eq!(long.chars().count(), 281);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn tool_summary_prefers_the_meaningful_field_on_one_line() {
        use super::tool_summary;
        use serde_json::json;
        assert_eq!(
            tool_summary("Read", &json!({"file_path": "src/main.rs", "limit": 20})),
            "src/main.rs"
        );
        assert_eq!(
            tool_summary(
                "Bash",
                &json!({"command": "cargo test\necho done", "description": "Run tests"})
            ),
            "cargo test…"
        );
        assert_eq!(
            tool_summary("Grep", &json!({"pattern": "fn main", "path": "src"})),
            "fn main  in src"
        );
        assert_eq!(
            tool_summary("TodoWrite", &json!({"todos": [1, 2, 3]})),
            "3 items"
        );
        assert_eq!(tool_summary("Custom", &json!({"x": 1, "name": "hi"})), "hi");
        assert_eq!(tool_summary("Custom", &json!({})), "");
        let long = "a".repeat(200);
        assert_eq!(
            tool_summary("Bash", &json!({ "command": long }))
                .chars()
                .count(),
            121
        );
    }

    #[test]
    fn stored_prompt_with_delivered_comments_adopts_the_optimistic_entry() {
        use super::append_stored_user;
        use trama_domain::message::Role;

        let user = |text: &str| {
            let mut message = Message::new_assistant(
                thread("thread-1"),
                AgentSessionId::generate(),
                vec![ContentBlock::Text { text: text.into() }],
            );
            message.role = Role::User;
            message
        };
        let block = "<review-comments>\nintro\n\n[1] On x:\nComment: y\n</review-comments>";
        let mut entries = vec![ChatEntry::User {
            text: "continue".into(),
            message_id: None,
        }];
        let mut pending = Some("continue".to_string());
        let stored = user(&format!("{block}\n\ncontinue"));
        append_stored_user(&mut entries, &mut pending, &stored);
        assert_eq!(pending, None);
        assert!(matches!(
            entries.as_slice(),
            [ChatEntry::User { text, message_id: Some(id) }]
                if text.starts_with("<review-comments>") && id == &stored.id
        ));

        // Comments sent on their own arrive as a new entry.
        let only = user(block);
        append_stored_user(&mut entries, &mut pending, &only);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn unscoped_error_from_previous_thread_does_not_end_selected_turn() {
        let (lifecycle, banner) =
            unscoped_error_transition(TurnLifecycle::Running, "old thread error".into());
        assert_eq!(lifecycle, TurnLifecycle::Running);
        assert_eq!(banner, "old thread error");
        assert!(!can_submit(lifecycle));
    }
}
