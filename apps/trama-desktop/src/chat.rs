use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    dock::{BasePanel, Panel, PanelEvent},
    h_flex,
    kbd::Kbd,
    spinner::Spinner,
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use trama_domain::agent::{AgentEvent, UsageReport};
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::{ContentBlock, Message, Role};
use trama_protocol::{ClientMessage, DaemonMessage};

use crate::agent::{ChatAgent, FakeAgent};
use crate::composer::{Composer, ComposerFooter};
use crate::net::{DaemonClient, SendStatus};
use crate::sidebar::ThreadStatus;
use crate::theme::{self, Palette};

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

pub enum ChatEntry {
    User(String),
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
}

pub struct ChatPanel {
    entries: Vec<ChatEntry>,
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
    /// Tool name of an approval the agent is waiting on. Approvals cannot be
    /// answered from this build yet; the thread is flagged instead.
    pending_approval: Option<String>,
    /// Bumped whenever a turn reaches a terminal event; observers use it to
    /// refresh views that depend on the checkout (the Changes drawer).
    pub completed_turns: u64,
    last_failed: bool,
    scroll: ScrollHandle,
    _enter: Subscription,
    _event_task: Option<Task<()>>,
}

impl ChatPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, client: DaemonClient) -> Self {
        let composer = Composer::new(window, cx);
        let enter = composer.subscribe_enter(window, cx, |this, window, cx| {
            this.submit(window, cx);
        });

