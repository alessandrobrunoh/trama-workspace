use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, TitleBar,
    dock::{
        BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, Panel, PanelEvent, panel_handle,
    },
    h_flex,
    status_bar::StatusBar,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::chat::{AgentStatus, ChatPanel};

pub struct Workspace {
    dock_area: Entity<DockArea>,
    chat: Entity<ChatPanel>,
    _observe_chat: Subscription,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("TramaWorkspace");

        let chat = cx.new(|cx| ChatPanel::new(window, cx));
        let sidebar = cx.new(|cx| SidebarPanel::new(cx));
        let (dock_area, _skin) = DockSkin::dock_area("trama-workspace", Some(1), window, cx);

        dock_area.update(cx, |area, cx| {
            area.set_center(
                DockLayout::v_split().child(
                    DockLayout::tabs().panel_view(panel_handle(chat.clone()), cx),
                    None,
                ),
                window,
                cx,
            );
            area.set_dock(
                DockPlacement::Left,
                DockLayout::v_split().child(
                    DockLayout::tabs().panel_view(panel_handle(sidebar), cx),
                    None,
                ),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Left, px(280.), window, cx);
        });

        let observe_chat = cx.observe(&chat, |_, _, cx| cx.notify());

        Self {
            dock_area,
            chat,
            _observe_chat: observe_chat,
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .pl_2()
                        .child(
                            Icon::new(IconName::Bot)
                                .with_size(px(14.))
                                .text_color(cx.theme().primary),
                        )
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("TramaWorkspace"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("LaFraternita"),
                        ),
                ),
            )
            .child(div().flex_1().min_h_0().child(self.dock_area.clone()))
            .child(render_status_bar(&self.chat, cx))
    }
}

fn render_status_bar(chat: &Entity<ChatPanel>, cx: &App) -> impl IntoElement {
    let chat = chat.read(cx);
    let status = match chat.status {
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "running",
    };
    let usage = chat.usage.as_ref();
    let tokens = match usage {
        Some(u) => format!(
            "in {}  out {}  cache {}/{}",
            format_tokens(u.input_tokens),
            format_tokens(u.output_tokens),
            format_tokens(u.cache_read_tokens),
            format_tokens(u.cache_write_tokens),
        ),
        None => "in —  out —  cache —".to_string(),
    };
    let cost = chat
        .cost_usd_micros
        .map(format_cost)
        .unwrap_or_else(|| "$—".into());
    let model = chat.model.clone().unwrap_or_else(|| "no model".into());
    let badge = match chat.turns {
        Some(n) => format!("turn {n} · {status}"),
        None => status.to_string(),
    };

    StatusBar::new()
        .left(
            h_flex()
                .gap_2()
                .items_center()
                .px_2()
                .child(status_dot(matches!(chat.status, AgentStatus::Running), cx))
                .child(div().text_xs().child(badge)),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(tokens),
        )
        .right(
            h_flex()
                .gap_3()
                .items_center()
                .px_2()
                .child(
                    div()
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(cost),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(model),
                ),
        )
}

fn status_dot(running: bool, cx: &App) -> impl IntoElement {
    let color = if running {
        cx.theme().warning
    } else {
        cx.theme().success
    };
    div().size(px(8.)).rounded_full().bg(color)
}

fn format_tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn format_cost(micros: u64) -> String {
    format!("${:.4}", micros as f64 / 1_000_000.0)
}

struct SidebarPanel {
    focus_handle: FocusHandle,
    selected_thread: usize,
}

impl SidebarPanel {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            selected_thread: 0,
        }
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let threads = ["Implement authentication", "Refactor API endpoints"];

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .p_3()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("LaFraternita"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Workspace"),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(section_label("Repositories", cx))
                    .child(repo_row(IconName::FolderGit2, "backend", cx))
                    .child(repo_row(IconName::FolderGit2, "frontend", cx)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(section_label("Threads", cx))
                    .children(threads.iter().enumerate().map(|(ix, title)| {
                        let selected = self.selected_thread == ix;
                        div()
                            .id(("thread", ix))
                            .px_2()
                            .py_1()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .when(selected, |this| {
                                this.bg(cx.theme().sidebar_accent)
                                    .text_color(cx.theme().sidebar_accent_foreground)
                            })
                            .when(!selected, |this| {
                                this.hover(|s| s.bg(cx.theme().sidebar_accent.opacity(0.45)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_thread = ix;
                                cx.notify();
                            }))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(IconName::MessageSquare).with_size(px(12.)))
                                    .child(div().text_sm().child(*title)),
                            )
                    })),
            )
    }
}

fn section_label(label: &'static str, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

fn repo_row(icon: IconName, name: &'static str, cx: &App) -> impl IntoElement {
    h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded(cx.theme().radius)
        .child(Icon::new(icon).with_size(px(13.)))
        .child(div().text_sm().child(name))
}

impl EventEmitter<PanelEvent> for SidebarPanel {}

impl Focusable for SidebarPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for SidebarPanel {
    fn panel_name(&self) -> &'static str {
        "Sidebar"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }
}

impl Panel for SidebarPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Workspace"
    }
}
