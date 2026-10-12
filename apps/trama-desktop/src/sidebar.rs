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
use trama_protocol::SharedThreadListing;

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
    /// Open (or join) a thread from "Shared with me".
    OpenShared(SharedThreadListing),
    RefreshShared,
    JoinShared,
    /// ⌥-click: show this thread (a subagent) in the side pane.
    OpenInPane(ThreadId),
}

pub struct SidebarPanel {
    pub onboarding: Onboarding,
    /// Status of the selected thread; other threads have no live data.
    pub selected_status: ThreadStatus,
    collapsed: HashSet<WorkspaceId>,
    pub clear_workspace_name: bool,
    pub clear_repo_fields: bool,
    pub clear_thread_title: bool,
    /// Shared-thread badges and the "Shared with me" list.
    pub collab: crate::collab::SidebarCollab,
    /// Collapsed threads and fully shown child lists.
    pub tree: crate::thread_tree::TreeState,
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
            collab: Default::default(),
            tree: Default::default(),
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
        self.tree.reveal(&self.onboarding.threads, &id);
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
            for tree_row in crate::thread_tree::tree_rows(&threads, &self.tree) {
                let (row, children, collapsed) = match tree_row {
                    crate::thread_tree::TreeRow::Thread {
                        row,
                        children,
                        collapsed,
                    } => (row, children, collapsed),
                    crate::thread_tree::TreeRow::ShowMore {
                        parent,
                        depth,
                        hidden,
                    } => {
                        list = list.child(self.show_more_row(parent, depth, hidden, &p, cx));
                        continue;
                    }
                };
                let thread = &threads[row.index];
                let is_selected = selected_thread.as_ref() == Some(&thread.id);
                let status = if is_selected {
                    self.selected_status
                } else {
                    ThreadStatus::Idle
                };
                list = list.child(self.thread_row(
                    thread,
                    &row,
                    (children, collapsed),
                    is_selected,
                    status,
                    &p,
                    cx,
                ));
            }
        }

        if connected && self.collab.signed_in {
            list = list.child(self.shared_section(&selected_thread, &p, cx));
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
    /// Children not shown yet: "Show N more", like Delta.
    fn show_more_row(
        &self,
        parent: ThreadId,
        depth: usize,
        hidden: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let indent = crate::thread_tree::indent_level(depth);
        h_flex()
            .id(SharedString::from(format!("th-more-{parent}")))
            .h(px(24.))
            .pl(theme::sp(0.5) + px(14.) + theme::sp(2.) + theme::sp(3.) * indent as f32)
            .pr(theme::sp(1.))
            .gap(theme::sp(1.5))
            .items_center()
            .rounded(theme::radius())
            .cursor_pointer()
            .text_size(theme::text_sm())
            .text_color(p.text_faint)
            .hover(|style| style.bg(p.hover).text_color(p.text_muted))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tree.show_all(&parent);
                cx.notify();
            }))
            .child(Icon::new(IconName::Ellipsis).with_size(px(12.)))
            .child(format!("Show {hidden} more"))
    }

    #[allow(clippy::too_many_arguments)]
    fn thread_row(
        &self,
        thread: &trama_protocol::ThreadSnapshot,
        row: &crate::subthreads::ThreadRow,
        (children, collapsed): (usize, bool),
        selected: bool,
        status: ThreadStatus,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = thread.id.clone();
        let title = thread.title.clone();
        let depth = row.depth;
        let indent = crate::thread_tree::indent_level(depth);
        let marker = crate::thread_tree::depth_marker(depth);
        let kind = row.kind;
        let verdict = row.verdict;
        let group = SharedString::from(format!("thread-row-{id}"));
        let click_id = id.clone();
        let toggle_id = id.clone();
        let copy_id = id.to_string();
        let subagent = thread
            .subagent
            .as_ref()
            .map(|snapshot| crate::subagents::sidebar_badge(&id, snapshot, p));
        // Fixed-width slot so rows line up whether or not they have children.
        let chevron = div()
            .id(SharedString::from(format!("th-toggle-{id}")))
            .w(px(14.))
            .h(px(14.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(theme::radius_sm())
            .when(children > 0, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.tree.toggle_collapsed(&toggle_id);
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .with_size(px(11.))
                        .text_color(p.text_faint),
                    )
            });
        h_flex()
            .id(SharedString::from(format!("th-{id}")))
            .group(group.clone())
            .h(px(28.))
            .pl(theme::sp(0.5) + theme::sp(3.) * indent as f32)
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
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if event.modifiers().alt {
                    cx.emit(SidebarEvent::OpenInPane(click_id.clone()));
                } else {
                    this.select_thread(click_id.clone(), cx);
                }
            }))
            .child(chevron)
            .child(
                div()
                    .id(SharedString::from(format!("th-status-{id}")))
                    .child(status_dot(status, p))
                    .tooltip(move |window, cx| Tooltip::new(status.label()).build(window, cx)),
            )
            .children(crate::subthreads::kind_icon(kind).map(|icon| {
                div()
                    .id(SharedString::from(format!("th-kind-{id}")))
                    .flex_shrink_0()
                    .child(Icon::new(icon).with_size(px(12.)).text_color(p.text_faint))
                    .tooltip(move |window, cx| {
                        Tooltip::new(crate::subthreads::kind_label(kind)).build(window, cx)
                    })
            }))
            .children(subagent)
            .child(div().flex_1().min_w_0().truncate().child(title))
            .when_some(marker, |this, marker| {
                let tooltip: SharedString = format!("Nested {marker} levels deep").into();
                this.child(
                    div()
                        .id(SharedString::from(format!("th-depth-{id}")))
                        .flex_shrink_0()
                        .px(theme::sp(1.))
                        .rounded(theme::radius_sm())
                        .bg(p.hover)
                        .text_size(theme::text_xs())
                        .text_color(p.text_faint)
                        .child(format!("\u{21b3}{marker}"))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)),
                )
            })
            .when(self.collab.shared.contains(&id), |this| {
                this.child(
                    div()
                        .id(SharedString::from(format!("th-shared-{id}")))
                        .flex_shrink_0()
                        .child(
                            Icon::new(IconName::Users)
                                .with_size(px(12.))
                                .text_color(p.text_faint),
                        )
                        .tooltip(|window, cx| Tooltip::new("Shared thread").build(window, cx)),
                )
            })
            .children(verdict.map(|badge| {
                div()
                    .id(SharedString::from(format!("th-verdict-{id}")))
                    .flex_shrink_0()
                    .child(
                        Icon::new(match badge {
                            crate::subthreads::VerdictBadge::Approved => IconName::CircleCheckBig,
                            crate::subthreads::VerdictBadge::ChangesRequested => {
                                IconName::CircleAlert
                            }
                        })
                        .with_size(px(12.))
                        .text_color(badge.color(p)),
                    )
                    .tooltip(move |window, cx| Tooltip::new(badge.label()).build(window, cx))
            }))
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

