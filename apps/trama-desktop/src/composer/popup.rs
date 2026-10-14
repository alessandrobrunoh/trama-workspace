//! The composer's caret popups: `@` mentions (`ListMentionables`, debounced
//! as the query grows), `/` skills and commands (`ListSkills`, cached per
//! thread), and the `!` run-in-terminal hint.
//!
//! The view observes the composer's textarea and re-detects the trigger on
//! every change. The chat panel forwards Up/Down/Enter/Tab/Escape here while
//! a popup is open (see [`ComposerPopup::is_open`]).

use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _, h_flex,
    input::{InputEvent, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_protocol::{ClientMessage, DaemonMessage, Mentionable, SkillInfo};

use crate::app::theme::{self, Palette};
use crate::conversation::mentions;
use crate::conversation::slash;
use crate::conversation::trigger::{self, Trigger};
use crate::system::net::{DaemonClient, SendStatus};

const MENTION_DEBOUNCE: Duration = Duration::from_millis(90);
const MENTION_LIMIT: u32 = 30;
const POPUP_WIDTH: f32 = 400.;
const ROW_HEIGHT: f32 = 30.;
const MAX_VISIBLE_ROWS: usize = 8;

pub struct ComposerPopup {
    client: DaemonClient,
    input: Entity<TextareaState>,
    thread: Option<ThreadId>,
    trigger: Option<Trigger>,
    caret: usize,
    /// Escape closed this trigger; it stays closed until the trigger moves.
    dismissed: Option<Trigger>,
    selected: usize,
    mention_items: Vec<Mentionable>,
    /// Query of the newest `ListMentionables` sent; older replies are dropped.
    pending_query: Option<String>,
    debounce: Option<Task<()>>,
    skills: Vec<SkillInfo>,
    skills_thread: Option<ThreadId>,
    skills_requested: bool,
    scroll: ScrollHandle,
    _observe: Subscription,
    _focus: Subscription,
}

/// Same popup anchor: same kind at the same offset.
fn same_anchor(a: &Trigger, b: &Trigger) -> bool {
    match (a, b) {
        (Trigger::Mention { start: x, .. }, Trigger::Mention { start: y, .. })
        | (Trigger::Slash { start: x, .. }, Trigger::Slash { start: y, .. }) => x == y,
        (Trigger::Bang { .. }, Trigger::Bang { .. }) => true,
        _ => false,
    }
}

impl ComposerPopup {
    pub fn new(client: DaemonClient, input: Entity<TextareaState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&input, |this, _, cx| this.recompute(cx));
        let focus = cx.subscribe(&input, |this, _, event: &InputEvent, _| {
            if matches!(event, InputEvent::Focus) {
                this.ensure_skills();
            }
        });
        Self {
            client,
            input,
            thread: None,
            trigger: None,
            caret: 0,
            dismissed: None,
            selected: 0,
            mention_items: Vec::new(),
            pending_query: None,
            debounce: None,
            skills: Vec::new(),
            skills_thread: None,
            skills_requested: false,
            scroll: ScrollHandle::new(),
            _observe: observe,
            _focus: focus,
        }
    }

    pub fn set_thread(&mut self, thread: Option<ThreadId>, cx: &mut Context<Self>) {
        if self.thread == thread {
            return;
        }
        self.thread = thread;
        self.mention_items.clear();
        self.pending_query = None;
        self.debounce = None;
        self.skills_requested = false;
        self.dismissed = None;
        cx.notify();
    }

    /// Forget cached replies after a reconnect.
    pub fn reconnected(&mut self) {
        self.skills_requested = false;
        self.skills_thread = None;
        self.pending_query = None;
    }

    /// The agent reported its commands again: refetch the list.
    pub fn refresh_skills(&mut self) {
        self.skills_requested = false;
        self.skills_thread = None;
        self.ensure_skills();
    }

    fn ensure_skills(&mut self) {
        let Some(thread) = self.thread.clone() else {
            return;
        };
        if self.skills_thread.as_ref() == Some(&thread) || self.skills_requested {
            return;
        }
        self.skills_requested = self
            .client
            .send(ClientMessage::ListSkills { thread_id: thread })
            == SendStatus::Enqueued;
    }

    pub fn apply(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        match message {
            DaemonMessage::Mentionables {
                thread_id,
                query,
                items,
            } => {
                if self.thread.as_ref() == Some(thread_id)
                    && self.pending_query.as_deref() == Some(query.as_str())
                {
                    self.mention_items = items.clone();
                    self.selected = self.selected.min(items.len().saturating_sub(1));
                    cx.notify();
                }
                true
            }
            DaemonMessage::Skills { thread_id, items } => {
                if self.thread.as_ref() == Some(thread_id) {
                    self.skills = items.clone();
                    self.skills_thread = Some(thread_id.clone());
                    self.skills_requested = false;
                    cx.notify();
                }
                true
            }
            _ => false,
        }
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        let (trigger, caret) = {
            let input = self.input.read(cx);
            let text = input.value();
            let caret = input.cursor();
            let tokens: Vec<_> = input.tokens().iter().map(|span| span.range()).collect();
            (trigger::detect(&text, caret, &tokens), caret)
        };
        self.caret = caret;
        if trigger == self.trigger {
            return;
        }
        if let Some(dismissed) = &self.dismissed
            && !trigger.as_ref().is_some_and(|t| same_anchor(t, dismissed))
        {
            self.dismissed = None;
        }
        let query_changed = match (&self.trigger, &trigger) {
            (Some(Trigger::Mention { query: a, .. }), Some(Trigger::Mention { query: b, .. }))
            | (Some(Trigger::Slash { query: a, .. }), Some(Trigger::Slash { query: b, .. })) => {
                a != b
            }
            _ => true,
        };
        if query_changed {
            self.selected = 0;
            self.scroll.scroll_to_item(0);
        }
        match &trigger {
            Some(Trigger::Mention { query, .. }) => self.schedule_mentions(query.clone(), cx),
            Some(Trigger::Slash { .. }) => self.ensure_skills(),
            _ => self.debounce = None,
        }
        if !matches!(trigger, Some(Trigger::Mention { .. })) {
            self.mention_items.clear();
            self.pending_query = None;
        }
        self.trigger = trigger;
        cx.notify();
    }

    fn schedule_mentions(&mut self, query: String, cx: &mut Context<Self>) {
        if self.pending_query.as_deref() == Some(query.as_str()) {
            return;
        }
        // The first keystroke after `@` asks right away; later ones wait.
        let delay = if self.pending_query.is_none() {
            Duration::ZERO
        } else {
            MENTION_DEBOUNCE
        };
        self.debounce = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let _ = this.update(cx, |this, _| this.fetch_mentions(query));
        }));
    }

    fn fetch_mentions(&mut self, query: String) {
        let Some(thread_id) = self.thread.clone() else {
            return;
        };
        let sent = self.client.send(ClientMessage::ListMentionables {
            thread_id,
            query: query.clone(),
            limit: Some(MENTION_LIMIT),
        });
        if sent == SendStatus::Enqueued {
            self.pending_query = Some(query);
        }
    }

    fn visible_trigger(&self) -> Option<&Trigger> {
        let trigger = self.trigger.as_ref()?;
        match &self.dismissed {
            Some(dismissed) if same_anchor(dismissed, trigger) => None,
            _ => Some(trigger),
        }
    }

    fn slash_rows(&self) -> Vec<usize> {
        match self.visible_trigger() {
            Some(Trigger::Slash { query, .. }) => slash::filter(&self.skills, query),
            _ => Vec::new(),
        }
    }

    fn row_count(&self) -> usize {
        match self.visible_trigger() {
            Some(Trigger::Mention { .. }) => self.mention_items.len(),
            Some(Trigger::Slash { .. }) => self.slash_rows().len(),
            _ => 0,
        }
    }

    /// A list popup is showing (keys go to it, not to the textarea).
    pub fn is_open(&self) -> bool {
        self.row_count() > 0
    }

    /// `!command` mode: the command to run (empty: an interactive shell).
    pub fn bang_command(&self) -> Option<String> {
        match self.visible_trigger() {
            Some(Trigger::Bang { command }) => Some(command.clone()),
            _ => None,
        }
    }

    /// Escape: close the popup (or leave `!` mode). `false` when nothing
    /// was showing.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(trigger) = self.visible_trigger().cloned() else {
            return false;
        };
        if !self.is_open() && !matches!(trigger, Trigger::Bang { .. }) {
            return false;
        }
        self.dismissed = Some(trigger);
        cx.notify();
        true
    }

    pub fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }

    /// Insert the selected row. `false` when no popup is open.
    pub fn accept(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.accept_row(self.selected, window, cx)
    }

    fn accept_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(trigger) = self.visible_trigger().cloned() else {
            return false;
        };
        let Some(range) = trigger.replace_range(self.caret) else {
            return false;
        };
        match trigger {
            Trigger::Mention { .. } => {
                let Some(item) = self.mention_items.get(row).cloned() else {
                    return false;
                };
                self.input.update(cx, |input, cx| {
                    let token = mentions::token_for(&item);
                    if input
                        .replace_range_with_token(range.clone(), token, window, cx)
                        .is_ok()
                    {
                        input.insert(" ", window, cx);
                    } else {
                        // Not representable as a chip: plain text, no mention.
                        input.set_selected_range(range, cx);
                        input.replace(
                            format!("{} ", mentions::token_text(&item.label)),
                            window,
                            cx,
                        );
                    }
                });
            }
            Trigger::Slash { .. } => {
                let rows = self.slash_rows();
                let Some(item) = rows.get(row).and_then(|ix| self.skills.get(*ix)).cloned() else {
                    return false;
                };
                self.input.update(cx, |input, cx| {
                    input.set_selected_range(range, cx);
                    input.replace(slash::insertion(&item), window, cx);
                });
            }
            Trigger::Bang { .. } => return false,
        }
        self.input.update(cx, |input, cx| input.focus(window, cx));
        self.recompute(cx);
        true
    }

    fn header(&self, p: &Palette, icon: IconName, title: &'static str) -> impl IntoElement {
        h_flex()
            .px(theme::sp(3.))
            .pt(theme::sp(2.))
            .pb(theme::sp(1.))
            .gap(theme::sp(1.5))
            .items_center()
            .text_size(theme::text_xs())
            .text_color(p.text_faint)
            .child(Icon::new(icon).with_size(px(11.)))
            .child(title)
    }

    fn footer(&self, p: &Palette) -> impl IntoElement {
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
        h_flex()
            .px(theme::sp(3.))
            .py(theme::sp(1.5))
            .gap(theme::sp(3.))
            .items_center()
            .border_t_1()
            .border_color(p.border)
            .bg(p.panel)
            .text_size(theme::text_xs())
            .text_color(p.text_faint)
            .child(
                h_flex()
                    .gap(theme::sp(1.))
                    .child(key("\u{2191}\u{2193}"))
                    .child("navigate"),
            )
            .child(
                h_flex()
                    .gap(theme::sp(1.))
                    .child(key("\u{21b5}"))
                    .child("insert"),
            )
            .child(
                h_flex()
                    .gap(theme::sp(1.))
                    .child(key("esc"))
                    .child("dismiss"),
            )
    }

    fn row_shell(&self, ix: usize, p: &Palette, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = ix == self.selected;
        h_flex()
            .id(("composer-popup-row", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex_shrink_0()
            .px(theme::sp(2.))
            .gap(theme::sp(2.))
            .items_center()
            .rounded(theme::radius())
            .cursor_pointer()
            .when(selected, |this| this.bg(p.accent_soft))
            .when(!selected, |this| this.hover(|style| style.bg(p.hover)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.accept_row(ix, window, cx);
            }))
    }

    fn mention_rows(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.mention_items
            .iter()
            .enumerate()
            .map(|(ix, item)| {
                let selected = ix == self.selected;
                self.row_shell(ix, p, cx)
                    .child(
                        div()
                            .size(px(20.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::radius_sm())
                            .bg(if selected { p.elevated } else { p.hover })
                            .child(
                                Icon::new(mentions::kind_icon(item.kind))
                                    .with_size(px(12.))
                                    .text_color(if selected {
                                        p.accent_text
                                    } else {
                                        p.text_muted
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(px(220.))
                            .truncate()
                            .text_size(theme::text_ui())
                            .text_color(p.text)
                            .child(item.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(
                                item.detail
                                    .clone()
                                    .unwrap_or_else(|| mentions::kind_name(item.kind).to_string()),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn skill_rows(&self, p: &Palette, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.slash_rows()
            .into_iter()
            .enumerate()
            .filter_map(|(ix, item_ix)| {
                let item = self.skills.get(item_ix)?;
                let badge = |label: &'static str| {
                    div()
                        .flex_shrink_0()
                        .h(px(16.))
                        .px(theme::sp(1.5))
                        .flex()
                        .items_center()
                        .rounded(theme::radius_sm())
                        .border_1()
                        .border_color(p.border)
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(label)
                };
                Some(
                    self.row_shell(ix, p, cx)
                        .child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(200.))
                                .truncate()
                                .text_size(theme::text_ui())
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.text)
                                .child(format!("/{}", item.name)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(theme::text_xs())
                                .text_color(p.text_muted)
                                .child(item.description.clone().unwrap_or_default()),
                        )
                        .when(item.kind == trama_protocol::SkillKind::Command, |this| {
                            this.child(badge(slash::kind_label(item.kind)))
                        })
                        .child(badge(slash::source_label(item.source)))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_bang(&self, command: &str, p: &Palette) -> AnyElement {
        h_flex()
            .max_w(px(POPUP_WIDTH))
            .px(theme::sp(2.5))
            .py(theme::sp(1.5))
            .gap(theme::sp(2.))
            .items_center()
            .rounded(theme::radius())
            .border_1()
            .border_color(p.border_strong)
            .bg(p.elevated)
            .shadow_md()
            .text_size(theme::text_sm())
            .text_color(p.text_muted)
            .child(
                Icon::new(IconName::SquareTerminal)
                    .with_size(px(13.))
                    .text_color(p.accent_text),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(p.text)
                    .child(if command.is_empty() {
                        "Open a shell in this thread's checkout".to_string()
                    } else {
                        format!("Run `{command}` in a terminal")
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child("\u{21b5} run \u{b7} esc to write a message"),
            )
            .into_any_element()
    }
}

impl Render for ComposerPopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.input.focus_handle(cx).is_focused(window);
        let Some(trigger) = self.visible_trigger().cloned().filter(|_| focused) else {
            return div().into_any_element();
        };
        let p = theme::palette(cx);
        let anchor = {
            let input = self.input.read(cx);
            let start = trigger.start();
            input
                .range_to_bounds(&(start..start))
                .map(|bounds| bounds.origin)
                .or_else(|| input.text_bounds().map(|bounds| bounds.origin))
                .unwrap_or_else(|| input.input_bounds().origin)
        };
        let card = match &trigger {
            Trigger::Bang { command } => self.render_bang(command, &p),
            Trigger::Mention { .. } | Trigger::Slash { .. } => {
                let (rows, icon, title) = match trigger {
                    Trigger::Mention { .. } => (
                        self.mention_rows(&p, cx),
                        IconName::AtSign,
                        "Mention a file, folder, thread or person",
                    ),
                    _ => (
                        self.skill_rows(&p, cx),
                        IconName::Slash,
                        "Skills and commands",
                    ),
                };
                if rows.is_empty() {
                    return div().into_any_element();
                }
                let visible = rows.len().min(MAX_VISIBLE_ROWS) as f32;
                v_flex()
                    .id("composer-popup")
                    .w(px(POPUP_WIDTH))
                    .rounded(theme::radius_lg())
                    .border_1()
                    .border_color(p.border_strong)
                    .bg(p.elevated)
                    .shadow_md()
                    .overflow_hidden()
                    // Keep the textarea focused when a row is clicked.
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .child(self.header(&p, icon, title))
                    .child(
                        v_flex()
                            .id("composer-popup-rows")
                            .w_full()
                            .h(px(visible * (ROW_HEIGHT + 1.) + 8.))
                            .px(theme::sp(1.))
                            .pb(theme::sp(1.))
                            .gap(px(1.))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(rows),
                    )
                    .child(self.footer(&p))
                    .into_any_element()
            }
        };
        deferred(
            anchored()
                .position(anchor)
                .anchor(Anchor::BottomLeft)
                .offset(point(px(-6.), px(-6.)))
                .snap_to_window_with_margin(px(8.))
                .child(card),
        )
        .with_priority(3)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `gpui_kit::*` brings GPUI's own `test` attribute into scope.
    use core::prelude::v1::test;

    #[test]
    fn dismissal_sticks_to_the_same_anchor_only() {
        let a = Trigger::Mention {
            start: 3,
            query: "x".into(),
        };
        let b = Trigger::Mention {
            start: 3,
            query: "xy".into(),
        };
        let c = Trigger::Mention {
            start: 9,
            query: String::new(),
        };
        assert!(same_anchor(&a, &b), "typing more keeps it dismissed");
        assert!(!same_anchor(&a, &c), "a new @ opens again");
        assert!(same_anchor(
            &Trigger::Bang {
                command: "a".into()
            },
            &Trigger::Bang {
                command: "ab".into()
            }
        ));
        assert!(!same_anchor(
            &a,
            &Trigger::Slash {
                start: 3,
                query: "x".into()
            }
        ));
    }
}
