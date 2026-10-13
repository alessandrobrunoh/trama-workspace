//! The side pane: a subagent's transcript next to its parent's.
//!
//! Clicking an "Agent spawned" line (or ⌥-clicking a subagent in the
//! sidebar) opens the child thread on the right of the transcript, in the
//! same document style and just as commentable, with its own composer to
//! message the subagent. Subagents opened from inside the pane stack: the
//! back button returns to the previous one. Selecting another thread in
//! the sidebar closes the pane. The pane can be resized from its left
//! edge or expanded over the parent transcript.
//!
//! The Changes / Copies drawer and the pane never overlap: the drawer
//! stays the rightmost column and the pane sits between the transcript
//! and the drawer, its width capped so the transcript keeps
//! [`MIN_TRANSCRIPT`].

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_domain::subagent::SubagentStatus;

use crate::theme::{self, Palette};

pub const DEFAULT_WIDTH: f32 = 520.;
pub const MIN_WIDTH: f32 = 340.;
pub const MAX_WIDTH: f32 = 1000.;
/// The transcript keeps at least this much room beside the pane.
pub const MIN_TRANSCRIPT: f32 = 360.;

/// The pane's state. The pure transitions return which thread the pane
/// stops and starts showing, so the caller can move its subscription.
#[derive(Debug, Clone, PartialEq)]
pub struct SidePane {
    thread: Option<ThreadId>,
    /// Subagents to go back to (opened from inside the pane).
    back: Vec<ThreadId>,
    pub expanded: bool,
    pub width: f32,
    /// The left edge is being dragged.
    pub resizing: bool,
}

/// The thread the pane stopped showing and the one it shows now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneChange {
    pub from: Option<ThreadId>,
    pub to: Option<ThreadId>,
}

impl Default for SidePane {
    fn default() -> Self {
        Self {
            thread: None,
            back: Vec::new(),
            expanded: false,
            width: DEFAULT_WIDTH,
            resizing: false,
        }
    }
}

impl SidePane {
    pub fn thread(&self) -> Option<&ThreadId> {
        self.thread.as_ref()
    }

    pub fn is_open(&self) -> bool {
        self.thread.is_some()
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    fn show(&mut self, thread: Option<ThreadId>) -> Option<PaneChange> {
        if self.thread == thread {
            return None;
        }
        let from = std::mem::replace(&mut self.thread, thread.clone());
        Some(PaneChange { from, to: thread })
    }

    /// Open `thread` from the transcript or the sidebar: it replaces what
    /// the pane showed. Nothing happens for the thread the transcript
    /// already shows (`main`).
    pub fn open(&mut self, thread: ThreadId, main: Option<&ThreadId>) -> Option<PaneChange> {
        if main == Some(&thread) || self.thread.as_ref() == Some(&thread) {
            return None;
        }
        self.back.clear();
        self.show(Some(thread))
    }

    /// Open `thread` from inside the pane: back returns to the current one.
    pub fn push(&mut self, thread: ThreadId, main: Option<&ThreadId>) -> Option<PaneChange> {
        if main == Some(&thread) || self.thread.as_ref() == Some(&thread) {
            return None;
        }
        if let Some(current) = self.thread.clone() {
            self.back.push(current);
        }
        self.show(Some(thread))
    }

    pub fn go_back(&mut self) -> Option<PaneChange> {
        let previous = self.back.pop()?;
        self.show(Some(previous))
    }

    pub fn close(&mut self) -> Option<PaneChange> {
        self.back.clear();
        self.expanded = false;
        self.resizing = false;
        self.show(None)
    }

    pub fn toggle_expanded(&mut self) {
        if self.is_open() {
            self.expanded = !self.expanded;
        }
    }

    /// Drag the left edge to `width`, within `available` (the room the
    /// transcript and the pane share).
    pub fn resize(&mut self, width: f32, available: f32) {
        let max = (available - MIN_TRANSCRIPT).clamp(MIN_WIDTH, MAX_WIDTH);
        self.width = width.clamp(MIN_WIDTH, max);
    }

    /// The width to draw at, given the room the transcript and the pane
    /// share (a narrow window shrinks the pane, never the transcript below
    /// [`MIN_TRANSCRIPT`] unless the pane would get narrower than
    /// [`MIN_WIDTH`]).
    pub fn shown_width(&self, available: f32) -> f32 {
        let max = (available - MIN_TRANSCRIPT).max(MIN_WIDTH);
        self.width.min(max)
    }
}

/// What the pane says about the subagent it shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub title: String,
    pub status: Option<SubagentStatus>,
    /// A subagent (or subthread) its parent's agent drives.
    pub controlled: bool,
    pub icon: IconName,
}

