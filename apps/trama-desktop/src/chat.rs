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
use trama_domain::agent::{AgentEvent, UsageReport};

use crate::agent::{ChatAgent, FakeAgent};
use crate::composer::Composer;

pub enum AgentStatus {
    Idle,
    Running,
}

pub enum ChatEntry {
    User(String),
    Assistant {
        markdown: String,
        streaming: bool,
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
    pub status: AgentStatus,
    pub usage: Option<UsageReport>,
    pub cost_usd_micros: Option<u64>,
    pub turns: Option<u32>,
    pub model: Option<String>,
    _enter: Subscription,
    _event_task: Option<Task<()>>,
}

impl ChatPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = Composer::new(window, cx);
        let enter = composer.subscribe_enter(window, cx, |this, window, cx| {
            this.submit(window, cx);
        });

        Self {
            entries: Vec::new(),
            composer,
            agent: Box::new(FakeAgent),
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
        matches!(self.status, AgentStatus::Running)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let Some(prompt) = self.composer.take_prompt(window, cx) else {
            return;
        };

        self.entries.push(ChatEntry::User(prompt.clone()));
        self.status = AgentStatus::Running;
        self.composer.set_disabled(true, cx);

        let rx = self.agent.send(prompt);
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
        cx.notify();
    }

    fn apply_event(&mut self, event: AgentEvent) {
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
                }) => markdown.push_str(&text),
                _ => self.entries.push(ChatEntry::Assistant {
                    markdown: text,
                    streaming: true,
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
                if let Some(call) = self.entries.iter_mut().rev().find_map(|entry| match entry {
                    ChatEntry::ToolCall {
                        tool_use_id: id, ..
                    } if *id == tool_use_id => Some(entry),
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
                });
                self.finish_turn();
            }
            AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ApprovalRequested { .. }
            | AgentEvent::SubagentStarted { .. }
            | AgentEvent::SubagentFinished { .. } => {}
        }
    }

    fn finish_turn(&mut self) {
        self.status = AgentStatus::Idle;
        // Composer is re-enabled on the next render tick via set_disabled.
    }

    fn toggle_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(ChatEntry::ToolCall { expanded, .. }) = self.entries.get_mut(index) {
            *expanded = !*expanded;
            cx.notify();
        }
    }
}

impl Render for ChatPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running();
        self.composer.set_disabled(running, cx);

        let messages = v_flex()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .p_4()
            .gap_3()
            .overflow_y_scroll()
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
                        .child("Send a message to start a turn.")
                        .child(
                            div()
                                .text_xs()
                                .child("The fake agent streams markdown, then an Edit tool call."),
                        ),
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
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(messages)
            .child(self.composer.render(
                running,
                cx.listener(|this, _, window, cx| this.submit(window, cx)),
                cx,
            ))
    }
}

fn render_entry(ix: usize, entry: &ChatEntry, cx: &mut Context<ChatPanel>) -> impl IntoElement {
    match entry {
        ChatEntry::User(text) => h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .max_w(relative(0.78))
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().primary)
                    .text_color(cx.theme().primary_foreground)
                    .child(text.clone()),
            )
            .into_any_element(),
        ChatEntry::Assistant {
            markdown,
            streaming,
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
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().secondary.opacity(0.35))
                    .child(
                        TextView::markdown(("assistant-md", ix), markdown.clone())
                            .stream_fade(*streaming),
                    ),
            )
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
