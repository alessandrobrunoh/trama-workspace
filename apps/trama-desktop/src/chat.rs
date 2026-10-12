use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _,
    dock::{BasePanel, Panel, PanelEvent},
    h_flex,
    tag::Tag,
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
use crate::composer::Composer;
use crate::net::{DaemonClient, SendStatus};

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
            _enter: enter,
            _event_task: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.lifecycle != TurnLifecycle::Idle
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
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }));
        }
        self.entries.push(ChatEntry::User(prompt));
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
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => {
                    self.finish_tool(tool_use_id, *is_error);
                }
                ContentBlock::Thinking { .. } => {}
            }
        }
    }

    fn finish_tool(&mut self, tool_use_id: &str, is_error: bool) {
        if let Some(call) = self.entries.iter_mut().rev().find_map(|entry| match entry {
            ChatEntry::ToolCall {
                tool_use_id: id, ..
            } if id == tool_use_id => Some(entry),
            _ => None,
        }) {
            if let ChatEntry::ToolCall {
                finished,
                is_error: err,
                ..
            } = call
            {
                *finished = true;
                *err = is_error;
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
                });
            }
            AgentEvent::ToolCallFinished {
                tool_use_id,
                is_error,
                ..
            } => {
                self.finish_tool(&tool_use_id, is_error);
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
                self.finish_turn();
            }
            AgentEvent::Failed { message } => {
                self.entries.push(ChatEntry::Assistant {
                    markdown: format!("**Failed:** {message}"),
                    streaming: false,
                    message_id: None,
                });
                self.final_assistant_persisted = true;
                self.finish_turn();
            }
            AgentEvent::ReasoningDelta { .. }
            | AgentEvent::SessionInfo { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. } => {}
        }
    }

    fn finish_turn(&mut self) {
        self.lifecycle = terminal_transition(self.uses_daemon(), self.final_assistant_persisted);
        let waiting_for_assistant = self.lifecycle == TurnLifecycle::AwaitingAssistant;
        self.status = if waiting_for_assistant {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        };
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
        let running = self.is_running();
        let stoppable = matches!(
            self.lifecycle,
            TurnLifecycle::Running | TurnLifecycle::Stopping
        );
        self.composer.set_disabled(running, cx);
        let uses_daemon = self.uses_daemon();

        let messages = v_flex()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .px_10()
            .py_5()
            .gap_5()
            .overflow_y_scroll()
            .when(self.connection_error.is_some(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child(self.connection_error.clone().unwrap_or_default()),
                )
            })
            .when(!self.connected, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().warning)
                        .child("Offline demo · scripted responses, not a live agent."),
                )
            })
            .when(self.entries.is_empty(), |this| {
                this.child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            Icon::new(IconName::Bot)
                                .text_color(cx.theme().muted_foreground)
                                .with_size(px(28.)),
                        )
                        .child(if uses_daemon {
                            "This thread is ready."
                        } else if self.connected {
                            "Select a live thread to start."
                        } else {
                            "Connect the local daemon to open real workspaces and threads."
                        })
                        .child(div().text_xs().child(if uses_daemon {
                            "Messages are sent to the selected daemon thread."
                        } else {
                            "Demo replies are scripted and are not sent to an agent."
                        })),
                )
            })
            .children(
                self.entries
                    .iter()
                    .enumerate()
                    .map(|(ix, entry)| render_entry(ix, entry, cx).into_any_element()),
            );

        v_flex()
            .size_full()
            .bg(rgb(0x191b1d))
            .text_color(rgb(0xe3e5e8))
            .child(messages)
            .child(self.composer.render(
                running,
                stoppable,
                cx.listener(|this, _, window, cx| this.submit(window, cx)),
                cx.listener(|this, _, _, cx| this.stop(cx)),
                cx,
            ))
    }
}

fn render_entry(ix: usize, entry: &ChatEntry, cx: &mut Context<ChatPanel>) -> impl IntoElement {
    match entry {
        ChatEntry::User(text) => h_flex()
            .w_full()
            .child(
                div()
                    .max_w(relative(0.94))
                    .text_color(cx.theme().muted_foreground)
                    .child(text.clone()),
            )
            .into_any_element(),
        ChatEntry::Assistant {
            markdown,
            streaming,
            ..
        } => v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::Bot)
                            .text_color(cx.theme().muted_foreground)
                            .with_size(px(14.)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if *streaming {
                                "Assistant · streaming"
                            } else {
                                "Assistant"
                            }),
                    ),
            )
            .child(div().w_full().child(
                TextView::markdown(("assistant-md", ix), markdown.clone()).stream_fade(*streaming),
            ))
            .into_any_element(),
        ChatEntry::ToolCall {
            name,
            input_json,
            finished,
            is_error,
            expanded,
            ..
        } => {
            let status = if *is_error {
                "error"
            } else if *finished {
                "done"
            } else {
                "running"
            };
            let tag = if *is_error {
                Tag::danger()
            } else if *finished {
                Tag::success()
            } else {
                Tag::warning()
            }
            .small()
            .child(status);

            let body = if *expanded {
                Some(
                    div()
                        .mt_1()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().secondary)
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_xs()
                        .child(input_json.to_string()),
                )
            } else {
                None
            };

            v_flex()
                .w_full()
                .child(
                    h_flex()
                        .id(("tool-chip", ix))
                        .w_full()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().secondary.opacity(0.5)))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(ix, cx)))
                        .child(Icon::new(IconName::Wrench).with_size(px(12.)))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(name.clone()),
                        )
                        .child(tag)
                        .child(
                            div()
                                .ml_auto()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if *expanded { "hide" } else { "show" }),
                        ),
                )
                .children(body)
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
    fn unscoped_error_from_previous_thread_does_not_end_selected_turn() {
        let (lifecycle, banner) =
            unscoped_error_transition(TurnLifecycle::Running, "old thread error".into());
        assert_eq!(lifecycle, TurnLifecycle::Running);
        assert_eq!(banner, "old thread error");
        assert!(!can_submit(lifecycle));
    }
}