impl SidebarPanel {
    /// Threads other people shared with the signed-in user.
    fn shared_section(
        &self,
        selected: &Option<ThreadId>,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let header = h_flex()
            .px(theme::sp(2.))
            .pt(theme::sp(4.))
            .pb(theme::sp(1.))
            .gap(px(2.))
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(theme::text_xs())
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.text_faint)
                    .child("Shared with me"),
            )
            .child(
                Button::new("sidebar-refresh-shared")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Refresh shared threads")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::RefreshShared))),
            )
            .child(
                Button::new("sidebar-join-shared")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Link)
                    .tooltip("Join a shared thread with a link")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::JoinShared))),
            );
        let mut section = v_flex().gap(px(1.)).child(header);
        if self.collab.shared_with_me.is_empty() {
            section = section.child(hint(p, "Nothing shared with you yet."));
        }
        for listing in &self.collab.shared_with_me {
            let is_selected =
                listing.local_thread_id.is_some() && &listing.local_thread_id == selected;
            let joined = listing.local_thread_id.is_some();
            let open = listing.clone();
            let tooltip: SharedString = format!(
                "Owned by {}{}",
                listing.owner,
                if joined {
                    ""
                } else {
                    " · click to join it into the selected project"
                }
            )
            .into();
            section = section.child(
                h_flex()
                    .id(SharedString::from(format!(
                        "shared-{}",
                        listing.collab_thread_id
                    )))
                    .h(px(28.))
                    .pl(theme::sp(2.))
                    .pr(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_ui())
                    .when(is_selected, |this| this.bg(p.selected).text_color(p.text))
                    .when(!is_selected, |this| {
                        this.text_color(p.text_muted)
                            .hover(|style| style.bg(p.hover).text_color(p.text))
                    })
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SidebarEvent::OpenShared(open.clone()));
                    }))
                    .child(crate::presence::avatar(p, &listing.owner, 16., false))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(listing.title.clone()),
                    )
                    .when(!joined, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .px(theme::sp(1.5))
                                .rounded(theme::radius_sm())
                                .border_1()
                                .border_color(p.border)
                                .text_size(theme::text_xs())
                                .text_color(p.text_faint)
                                .child("Join"),
                        )
                    }),
            );
        }
        section
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
