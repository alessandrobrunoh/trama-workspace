//! Left sidebar: a compact thread list grouped by project.
//!
//! Setup forms live in the Manage projects dialog ([`crate::projects`]). This
//! panel owns the shared [`Onboarding`] state because the thread list is its
//! main reader; the workspace and dialogs reach it through the entity.

use std::collections::HashSet;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{ThreadId, WorkspaceId};

use crate::onboarding::{Link, Onboarding, OnboardingRequest};
use crate::theme::{self, Palette};

/// Live state shown as the dot in front of a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadStatus {
    #[default]
    Idle,
    Running,
    /// The agent is waiting on the user (for example an approval request).
    NeedsAttention,
    Error,
}

impl ThreadStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::NeedsAttention => "Needs attention",
            Self::Error => "Error",
        }
    }

    pub fn color(self, p: &Palette) -> Hsla {
        match self {
            Self::Idle => p.text_faint,
            Self::Running => p.accent,
            Self::NeedsAttention => p.warning,
            Self::Error => p.danger,
        }
    }
}

pub enum SidebarEvent {
    Requests(Vec<OnboardingRequest>),
    NewThread,
    ManageProjects,
    QuickSwitch,
}

pub struct SidebarPanel {
    pub onboarding: Onboarding,
    /// Status of the selected thread; other threads have no live data.
    pub selected_status: ThreadStatus,
    collapsed: HashSet<WorkspaceId>,
    pub clear_workspace_name: bool,
    pub clear_repo_fields: bool,
    pub clear_thread_title: bool,
}

impl SidebarPanel {
    pub fn new() -> Self {
        Self {
            onboarding: Onboarding::new(),
            selected_status: ThreadStatus::Idle,
            collapsed: HashSet::new(),
            clear_workspace_name: false,
            clear_repo_fields: false,
            clear_thread_title: false,
        }
    }

    pub fn set_link(&mut self, link: Link, cx: &mut Context<Self>) {
        self.onboarding.set_link(link);
        cx.notify();
    }

    pub fn mark_workspace_saved(&mut self) {
        self.clear_workspace_name = true;
    }

    pub fn mark_repository_saved(&mut self) {
        self.clear_repo_fields = true;
    }

    pub fn mark_thread_saved(&mut self) {
        self.clear_thread_title = true;
    }

    pub fn selected_thread_title(&self) -> Option<String> {
        let id = self.onboarding.selected_thread.as_ref()?;
        self.onboarding
            .threads
            .iter()
            .find(|thread| &thread.id == id)
            .map(|thread| thread.title.clone())
    }

    pub fn selected_workspace_title(&self) -> Option<String> {
        let id = self.onboarding.selected_workspace.as_ref()?;
        self.onboarding
            .workspaces
            .iter()
            .find(|workspace| &workspace.id == id)
            .map(|workspace| workspace.title.clone())
    }

    pub fn emit_requests(&mut self, requests: Vec<OnboardingRequest>, cx: &mut Context<Self>) {
        if !requests.is_empty() {
            cx.emit(SidebarEvent::Requests(requests));
        }
    }

    pub fn select_thread(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        let requests = self.onboarding.select_thread(id);
        self.emit_requests(requests, cx);
        cx.notify();
    }

    fn click_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        if self.onboarding.selected_workspace.as_ref() == Some(&id) {
            if !self.collapsed.remove(&id) {
                self.collapsed.insert(id);
            }
        } else {
            self.collapsed.remove(&id);
            let requests = self.onboarding.select_workspace(id);
            self.emit_requests(requests, cx);
        }
        cx.notify();
    }
}

impl Default for SidebarPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitter<SidebarEvent> for SidebarPanel {}

/// Order threads so children follow their parent, with a nesting depth.
///
/// `items` is `(id, parent)` in display order. A parent that is missing from
/// the list (or a cycle) leaves the child at the top level. The current
/// protocol snapshot has no parent, so every row is depth 0 today; when
/// `ThreadSnapshot` grows `parent_thread_id`, pass it here.
pub fn nest_threads<T: PartialEq + Clone>(items: &[(T, Option<T>)]) -> Vec<(usize, usize)> {
    let known = |id: &T| items.iter().any(|(candidate, _)| candidate == id);
    let mut out = Vec::with_capacity(items.len());
    let mut placed = vec![false; items.len()];

    fn visit<T: PartialEq + Clone>(
        index: usize,
        depth: usize,
        items: &[(T, Option<T>)],
        placed: &mut [bool],
        out: &mut Vec<(usize, usize)>,
    ) {
        if placed[index] {
            return;
        }
        placed[index] = true;
        out.push((index, depth));
        let id = &items[index].0;
        for (child, (_, parent)) in items.iter().enumerate() {
            if parent.as_ref() == Some(id) {
                visit(child, depth + 1, items, placed, out);
            }
        }
    }

    for (index, (_, parent)) in items.iter().enumerate() {
        let is_root = match parent {
            None => true,
            Some(parent) => !known(parent),
        };
        if is_root {
            visit(index, 0, items, &mut placed, &mut out);
        }
    }
    // Anything left is part of a cycle: show it flat rather than hide it.
    for index in 0..items.len() {
        if !placed[index] {
            visit(index, 0, items, &mut placed, &mut out);
        }
    }
    out
}

