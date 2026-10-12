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
use crate::net::{DaemonClient, SendStatus};
use crate::sidebar::ThreadStatus;
use crate::theme::{self, Palette};
use crate::tools::{self, Item, ToolKind, ToolTiming, ToolViewState};

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
}

/// Requests the chat panel makes of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// Select this thread (a subagent link was clicked).
    OpenThread(ThreadId),
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
struct SelectionHit {
    message_id: MessageId,
    /// Byte range of `Message::text()`.
    range: Range<usize>,
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
    Reply(CommentId),
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
    /// Threads the sidebar lists, for subagent links.
    known_threads: Vec<(ThreadId, String)>,
    /// Tool calls and thinking of turns seen this session, by thread and
    /// prompt. The daemon stores only prompts and final answers, so these
    /// are put back when a thread replays.
    turn_details: HashMap<ThreadId, TurnDetails>,
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
            known_threads: Vec::new(),
            turn_details: HashMap::new(),
            _enter: enter,
            _event_task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.lifecycle != TurnLifecycle::Idle
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
            // Re-subscribe replay will refill; drop anything streamed while offline.
            self.stash_turn_details();
            self.clear_transcript();
        }
        if !connected && was_connected {
            self.connection_error =
                Some("Daemon disconnected; waiting to reconcile this turn.".into());
        }
        if connected {
            self.connection_error = None;
        }
        cx.notify();
    }

    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.selected_thread == thread {
            return;
        }
        self.stash_turn_details();
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
        match message {
            DaemonMessage::MessageAppended { thread_id, message } => {
                if !self.thread_matches(&thread_id) {
                    return;
                }
                if !self.seen_message_ids.insert(message.id.clone()) {
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
        let Some(prompt) = self.composer.take_prompt(window, cx) else {
            return;
        };

        if let (true, Some(thread_id)) = (self.connected, self.selected_thread.clone()) {
            if self.client.send(ClientMessage::SendPrompt {
                thread_id: thread_id.clone(),
                text: prompt.clone(),
                mentions: Vec::new(),
                terminals: Vec::new(),
            }) == SendStatus::Rejected
            {
                self.restore_prompt(&thread_id, &prompt, window, cx);
                return;
            }
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
        match message.role {
            Role::User => {
                append_stored_user(&mut self.entries, &mut self.pending_prompt, &message);
                self.restore_turn_details(&message.id);
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
            | AgentEvent::CommandsReported { .. } => {}
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

    fn toggle_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(ChatEntry::ToolCall { expanded, .. }) = self.entries.get_mut(index) {
            *expanded = !*expanded;
            cx.notify();
        }
    }

    /// Remember the selected thread's tool calls and thinking per prompt.
    fn stash_turn_details(&mut self) {
        let Some(thread) = self.selected_thread.clone() else {
            return;
        };
        let stash = self.turn_details.entry(thread).or_default();
        for (prompt, entries) in turn_details(&self.entries) {
            stash.entries.insert(prompt, entries);
        }
        stash.timings.extend(
            self.tool_views
                .timings
                .iter()
                .map(|(id, timing)| (id.clone(), *timing)),
        );
    }

    /// Put a replayed prompt's stashed tool calls back under it.
    fn restore_turn_details(&mut self, prompt: &MessageId) {
        let Some(stash) = self
            .selected_thread
            .as_ref()
            .and_then(|thread| self.turn_details.get(thread))
        else {
            return;
        };
        let Some(entries) = stash.entries.get(prompt) else {
            return;
        };
        for entry in entries {
            if let ChatEntry::ToolCall { tool_use_id, .. } = entry
                && let Some(timing) = stash.timings.get(tool_use_id)
            {
                self.tool_views
                    .timings
                    .entry(tool_use_id.clone())
                    .or_insert(*timing);
            }
            if let Some(entry) = detail_copy(entry) {
                self.entries.push(entry);
            }
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
}

/// One thread's stashed turn details.
#[derive(Default)]
struct TurnDetails {
    entries: HashMap<MessageId, Vec<ChatEntry>>,
    timings: HashMap<String, ToolTiming>,
}

/// A finished copy of a tool call or thought; other entries are not kept.
fn detail_copy(entry: &ChatEntry) -> Option<ChatEntry> {
    match entry {
        ChatEntry::ToolCall {
            tool_use_id,
            name,
            input_json,
            finished,
            is_error,
            output,
            ..
        } => Some(ChatEntry::ToolCall {
            tool_use_id: tool_use_id.clone(),
            name: name.clone(),
            input_json: input_json.clone(),
            // A call cut off by the switch will not report back.
            finished: true,
            is_error: *is_error || (!*finished && output.is_none()),
            expanded: false,
            output: output.clone(),
        }),
        ChatEntry::Thinking { text } => Some(ChatEntry::Thinking { text: text.clone() }),
        _ => None,
    }
}

/// Tool calls and thinking after each stored prompt, by the prompt's id.
fn turn_details(entries: &[ChatEntry]) -> Vec<(MessageId, Vec<ChatEntry>)> {
    let mut out: Vec<(MessageId, Vec<ChatEntry>)> = Vec::new();
    let mut prompt: Option<MessageId> = None;
    for entry in entries {
        if let ChatEntry::User { message_id, .. } = entry {
            prompt = message_id.clone();
            continue;
        }
        let (Some(id), Some(copy)) = (&prompt, detail_copy(entry)) else {
            continue;
        };
        match out.last_mut() {
            Some((last, list)) if last == id => list.push(copy),
            _ => out.push((id.clone(), vec![copy])),
        }
    }
    out
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
    /// Comment on the selected transcript text (⌘⌥M and the floating button).
    pub fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.selection.clone() {
            Some(hit) => self.open_passage_draft(hit, None, window, cx),
            None if self.uses_daemon() => {
                self.connection_error = Some(
                    "Select text in a message, then type your comment (or press \u{2318}\u{2325}M)."
                        .into(),
                );
                cx.notify();
            }
            None => {}
        }
    }

    /// Type to comment: a printable key while transcript text is selected
    /// opens the composer under the passage with that text already in it.
    fn type_to_comment(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hit) = self.selection.clone() else {
            return;
        };
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
        let Some(text) = comments::typed_text(&event.keystroke) else {
            return;
        };
        cx.stop_propagation();
        window.prevent_default();
        self.open_passage_draft(hit, Some(text), window, cx);
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
            Some(hit) if hit.message_id == message_id => Some(hit.clone()),
            _ => self.whole_message_hit(&message_id),
        };
        if let Some(hit) = hit {
            self.open_passage_draft(hit, None, window, cx);
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
        Some(SelectionHit {
            message_id: message_id.clone(),
            range,
        })
    }

    fn open_passage_draft(
        &mut self,
        hit: SelectionHit,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.uses_daemon() {
            return;
        }
        if let Some(view) = self.message_views.get(&hit.message_id) {
            for segment in &view.segments {
                segment
                    .state
                    .update(cx, |state, cx| state.clear_selection(cx));
            }
        }
        self.selection = None;
        self.selection_point = None;
        self.connection_error = None;
        let target = CommentTarget::MessageRange {
            message_id: hit.message_id.clone(),
            start_byte: hit.range.start,
            end_byte: hit.range.end,
        };
        let draft = CommentDraft::new(
            target,
            None,
            initial,
            window,
            cx,
            |this: &mut Self, window, cx| this.submit_draft(window, cx),
        );
        self.draft = Some(ChatDraft {
            anchor: DraftAnchor::Passage {
                message_id: hit.message_id,
                range: hit.range,
            },
            draft,
        });
        cx.notify();
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
            self.draft = None;
            self.focus_composer(window, cx);
        }
        cx.notify();
    }

    fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn is_thread_open(&self, group: &CommentGroup) -> bool {
        self.thread_open
            .get(group.id())
            .copied()
            .unwrap_or_else(|| group.open_by_default())
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
        groups: &[CommentGroup],
        hits: &mut Vec<SelectionHit>,
        handlers: &CardHandlers,
        now: i64,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Vec<usize>) {
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
                comments::highlight_color(&groups[anchor.group].phase(), p)
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
            hits.push(SelectionHit {
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
                body = body.child(self.thread_element(&groups[group], handlers, now, p, cx));
            }
            if draft_cut.is_some_and(ends_here)
                && let Some(open) = &self.draft
            {
                body = body.child(self.render_open_draft("chat-draft", open, p, cx));
            }
        }
        (body.into_any_element(), threads)
    }

    /// An open thread card (with its reply composer) or a collapsed row.
    fn thread_element(
        &self,
        group: &CommentGroup,
        handlers: &CardHandlers,
        now: i64,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.is_thread_open(group) {
            return comments::render_collapsed(
                format!("chat-thread-row-{}", group.id()),
                group,
                None,
                handlers,
                p,
            );
        }
        let reply = self
            .draft
            .as_ref()
            .filter(|open| open.anchor == DraftAnchor::Reply(group.id().clone()))
            .map(|open| self.render_open_draft("chat-reply", open, p, cx));
        comments::render_card(
            format!("chat-thread-{}", group.id()),
            group,
            false,
            None,
            reply,
            handlers,
            now,
            p,
        )
    }

    /// Left gutter of a commentable message: a marker for its collapsed
    /// threads and, on hover, the add-comment button.
    fn gutter(
        &self,
        message_id: &MessageId,
        threads: &[usize],
        groups: &[CommentGroup],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collapsed: Vec<&CommentGroup> = threads
            .iter()
            .map(|&index| &groups[index])
            .filter(|group| !self.is_thread_open(group))
            .collect();
        let marker = (!collapsed.is_empty()).then(|| {
            let color = match comments::marker_phase(&collapsed) {
                Phase::Pending => p.accent,
                Phase::Delivered(_) => p.success,
                Phase::Resolved(_) => p.text_faint,
            };
            let tooltip: SharedString = comments::marker_summary(&collapsed).into();
            let ids: Vec<CommentId> = collapsed.iter().map(|group| group.id().clone()).collect();
            h_flex()
                .id(SharedString::from(format!("comment-marker-{message_id}")))
                .h(px(18.))
                .px(theme::sp(1.))
                .gap(theme::sp(0.5))
                .items_center()
                .rounded(theme::radius_sm())
                .bg(color.opacity(0.14))
                .text_size(theme::text_xs())
                .text_color(color)
                .cursor_pointer()
                .hover(|style| style.bg(color.opacity(0.24)))
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    for id in &ids {
                        this.thread_open.insert(id.clone(), true);
                    }
                    cx.notify();
                }))
                .child(Icon::new(IconName::MessageSquare).with_size(px(11.)))
                .child(collapsed.len().to_string())
        });
        let id = message_id.clone();
        let add = div()
            .id(SharedString::from(format!("comment-add-{message_id}")))
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::radius_sm())
            .cursor_pointer()
            .opacity(0.)
            .group_hover(SharedString::from(format!("msg-{message_id}")), |style| {
                style.opacity(1.)
            })
            .hover(|style| style.bg(p.hover))
            .tooltip(|window, cx| {
                Tooltip::new(
                    "Comment on the selection or the whole message. Tip: select text and start typing.",
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
        v_flex()
            .w(theme::sp(7.))
            .flex_shrink_0()
            .pt(px(2.))
            .gap(theme::sp(1.))
            .items_start()
            .children(marker)
            .when(self.uses_daemon(), |this| this.child(add))
            .into_any_element()
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
        let groups = if uses_daemon {
            self.comments.read(cx).groups()
        } else {
            Vec::new()
        };
        let handlers = self.card_handlers(cx);
        let now = comments::now_secs();

        // Each row is one child of the scroll container, so a message can be
        // scrolled to by its row index.
        let row = |content: AnyElement| {
            h_flex()
                .w_full()
                .flex_shrink_0()
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(theme::transcript_width())
                        .px(theme::sp(6.))
                        .child(content),
                )
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut row_of: HashMap<MessageId, usize> = HashMap::new();
        let mut hits: Vec<SelectionHit> = Vec::new();

        if let Some(error) = self.connection_error.clone() {
            rows.push(row(
                notice(&p, IconName::TriangleAlert, p.warning, error).into_any_element()
            ));
        }
        if !self.connected {
            rows.push(row(notice(
                &p,
                IconName::Info,
                p.text_muted,
                "Offline demo: replies are scripted and nothing is sent to an agent.".into(),
            )
            .into_any_element()));
        }

        // Moved out while rendering so rows can borrow `self` mutably.
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

        let items = tools::layout_items(&entries);
        let mut previous_was_agent = false;
        for item in &items {
            let ix = match item {
                Item::Single(ix) => *ix,
                Item::Group(range) => {
                    let open = self.tool_views.open_groups.contains(&range.start);
                    let content = tools::render_group(range.clone(), &entries, open, &p, cx);
                    rows.push(row(agent_block(&p, !previous_was_agent, content)));
                    previous_was_agent = true;
                    continue;
                }
                Item::Edits(range) => {
                    let content =
                        tools::render_edits(range.clone(), &entries, &self.tool_views, &p, cx);
                    rows.push(row(agent_block(&p, !previous_was_agent, content)));
                    previous_was_agent = true;
                    continue;
                }
            };
            let entry = &entries[ix];
            match entry {
                ChatEntry::User { text, message_id } => {
                    let mut block = v_flex().w_full().gap(theme::sp(1.5));
                    if ix > 0 {
                        block =
                            block.child(div().w_full().h(px(1.)).my(theme::sp(2.)).bg(p.border));
                    }
                    block = block.child(author_row(&p, Author::User));
                    match message_id {
                        Some(id) if uses_daemon => {
                            let split = comments::split_prompt_block(text);
                            let base = split.as_ref().map_or(0, |split| split.body_start);
                            if let Some(split) = &split {
                                block = block.child(
                                    div()
                                        .pl(theme::sp(7.))
                                        .child(self.sent_block(id, text, split, &p, cx)),
                                );
                            }
                            let body = &text[base..];
                            if !body.trim().is_empty() {
                                let (content, threads) = self.message_body(
                                    id, body, base, &groups, &mut hits, &handlers, now, &p, cx,
                                );
                                block = block.child(
                                    self.message_row(id, content, &threads, &groups, &p, cx),
                                );
                            }
                            row_of.insert(id.clone(), rows.len());
                        }
                        _ => {
                            block = block.child(
                                div()
                                    .pl(theme::sp(7.))
                                    .font_family(theme::PROSE_FONT)
                                    .text_size(theme::text_prose())
                                    .line_height(relative(1.55))
                                    .text_color(p.text)
                                    .child(comments::prompt_body(text).to_string()),
                            );
                        }
                    }
                    rows.push(row(block.into_any_element()));
                    previous_was_agent = false;
                }
                ChatEntry::Approval(card) => {
                    let content = render_approval(card, &p, cx);
                    rows.push(row(agent_block(&p, !previous_was_agent, content)));
                    previous_was_agent = true;
                }
                ChatEntry::Assistant {
                    markdown,
                    streaming: false,
                    message_id: Some(id),
                } if uses_daemon && self.stored_len.get(id) == Some(&markdown.len()) => {
                    let (content, threads) = self
                        .message_body(id, markdown, 0, &groups, &mut hits, &handlers, now, &p, cx);
                    let content = self.message_row(id, content, &threads, &groups, &p, cx);
                    row_of.insert(id.clone(), rows.len());
                    rows.push(row(agent_block(&p, !previous_was_agent, content)));
                    previous_was_agent = true;
                }
                ChatEntry::ToolCall {
                    name,
                    input_json,
                    output,
                    ..
                } => {
                    let content = match tools::tool_kind(name) {
                        ToolKind::Command => {
                            tools::render_command(ix, entry, &self.tool_views, &p, cx)
                        }
                        ToolKind::Subthread => {
                            let link = tools::subthread_link(
                                name,
                                input_json,
                                output.as_ref(),
                                &self.known_threads,
                            );
                            tools::render_subthread(ix, entry, link, &p, cx)
                        }
                        ToolKind::Edit | ToolKind::Minor(_) => {
                            render_entry(ix, entry, &p, cx).into_any_element()
                        }
                    };
                    rows.push(row(agent_block(&p, !previous_was_agent, content)));
                    previous_was_agent = true;
                }
                ChatEntry::Assistant { .. } | ChatEntry::Thinking { .. } => {
                    let content = render_entry(ix, entry, &p, cx).into_any_element();
                    rows.push(row(agent_block(&p, !previous_was_agent, content)));
                    previous_was_agent = true;
                }
            }
        }
        self.entries = entries;

        // A selection inside exactly one message can be commented on.
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

        if running {
            let label = match self.lifecycle {
                TurnLifecycle::Stopping => "Stopping…",
                TurnLifecycle::AwaitingAssistant => "Finishing…",
                _ if self.has_open_approval() => "Waiting for approval…",
                _ => "Working…",
            };
            let mut block = v_flex().w_full().gap(theme::sp(3.));
            if !previous_was_agent {
                block = block.child(author_row(&p, Author::Agent));
            }
            block = block.child(
                h_flex()
                    .pl(theme::sp(7.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Spinner::new().small().color(p.accent))
                    .child(label),
            );
            rows.push(row(block.into_any_element()));
        }

        let transcript = v_flex()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .pt(theme::sp(8.))
            .pb(theme::sp(6.))
            .gap(theme::sp(3.))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    this.selection_point = Some(event.position);
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                if this.selection_point.take().is_some() {
                    cx.notify();
                }
            }))
            .children(rows);

        let floating = self
            .selection
            .as_ref()
            .and(self.selection_point)
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

        let footer = ComposerFooter {
            model: self.model.clone(),
            status_label: self.status_pill(&p).0,
            status_color: self.status_pill(&p).1,
            usage: self.usage.clone(),
            cost_usd_micros: self.cost_usd_micros,
            turns: self.turns,
            billing: self.session_info.as_ref().map(billing_badge),
        };
        let strip = if show_composer && uses_daemon {
            self.pending_strip(&groups, &p, cx)
        } else {
            None
        };

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.type_to_comment(event, window, cx);
            }))
            .child(transcript)
            .children(floating)
            .when(show_composer, |this| {
                this.child(
                    h_flex().w_full().justify_center().child(
                        v_flex()
                            .w_full()
                            .max_w(theme::transcript_width())
                            .px(theme::sp(6.))
                            .pb(theme::sp(4.))
                            .children(strip)
                            .child(self.composer.render(
                                running,
                                stoppable,
                                footer,
                                cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                cx.listener(|this, _, _, cx| this.stop(cx)),
                                window,
                                cx,
                            )),
                    ),
                )
            })
    }
}

