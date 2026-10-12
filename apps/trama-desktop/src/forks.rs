//! Fork from a message (Delta's "fork from this point"): the hover action
//! on stored messages, which messages can be forked at, and the small
//! dialog that asks for an optional title, first prompt and kind before
//! sending `ForkThread`.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Escape, Input, InputEvent, InputState, Textarea, TextareaState},
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::{Message, Role};
use trama_protocol::{ClientMessage, SubthreadKind};

use crate::chat::{ChatEntry, ChatPanel};
use crate::theme::{self, Palette};

// -- which messages can be forked at -----------------------------------------

/// Whether a fork at a message can work, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkState {
    Ready,
    Disabled(&'static str),
}

pub const NOT_STORED: &str = "This message is not stored yet.";
pub const INHERITED: &str =
    "This message belongs to the parent thread's history. Open the parent to fork from it.";
pub const NO_TURN_YET: &str = "No agent turn has completed at or before this message yet.";
pub const OLDER_HISTORY: &str = "Recorded before Trama kept fork points (Claude transcript position and file checkpoint), so this turn cannot be forked. Fork from a later message, or start a subthread from the current state.";

#[derive(Debug, Clone)]
struct Point {
    id: MessageId,
    own: bool,
    assistant: bool,
    fork_point: bool,
}

/// The selected thread's stored messages in order, as far as forking is
/// concerned. Mirrors the daemon: a fork at a message cuts at the last
/// assistant message with a transcript uuid at or before it, among the
/// thread's own messages (not the history it inherited from its parent).
#[derive(Debug, Clone, Default)]
pub struct ForkIndex {
    points: Vec<Point>,
}

impl ForkIndex {
    pub fn clear(&mut self) {
        self.points.clear();
    }

    pub fn note(&mut self, message: &Message, thread: &ThreadId) {
        if self.points.iter().any(|point| point.id == message.id) {
            return;
        }
        self.points.push(Point {
            id: message.id.clone(),
            own: &message.thread_id == thread,
            assistant: message.role == Role::Assistant,
            fork_point: message.role == Role::Assistant && message.claude_uuid.is_some(),
        });
    }

    pub fn state(&self, id: &MessageId) -> ForkState {
        let Some(index) = self.points.iter().position(|point| &point.id == id) else {
            return ForkState::Disabled(NOT_STORED);
        };
        if !self.points[index].own {
            return ForkState::Disabled(INHERITED);
        }
        let mine = self.points[..=index].iter().filter(|point| point.own);
        let (mut any_answer, mut any_point) = (false, false);
        for point in mine {
            any_answer |= point.assistant;
            any_point |= point.fork_point;
        }
        if any_point {
            ForkState::Ready
        } else if any_answer {
            ForkState::Disabled(OLDER_HISTORY)
        } else {
            ForkState::Disabled(NO_TURN_YET)
        }
    }
}

/// The message "Fork from here" uses for a turn: its stored answer (the
/// state right after the turn), else its stored prompt.
pub fn turn_fork_message(entries: &[ChatEntry]) -> Option<MessageId> {
    let answer = entries.iter().rev().find_map(|entry| match entry {
        ChatEntry::Assistant {
            message_id: Some(id),
            ..
        } => Some(id.clone()),
        _ => None,
    });
    answer.or_else(|| {
        entries.iter().find_map(|entry| match entry {
            ChatEntry::User {
                message_id: Some(id),
                ..
            } => Some(id.clone()),
            _ => None,
        })
    })
}

/// A short preview of the message a fork starts from.
pub fn preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(90).collect();
    if line.chars().count() > 90 {
        out.push('…');
    }
    out
}

/// What a fork request is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkTarget {
    pub thread_id: ThreadId,
    pub message_id: MessageId,
    /// Preview of the message ("Fork from: …").
    pub label: String,
}

/// The `ForkThread` request for a target and the dialog's fields.
pub fn fork_message(
    target: &ForkTarget,
    title: &str,
    prompt: &str,
    kind: SubthreadKind,
) -> ClientMessage {
    let title = title.trim();
    let prompt = prompt.trim();
    ClientMessage::ForkThread {
        thread_id: target.thread_id.clone(),
        at_message_id: Some(target.message_id.clone()),
        title: (!title.is_empty()).then(|| title.to_string()),
        initial_prompt: (!prompt.is_empty()).then(|| prompt.to_string()),
        kind,
    }
}