pub fn status_dot(status: ThreadStatus, p: &Palette) -> impl IntoElement {
    let color = status.color(p);
    div()
        .size(px(7.))
        .flex_shrink_0()
        .rounded_full()
        .when(status == ThreadStatus::Idle, |this| {
            this.border_1().border_color(color)
        })
        .when(status != ThreadStatus::Idle, |this| this.bg(color))
}

impl Render for SidebarPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let connected = self.onboarding.connected();
        let workspaces = self.onboarding.workspaces.clone();
        let selected_workspace = self.onboarding.selected_workspace.clone();
        let threads = self.onboarding.threads.clone();
        let selected_thread = self.onboarding.selected_thread.clone();
        let can_thread = self.onboarding.can_create_thread();

        let new_thread = h_flex()
            .id("sidebar-new-thread")
            .flex_1()
            .h(px(30.))
            .px(theme::sp(2.5))
            .gap(theme::sp(2.))
            .items_center()
            .rounded(theme::radius())
            .border_1()
            .border_color(p.border)
            .bg(p.elevated)
            .text_size(theme::text_ui())
            .text_color(if connected { p.text } else { p.text_faint })
            .when(connected, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(p.hover).border_color(p.border_strong))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::NewThread)))
            })
            .child(
                Icon::new(IconName::SquarePen)
                    .with_size(px(14.))
                    .text_color(p.text_muted),
            )
            .child(div().flex_1().child("New thread"))
            .child(
                div()
                    .text_size(theme::text_xs())
                    .text_color(p.text_faint)
                    .child("\u{2318}N"),
            );

        let header = h_flex()
            .flex_shrink_0()
            .px(theme::sp(2.))
            .pt(theme::sp(2.))
            .pb(theme::sp(1.))
            .gap(theme::sp(1.))
            .items_center()
            .child(new_thread)
            .child(
                Button::new("sidebar-search")
                    .ghost()
                    .small()
                    .icon(IconName::Search)
                    .tooltip("Jump to thread (\u{2318}K)")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::QuickSwitch))),
            );

        let mut list = v_flex()
            .id("sidebar-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px(theme::sp(2.))
            .py(theme::sp(1.))
            .gap(px(1.))
            .child(
                h_flex()
                    .px(theme::sp(2.))
                    .pt(theme::sp(2.))
                    .pb(theme::sp(1.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::text_xs())
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.text_faint)
                            .child("Projects"),
                    )
                    .child(
                        Button::new("sidebar-manage-projects")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("Manage projects (\u{2318},)")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ManageProjects)),
                            ),
                    ),
            );

        if !connected {
            list = list.child(hint(&p, "Projects appear when the local daemon connects."));
        } else if workspaces.is_empty() {
            list = list.child(
                v_flex()
                    .px(theme::sp(2.))
                    .py(theme::sp(1.))
                    .gap(theme::sp(2.))
                    .child(hint(&p, "No projects yet."))
                    .child(
                        Button::new("sidebar-first-project")
                            .small()
                            .label("Create a project")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ManageProjects)),
                            ),
                    ),
            );
        }

        for workspace in &workspaces {
            let selected = selected_workspace.as_ref() == Some(&workspace.id);
            let expanded = selected && !self.collapsed.contains(&workspace.id);
            let id = workspace.id.clone();
            let count = self.onboarding.displayed_repo_count(workspace);
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("ws-{}", workspace.id)))
                    .h(px(28.))
                    .px(theme::sp(2.))
                    .gap(theme::sp(1.5))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_ui())
                    .hover(|style| style.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.click_workspace(id.clone(), cx);
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .with_size(px(12.))
                        .text_color(p.text_faint),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if selected { p.text } else { p.text_muted })
                            .child(workspace.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(format!("{count} repo{}", if count == 1 { "" } else { "s" })),
                    ),
            );
            if !expanded {
                continue;
            }
            if threads.is_empty() {
                list = list.child(
                    h_flex()
                        .pl(theme::sp(7.))
                        .pr(theme::sp(2.))
                        .py(theme::sp(1.))
                        .gap(theme::sp(1.))
                        .text_size(theme::text_sm())
                        .text_color(p.text_faint)
                        .child(if can_thread {
                            "No threads yet."
                        } else {
                            "Add a repository to start a thread."
                        }),
                );
                continue;
            }
            let pairs: Vec<(ThreadId, Option<ThreadId>)> = threads
                .iter()
                .map(|thread| (thread.id.clone(), None))
                .collect();
            for (index, depth) in nest_threads(&pairs) {
                let thread = &threads[index];
                let is_selected = selected_thread.as_ref() == Some(&thread.id);
                let status = if is_selected {
                    self.selected_status
                } else {
                    ThreadStatus::Idle
                };
                list = list.child(self.thread_row(
                    thread.id.clone(),
                    thread.title.clone(),
                    depth,
                    is_selected,
                    status,
                    &p,
                    cx,
                ));
            }
        }

        let link = self.onboarding.link().clone();
        let (link_color, link_label) = match link {
            Link::Online => (p.success, "Daemon connected"),
            Link::Offline => (p.warning, "Daemon offline"),
            Link::ProtocolMismatch { .. } => (p.danger, "Restart required"),
        };
        let footer = h_flex()
            .flex_shrink_0()
            .px(theme::sp(2.))
            .py(theme::sp(2.))
            .gap(theme::sp(1.))
            .items_center()
            .border_t_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .id("sidebar-manage")
                    .flex_1()
                    .h(px(28.))
                    .px(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_ui())
                    .text_color(p.text_muted)
                    .hover(|style| style.bg(p.hover).text_color(p.text))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ManageProjects)))
                    .child(Icon::new(IconName::Settings).with_size(px(14.)))
                    .child("Manage projects"),
            )
            .child(
                div()
                    .id("sidebar-link")
                    .p(theme::sp(2.))
                    .child(div().size(px(7.)).rounded_full().bg(link_color))
                    .tooltip(move |window, cx| Tooltip::new(link_label).build(window, cx)),
            );

        v_flex()
            .size_full()
            .bg(p.panel)
            .text_color(p.text)
            .child(header)
            .child(list)
            .child(footer)
    }
}