impl ChatPanel {
    /// Gutter and body of one commentable message.
    fn message_row(
        &self,
        message_id: &MessageId,
        content: AnyElement,
        threads: &[usize],
        groups: &[CommentGroup],
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .items_start()
            .group(SharedString::from(format!("msg-{message_id}")))
            .child(self.gutter(message_id, threads, groups, p, cx))
            .child(div().flex_1().min_w_0().child(content))
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum Author {
    User,
    Agent,
}

fn author_row(p: &Palette, author: Author) -> impl IntoElement {
    let (label, avatar) = match author {
        Author::User => (
            "You",
            div()
                .size(px(20.))
                .rounded_full()
                .bg(p.selected)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::User)
                        .with_size(px(12.))
                        .text_color(p.text_muted),
                ),
        ),
        Author::Agent => (
            "Claude",
            div()
                .size(px(20.))
                .rounded_full()
                .bg(p.accent_soft)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::Bot)
                        .with_size(px(12.))
                        .text_color(p.accent_text),
                ),
        ),
    };
    h_flex()
        .gap(theme::sp(2.))
        .items_center()
        .child(avatar)
        .child(
            div()
                .text_size(theme::text_sm())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(label),
        )
}

/// An agent row: the author line unless the previous row was the agent's.
fn agent_block(p: &Palette, show_author: bool, content: AnyElement) -> AnyElement {
    v_flex()
        .w_full()
        .gap(theme::sp(3.))
        .when(show_author, |this| this.child(author_row(p, Author::Agent)))
        .child(content)
        .into_any_element()
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

fn tool_icon(name: &str) -> IconName {
    match name {
        "Read" | "NotebookRead" => IconName::FileText,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => IconName::Pencil,
        "Bash" | "BashOutput" | "KillShell" => IconName::SquareTerminal,
        "Grep" | "Glob" | "LS" | "WebSearch" | "ToolSearch" => IconName::Search,
        "WebFetch" => IconName::Globe,
        "Task" | "Agent" => IconName::Bot,
        "TodoWrite" => IconName::ListChecks,
        _ => IconName::Wrench,
    }
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

fn pretty_json(value: &serde_json::Value) -> String {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    const LIMIT: usize = 4000;
    if text.len() > LIMIT {
        let mut end = LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
    } else {
        text
    }
}

fn render_entry(
    ix: usize,
    entry: &ChatEntry,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> impl IntoElement {
    match entry {
        ChatEntry::User { text, .. } => div().child(text.clone()).into_any_element(),
        ChatEntry::Approval(card) => render_approval(card, p, cx).into_any_element(),
        ChatEntry::Assistant {
            markdown,
            streaming,
            ..
        } => div()
            .w_full()
            .pl(theme::sp(7.))
            .font_family(theme::PROSE_FONT)
            .text_size(theme::text_prose())
            .line_height(relative(1.6))
            .text_color(p.text)
            .child(
                TextView::markdown(("assistant-md", ix), markdown.clone()).stream_fade(*streaming),
            )
            .into_any_element(),
        ChatEntry::ToolCall { .. } => div()
            .w_full()
            .pl(theme::sp(7.))
            .child(tool_row(ix, entry, p, cx))
            .into_any_element(),
        ChatEntry::Thinking { text } => div()
            .w_full()
            .pl(theme::sp(7.))
            .text_size(theme::text_sm())
            .line_height(relative(1.5))
            .italic()
            .text_color(p.text_faint)
            .child(text.trim().to_string())
            .into_any_element(),
    }
}

/// A compact, expandable row for one tool call (input and output JSON).
pub fn tool_row(
    ix: usize,
    entry: &ChatEntry,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let ChatEntry::ToolCall {
        name,
        input_json,
        finished,
        is_error,
        expanded,
        output,
        ..
    } = entry
    else {
        return div().into_any_element();
    };
    let summary = tool_summary(name, input_json);
    let status: AnyElement = if *is_error {
        Icon::new(IconName::CircleX)
            .with_size(px(13.))
            .text_color(p.danger)
            .into_any_element()
    } else if *finished {
        Icon::new(IconName::Check)
            .with_size(px(13.))
            .text_color(p.success)
            .into_any_element()
    } else {
        Spinner::new().xsmall().color(p.accent).into_any_element()
    };

    let body = expanded.then(|| {
        let block = |label: &'static str, text: String| {
            v_flex()
                .gap(theme::sp(1.))
                .child(
                    div()
                        .text_size(theme::text_xs())
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text_faint)
                        .child(label),
                )
                .child(
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
                        .child(text),
                )
        };
        v_flex()
            .w_full()
            .px(theme::sp(2.))
            .pb(theme::sp(2.))
            .gap(theme::sp(2.))
            .child(block("Input", pretty_json(input_json)))
            .children(output.as_ref().map(|output| {
                block(
                    if *is_error { "Error" } else { "Output" },
                    pretty_json(output),
                )
            }))
    });

    v_flex()
        .w_full()
        .rounded(theme::radius())
        .border_1()
        .border_color(if *expanded {
            p.border
        } else {
            p.bg.opacity(0.)
        })
        .when(*expanded, |this| this.bg(p.elevated))
        .child(
            h_flex()
                .id(("tool-row", ix))
                .w_full()
                .h(px(26.))
                .px(theme::sp(2.))
                .gap(theme::sp(2.))
                .items_center()
                .rounded(theme::radius())
                .cursor_pointer()
                .text_size(theme::text_sm())
                .hover(|style| style.bg(p.hover))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(ix, cx)))
                .child(
                    Icon::new(tool_icon(name))
                        .with_size(px(13.))
                        .text_color(p.text_muted),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child(name.clone()),
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
                )
                .child(status)
                .child(
                    Icon::new(if *expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .with_size(px(12.))
                    .text_color(p.text_faint),
                ),
        )
        .children(body)
        .into_any_element()
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
    fn turn_details_are_kept_per_stored_prompt() {
        use super::turn_details;
        let tool = |id: &str, finished: bool| ChatEntry::ToolCall {
            tool_use_id: id.into(),
            name: "Read".into(),
            input_json: serde_json::json!({}),
            finished,
            is_error: false,
            expanded: true,
            output: None,
        };
        let entries = vec![
            ChatEntry::User {
                text: "first".into(),
                message_id: Some(MessageId::from("m1")),
            },
            ChatEntry::Thinking { text: "hm".into() },
            tool("t1", true),
            ChatEntry::Assistant {
                markdown: "done".into(),
                streaming: false,
                message_id: None,
            },
            ChatEntry::User {
                text: "pending".into(),
                message_id: None,
            },
            tool("t2", true),
            ChatEntry::User {
                text: "second".into(),
                message_id: Some(MessageId::from("m2")),
            },
            tool("t3", false),
        ];
        let details = turn_details(&entries);
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].0, MessageId::from("m1"));
        assert_eq!(
            details[0].1.len(),
            2,
            "thinking and the tool call, not the text"
        );
        assert_eq!(details[1].0, MessageId::from("m2"));
        assert!(matches!(
            details[1].1.as_slice(),
            [ChatEntry::ToolCall {
                finished: true,
                is_error: true,
                expanded: false,
                ..
            }]
        ));
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