// -- the hover action ---------------------------------------------------------

/// The gutter's fork button: shown on hover like the comment button,
/// disabled (with the reason as its tooltip) when the message cannot be
/// forked at.
pub fn fork_button(
    message_id: &MessageId,
    state: ForkState,
    p: &Palette,
    cx: &mut Context<ChatPanel>,
) -> AnyElement {
    let id = message_id.clone();
    let ready = state == ForkState::Ready;
    let tooltip: SharedString = match state {
        ForkState::Ready => "Fork from here: a new thread with the conversation and files as they were after this turn".into(),
        ForkState::Disabled(reason) => format!("Cannot fork here. {reason}").into(),
    };
    div()
        .id(SharedString::from(format!("fork-{message_id}")))
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius_sm())
        .opacity(0.)
        .group_hover(SharedString::from(format!("msg-{message_id}")), |style| {
            style.opacity(1.)
        })
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .when(ready, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(p.hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.request_fork(id.clone(), cx);
                }))
        })
        .child(
            Icon::new(IconName::GitFork)
                .with_size(px(13.))
                .text_color(if ready {
                    p.text_faint
                } else {
                    p.text_faint.opacity(0.45)
                }),
        )
        .into_any_element()
}

// -- the dialog ---------------------------------------------------------------

pub enum ForkEvent {
    Submit(ClientMessage),
    Cancel,
}

pub struct ForkView {
    target: Option<ForkTarget>,
    title: Entity<InputState>,
    prompt: Entity<TextareaState>,
    kind: SubthreadKind,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ForkEvent> for ForkView {}

impl ForkView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Title (optional)"));
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("First prompt for the fork (optional)")
                .rows(3)
                .submit_on_enter(true)
        });
        let subscriptions = vec![
            cx.subscribe_in(&title, window, |this, _, event, _, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(cx);
                }
            }),
            cx.subscribe_in(&prompt, window, |this, _, event, _, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(cx);
                }
            }),
        ];
        Self {
            target: None,
            title,
            prompt,
            kind: SubthreadKind::Plain,
            _subscriptions: subscriptions,
        }
    }

    /// Clear the fields for a new target and focus the title.
    pub fn reset(&mut self, target: ForkTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.target = Some(target);
        self.kind = SubthreadKind::Plain;
        self.title
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.prompt
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.title.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let title = self.title.read(cx).value().to_string();
        let prompt = self.prompt.read(cx).value().to_string();
        cx.emit(ForkEvent::Submit(fork_message(
            &target, &title, &prompt, self.kind,
        )));
    }

    fn kind_button(
        &self,
        kind: SubthreadKind,
        label: &'static str,
        icon: IconName,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(SharedString::from(format!("fork-kind-{label}")))
            .small()
            .ghost()
            .icon(icon)
            .label(label)
            .tooltip(tooltip)
            .toggled(self.kind == kind)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.kind = kind;
                cx.notify();
            }))
    }
}