/// The footer's status words.
pub fn status_text(status: Option<SubagentStatus>, running: bool) -> &'static str {
    match status {
        Some(SubagentStatus::Queued) => "Queued",
        Some(SubagentStatus::Running) => "Running…",
        Some(SubagentStatus::WaitingToSync) => "Waiting to sync",
        Some(SubagentStatus::Completed) => "Completed",
        Some(SubagentStatus::Failed) => "Failed",
        None if running => "Running…",
        None => "Idle",
    }
}

/// Callbacks of the pane's chrome.
pub struct PaneActions {
    pub back: Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>,
    pub expand: Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>,
    pub open_main: Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>,
    pub close: Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>,
    pub start_resize: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

/// The pane: "Subagent" header with back / expand / open / close, the
/// subagent's title, its transcript, and a status footer.
#[allow(clippy::too_many_arguments)]
pub fn render(
    pane: &SidePane,
    info: &PaneInfo,
    running: bool,
    width: Option<f32>,
    body: AnyView,
    actions: PaneActions,
    p: &Palette,
) -> AnyElement {
    let PaneActions {
        back,
        expand,
        open_main,
        close,
        start_resize,
    } = actions;
    let header = h_flex()
        .w_full()
        .h(px(36.))
        .flex_shrink_0()
        .px(theme::sp(3.))
        .gap(theme::sp(1.))
        .items_center()
        .border_b_1()
        .border_color(p.border)
        .bg(p.panel)
        .child(
            div()
                .flex_1()
                .text_size(theme::text_ui())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child("Subagent"),
        )
        .child(
            Button::new("pane-open-main")
                .ghost()
                .xsmall()
                .icon(IconName::ExternalLink)
                .tooltip("Open as the selected thread")
                .on_click(open_main),
        )
        .child(
            Button::new("pane-expand")
                .ghost()
                .xsmall()
                .icon(if pane.expanded {
                    IconName::Minimize
                } else {
                    IconName::Maximize
                })
                .tooltip(if pane.expanded {
                    "Show the parent transcript"
                } else {
                    "Expand over the parent transcript"
                })
                .on_click(expand),
        )
        .child(
            Button::new("pane-close")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip("Close the pane")
                .on_click(close),
        );
    let title = h_flex()
        .w_full()
        .flex_shrink_0()
        .px(theme::sp(3.))
        .py(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .text_size(theme::text_sm())
        .text_color(p.text_muted)
        .when(pane.can_go_back(), |this| {
            this.child(
                Button::new("pane-back")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip("Back to the previous subagent")
                    .on_click(back),
            )
        })
        .child(
            Icon::new(info.icon)
                .with_size(px(13.))
                .text_color(p.text_faint),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(info.title.clone()),
        );
    let status = status_text(info.status, running);
    let status_color = match info.status {
        Some(SubagentStatus::Completed) => p.success,
        Some(SubagentStatus::Failed) => p.danger,
        Some(SubagentStatus::WaitingToSync) => p.warning,
        _ if running => p.accent_text,
        _ => p.text_muted,
    };
    let footer = h_flex()
        .w_full()
        .h(px(28.))
        .flex_shrink_0()
        .px(theme::sp(3.))
        .gap(theme::sp(2.))
        .items_center()
        .border_t_1()
        .border_color(p.border)
        .bg(p.panel)
        .text_size(theme::text_xs())
        .child(match info.status {
            Some(status) => crate::subagents::status_glyph(status, 12., p),
            None => Icon::new(IconName::Bot)
                .with_size(px(12.))
                .text_color(p.text_faint)
                .into_any_element(),
        })
        .child(div().text_color(status_color).child(status))
        .when(info.controlled, |this| {
            this.child(
                div()
                    .text_color(p.text_faint)
                    .child("Controlled by parent thread"),
            )
        });
    let handle = div()
        .id("pane-resize")
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(-3.))
        .w(px(6.))
        .cursor_col_resize()
        .when(pane.resizing, |this| this.bg(p.accent.opacity(0.4)))
        .hover(|style| style.bg(p.accent.opacity(0.25)))
        .on_mouse_down(MouseButton::Left, start_resize);
    v_flex()
        .id("side-pane")
        .relative()
        .h_full()
        .map(|this| match width {
            Some(width) => this.w(px(width)).flex_shrink_0(),
            None => this.flex_1().min_w_0(),
        })
        .border_l_1()
        .border_color(p.border)
        .bg(p.bg)
        .child(header)
        .child(title)
        .child(div().flex_1().min_h_0().child(body))
        .child(footer)
        .when(width.is_some(), |this| this.child(handle))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn id(text: &str) -> ThreadId {
        ThreadId::from(text)
    }

    fn change(from: Option<&str>, to: Option<&str>) -> Option<PaneChange> {
        Some(PaneChange {
            from: from.map(id),
            to: to.map(id),
        })
    }

    #[test]
    fn opening_replaces_and_pushing_stacks() {
        let mut pane = SidePane::default();
        let main = id("parent");
        assert_eq!(pane.open(id("a"), Some(&main)), change(None, Some("a")));
        assert!(pane.is_open() && !pane.can_go_back());
        // The same subagent again: nothing to do.
        assert_eq!(pane.open(id("a"), Some(&main)), None);
        // The transcript's own thread never opens beside itself.
        assert_eq!(pane.open(id("parent"), Some(&main)), None);
        assert_eq!(pane.push(id("parent"), Some(&main)), None);
        // From inside the pane: a stack to go back through.
        assert_eq!(
            pane.push(id("b"), Some(&main)),
            change(Some("a"), Some("b"))
        );
        assert_eq!(
            pane.push(id("c"), Some(&main)),
            change(Some("b"), Some("c"))
        );
        assert!(pane.can_go_back());
        assert_eq!(pane.go_back(), change(Some("c"), Some("b")));
        assert_eq!(pane.go_back(), change(Some("b"), Some("a")));
        assert_eq!(pane.go_back(), None);
        // From the transcript again: the stack is dropped.
        pane.push(id("b"), Some(&main));
        assert_eq!(
            pane.open(id("d"), Some(&main)),
            change(Some("b"), Some("d"))
        );
        assert!(!pane.can_go_back());
    }

    #[test]
    fn closing_and_selecting_another_thread_empty_the_pane() {
        let mut pane = SidePane::default();
        pane.open(id("a"), None);
        pane.push(id("b"), None);
        pane.toggle_expanded();
        assert!(pane.expanded);
        assert_eq!(pane.close(), change(Some("b"), None));
        assert!(!pane.is_open() && !pane.can_go_back() && !pane.expanded);
        assert_eq!(pane.close(), None);
        pane.toggle_expanded();
        assert!(!pane.expanded, "a closed pane does not expand");
        pane.open(id("a"), None);
        assert_eq!(pane.close(), change(Some("a"), None));
    }

    #[test]
    fn width_stays_between_the_limits_and_leaves_the_transcript_room() {
        let mut pane = SidePane::default();
        assert_eq!(pane.width, DEFAULT_WIDTH);
        pane.resize(100., 1400.);
        assert_eq!(pane.width, MIN_WIDTH);
        pane.resize(5000., 3000.);
        assert_eq!(pane.width, MAX_WIDTH);
        pane.resize(900., 1000.);
        assert_eq!(pane.width, 1000. - MIN_TRANSCRIPT);
        // A narrow window shrinks the drawn pane, down to its minimum.
        assert_eq!(pane.shown_width(800.), 800. - MIN_TRANSCRIPT);
        assert_eq!(pane.shown_width(500.), MIN_WIDTH);
        assert_eq!(pane.shown_width(3000.), pane.width);
    }

    #[test]
    fn footer_words_follow_the_subagent() {
        assert_eq!(
            status_text(Some(SubagentStatus::Completed), false),
            "Completed"
        );
        assert_eq!(
            status_text(Some(SubagentStatus::Running), false),
            "Running…"
        );
        assert_eq!(status_text(None, true), "Running…");
        assert_eq!(status_text(None, false), "Idle");
    }
}