impl SidebarPanel {
    #[allow(clippy::too_many_arguments)]
    fn thread_row(
        &self,
        id: ThreadId,
        title: String,
        depth: usize,
        selected: bool,
        status: ThreadStatus,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let group = SharedString::from(format!("thread-row-{id}"));
        let click_id = id.clone();
        let copy_id = id.to_string();
        h_flex()
            .id(SharedString::from(format!("th-{id}")))
            .group(group.clone())
            .h(px(28.))
            .pl(theme::sp(6.) + theme::sp(4.) * depth as f32)
            .pr(theme::sp(1.))
            .gap(theme::sp(2.))
            .items_center()
            .rounded(theme::radius())
            .cursor_pointer()
            .text_size(theme::text_ui())
            .when(selected, |this| this.bg(p.selected).text_color(p.text))
            .when(!selected, |this| {
                this.text_color(p.text_muted)
                    .hover(|style| style.bg(p.hover).text_color(p.text))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_thread(click_id.clone(), cx);
            }))
            .child(
                div()
                    .id(SharedString::from(format!("th-status-{id}")))
                    .child(status_dot(status, p))
                    .tooltip(move |window, cx| Tooltip::new(status.label()).build(window, cx)),
            )
            .child(div().flex_1().min_w_0().truncate().child(title))
            .child(
                h_flex()
                    .opacity(0.)
                    .group_hover(group, |style| style.opacity(1.))
                    .child(
                        Button::new(SharedString::from(format!("th-copy-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("Copy thread id")
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_id.clone()));
                            }),
                    ),
            )
    }
}

fn hint(p: &Palette, text: &'static str) -> impl IntoElement {
    div()
        .px(theme::sp(2.))
        .py(theme::sp(1.))
        .text_size(theme::text_sm())
        .text_color(p.text_faint)
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::nest_threads;

    #[test]
    fn flat_threads_stay_in_order_at_depth_zero() {
        let items = [("a", None), ("b", None), ("c", None)];
        assert_eq!(nest_threads(&items), vec![(0, 0), (1, 0), (2, 0)]);
    }

    #[test]
    fn children_follow_their_parent_indented() {
        let items = [
            ("child", Some("root")),
            ("root", None),
            ("other", None),
            ("grandchild", Some("child")),
            ("orphan", Some("missing")),
        ];
        assert_eq!(
            nest_threads(&items),
            vec![(1, 0), (0, 1), (3, 2), (2, 0), (4, 0)]
        );
    }

    #[test]
    fn cycles_are_shown_flat_instead_of_hidden() {
        let items = [("a", Some("b")), ("b", Some("a"))];
        let nested = nest_threads(&items);
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0], (0, 0));
    }
}