impl Render for ForkView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let label = self
            .target
            .as_ref()
            .map(|target| target.label.clone())
            .unwrap_or_default();
        v_flex()
            .id("fork-dialog")
            .capture_action(cx.listener(|_, _: &Escape, _, cx| cx.emit(ForkEvent::Cancel)))
            .gap(theme::sp(3.))
            .child(
                h_flex()
                    .min_w_0()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(
                        Icon::new(IconName::GitFork)
                            .with_size(px(13.))
                            .text_color(p.accent_text),
                    )
                    .child(div().flex_shrink_0().child("From"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(p.text)
                            .child(if label.is_empty() {
                                "this message".to_string()
                            } else {
                                format!("\u{201c}{label}\u{201d}")
                            }),
                    ),
            )
            .child(Input::new(&self.title).id("fork-title"))
            .child(
                div()
                    .w_full()
                    .px(theme::sp(2.5))
                    .py(theme::sp(2.))
                    .rounded(theme::radius())
                    .border_1()
                    .border_color(p.border)
                    .bg(p.elevated)
                    .text_size(theme::text_ui())
                    .child(Textarea::new(&self.prompt).appearance(false).h(px(64.))),
            )
            .child(
                h_flex()
                    .gap(theme::sp(1.))
                    .items_center()
                    .child(self.kind_button(
                        SubthreadKind::Plain,
                        "Plain",
                        IconName::Split,
                        "A subthread that waits for your prompt (or runs the first prompt above)",
                        cx,
                    ))
                    .child(self.kind_button(
                        SubthreadKind::Review,
                        "Review",
                        IconName::MessageSquareDiff,
                        "A review subthread that starts reviewing the forked state right away",
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child("Conversation and files as of this turn \u{00b7} Enter to fork \u{00b7} Esc to cancel"),
                    )
                    .child(
                        Button::new("fork-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ForkEvent::Cancel))),
                    )
                    .child(
                        Button::new("fork-submit")
                            .primary()
                            .icon(IconName::GitFork)
                            .label("Fork")
                            .disabled(self.target.is_none())
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;
    use trama_domain::ids::AgentSessionId;
    use trama_domain::message::ContentBlock;

    fn user(thread: &str, text: &str) -> Message {
        Message::new_user(ThreadId::from(thread), "me", text)
    }

    fn answer(thread: &str, uuid: Option<&str>) -> Message {
        let mut message = Message::new_assistant(
            ThreadId::from(thread),
            AgentSessionId::generate(),
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        );
        message.claude_uuid = uuid.map(str::to_string);
        message
    }

    #[test]
    fn fork_action_is_enabled_only_where_the_daemon_can_fork() {
        let thread = ThreadId::from("t");
        let inherited = user("parent", "from the parent");
        let old_prompt = user("t", "old");
        let old_answer = answer("t", None);
        let prompt = user("t", "new");
        let new_answer = answer("t", Some("uuid-1"));
        let next_prompt = user("t", "next");
        let mut index = ForkIndex::default();
        for message in [
            &inherited,
            &old_prompt,
            &old_answer,
            &prompt,
            &new_answer,
            &next_prompt,
        ] {
            index.note(message, &thread);
        }
        assert_eq!(index.state(&inherited.id), ForkState::Disabled(INHERITED));
        assert_eq!(
            index.state(&old_prompt.id),
            ForkState::Disabled(NO_TURN_YET)
        );
        assert_eq!(
            index.state(&old_answer.id),
            ForkState::Disabled(OLDER_HISTORY)
        );
        assert_eq!(
            index.state(&prompt.id),
            ForkState::Disabled(OLDER_HISTORY),
            "a prompt forks at the turn before it, which predates fork points"
        );
        assert_eq!(index.state(&new_answer.id), ForkState::Ready);
        assert_eq!(index.state(&next_prompt.id), ForkState::Ready);
        assert_eq!(
            index.state(&MessageId::from("unknown")),
            ForkState::Disabled(NOT_STORED)
        );
        index.clear();
        assert_eq!(index.state(&new_answer.id), ForkState::Disabled(NOT_STORED));
    }

    #[test]
    fn the_turn_forks_at_its_answer_else_its_prompt() {
        let entries = vec![
            ChatEntry::User {
                text: "q".into(),
                message_id: Some(MessageId::from("p1")),
            },
            ChatEntry::Thinking { text: "t".into() },
            ChatEntry::Assistant {
                markdown: "a".into(),
                streaming: false,
                message_id: Some(MessageId::from("a1")),
            },
        ];
        assert_eq!(turn_fork_message(&entries), Some(MessageId::from("a1")));
        assert_eq!(
            turn_fork_message(&entries[..2]),
            Some(MessageId::from("p1"))
        );
        assert_eq!(turn_fork_message(&entries[1..2]), None);
    }

    #[test]
    fn the_request_carries_only_what_was_filled_in() {
        let target = ForkTarget {
            thread_id: ThreadId::from("t"),
            message_id: MessageId::from("a1"),
            label: preview("\n  Try another approach\nmore"),
        };
        assert_eq!(target.label, "Try another approach");
        assert_eq!(
            fork_message(&target, "  ", "\n", SubthreadKind::Plain),
            ClientMessage::ForkThread {
                thread_id: ThreadId::from("t"),
                at_message_id: Some(MessageId::from("a1")),
                title: None,
                initial_prompt: None,
                kind: SubthreadKind::Plain,
            }
        );
        assert_eq!(
            fork_message(&target, " B ", "go", SubthreadKind::Review),
            ClientMessage::ForkThread {
                thread_id: ThreadId::from("t"),
                at_message_id: Some(MessageId::from("a1")),
                title: Some("B".into()),
                initial_prompt: Some("go".into()),
                kind: SubthreadKind::Review,
            }
        );
    }
}