        Self {
            entries: Vec::new(),
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
            pending_approval: None,
            completed_turns: 0,
            last_failed: false,
            scroll: ScrollHandle::new(),
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

    /// Dot shown next to the selected thread in the sidebar.
    pub fn thread_status(&self) -> ThreadStatus {
        if self.pending_approval.is_some() {
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
        if self.pending_approval.is_some() {
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
        self.connection_error = None;
        cx.notify();
    }

    fn clear_transcript(&mut self) {
        self.entries.clear();
        self.seen_message_ids.clear();
        self.lifecycle = TurnLifecycle::Idle;
        self.final_assistant_persisted = false;
        self.pending_prompt = None;
        self.status = AgentStatus::Idle;
        self.usage = None;
        self.cost_usd_micros = None;
        self.turns = None;
        self.model = None;
        self.pending_approval = None;
        self.last_failed = false;
        self._event_task = None;
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
            if let Some(ChatEntry::User(text)) = self.entries.last()
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
                    if matches!(self.entries.last(), Some(ChatEntry::User(text)) if text == &prompt)
                    {
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
        self.entries.push(ChatEntry::User(prompt));
        self.last_failed = false;
        self.pending_approval = None;
        self.scroll.scroll_to_bottom();
        self.connection_error = None;
        self.lifecycle = TurnLifecycle::Running;
        self.final_assistant_persisted = false;
        self.status = AgentStatus::Running;
        self.composer.set_disabled(true, cx);
        cx.notify();
    }

    fn append_stored_message(&mut self, message: Message) {
        match message.role {
            Role::User => {
                let text = message.text();
                if self.pending_prompt.as_deref() == Some(text.as_str()) {
                    self.pending_prompt = None;
                }
                if let Some(ChatEntry::User(existing)) = self.entries.last() {
                    if existing == &text {
                        return;
                    }
                }
                self.entries.push(ChatEntry::User(text));
            }
            Role::Assistant => {
                append_stored_assistant(&mut self.entries, self.lifecycle, &message);
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
                ContentBlock::Thinking { .. } => {}
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
                self.pending_approval = None;
                self.finish_tool(&tool_use_id, is_error, Some(output_json));
            }
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
                self.pending_approval = None;
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
                self.pending_approval = None;
                self.finish_turn();
            }
            // Integration hook: render an approval prompt here once the
            // composer can answer with `ClientMessage::RespondApproval`.
            AgentEvent::ApprovalRequested { tool_name, .. } => {
                self.pending_approval = Some(tool_name);
            }
            AgentEvent::ReasoningDelta { .. }
            | AgentEvent::SessionInfo { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. } => {}
        }
    }

    fn finish_turn(&mut self) {
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

        let mut column = v_flex()
            .w_full()
            .max_w(theme::transcript_width())
            .px(theme::sp(6.))
            .pt(theme::sp(8.))
            .pb(theme::sp(6.))
            .gap(theme::sp(3.));

        if let Some(error) = self.connection_error.clone() {
            column = column.child(notice(&p, IconName::TriangleAlert, p.warning, error));
        }
        if !self.connected {
            column = column.child(notice(
                &p,
                IconName::Info,
                p.text_muted,
                "Offline demo: replies are scripted and nothing is sent to an agent.".into(),
            ));
        }
        if let Some(tool) = self.pending_approval.clone() {
            column = column.child(notice(
                &p,
                IconName::CircleAlert,
                p.warning,
                format!(
                    "The agent is waiting for approval to use {tool}. Answering approvals from the app is not available yet."
                ),
            ));
        }

        if self.entries.is_empty() {
            column = column.child(if uses_daemon || !self.connected {
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
            });
        }

        let mut previous_was_agent = false;
        for (ix, entry) in self.entries.iter().enumerate() {
            match entry {
                ChatEntry::User(text) => {
                    if ix > 0 {
                        column =
                            column.child(div().w_full().h(px(1.)).my(theme::sp(2.)).bg(p.border));
                    }
                    column = column.child(
                        v_flex()
                            .w_full()
                            .gap(theme::sp(1.5))
                            .child(author_row(&p, Author::User))
                            .child(
                                div()
                                    .pl(theme::sp(7.))
                                    .font_family(theme::PROSE_FONT)
                                    .text_size(theme::text_prose())
                                    .line_height(relative(1.55))
                                    .text_color(p.text)
                                    .child(text.clone()),
                            ),
                    );
                    previous_was_agent = false;
                }
                ChatEntry::Assistant { .. } | ChatEntry::ToolCall { .. } => {
                    if !previous_was_agent {
                        column = column.child(author_row(&p, Author::Agent));
                    }
                    column = column.child(render_entry(ix, entry, &p, cx).into_any_element());
                    previous_was_agent = true;
                }
            }
        }

        if running {
            let label = match self.lifecycle {
                TurnLifecycle::Stopping => "Stopping…",
                TurnLifecycle::AwaitingAssistant => "Finishing…",
                _ if self.pending_approval.is_some() => "Waiting for approval…",
                _ => "Working…",
            };
            if !previous_was_agent {
                column = column.child(author_row(&p, Author::Agent));
            }
            column = column.child(
                h_flex()
                    .pl(theme::sp(7.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Spinner::new().small().color(p.accent))
                    .child(label),
            );
        }

        let transcript = div()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(h_flex().w_full().justify_center().child(column));

        let footer = ComposerFooter {
            model: self.model.clone(),
            status_label: self.status_pill(&p).0,
            status_color: self.status_pill(&p).1,
            usage: self.usage.clone(),
            cost_usd_micros: self.cost_usd_micros,
            turns: self.turns,
        };

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .child(transcript)
            .when(show_composer, |this| {
                this.child(
                    h_flex().w_full().justify_center().child(
                        v_flex()
                            .w_full()
                            .max_w(theme::transcript_width())
                            .px(theme::sp(6.))
                            .pb(theme::sp(4.))
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
        ChatEntry::User(text) => div().child(text.clone()).into_any_element(),
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
        ChatEntry::ToolCall {
            name,
            input_json,
            finished,
            is_error,
            expanded,
            output,
            ..
        } => {
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

            div()
                .w_full()
                .pl(theme::sp(7.))
                .child(
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
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_tool(ix, cx)),
                                )
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
                        .children(body),
                )
                .into_any_element()
        }
    }
}

impl EventEmitter<PanelEvent> for ChatPanel {}

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
        .rposition(|entry| matches!(entry, ChatEntry::User(_)))
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
            ChatEntry::User("prompt".into()),
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
                ChatEntry::User(_),
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
        let mut entries = vec![ChatEntry::User("first".into())];
        let mut lifecycle = terminal_transition(true, false);
        assert_eq!(lifecycle, TurnLifecycle::AwaitingAssistant);
        assert!(!can_submit(lifecycle));
        assert!(!accepts_agent_events(lifecycle));
        append_stored_assistant(&mut entries, lifecycle, &stored);
        lifecycle = stored_assistant_transition(lifecycle);
        assert_eq!(lifecycle, TurnLifecycle::Idle);
        assert!(can_submit(lifecycle));

        entries.push(ChatEntry::User("second".into()));
        lifecycle = TurnLifecycle::Running;
        assert!(accepts_agent_events(lifecycle));
        assert_eq!(
            record_message_id(&mut HashSet::new(), MessageId::from("same-id")),
            true,
        );
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
    fn unscoped_error_from_previous_thread_does_not_end_selected_turn() {
        let (lifecycle, banner) =
            unscoped_error_transition(TurnLifecycle::Running, "old thread error".into());
        assert_eq!(lifecycle, TurnLifecycle::Running);
        assert_eq!(banner, "old thread error");
        assert!(!can_submit(lifecycle));
    }
}
