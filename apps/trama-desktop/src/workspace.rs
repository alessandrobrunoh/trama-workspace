use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    dock::{
        BasePanel, DockArea, DockLayout, DockPlacement, DockSkin, Panel, PanelEvent, panel_handle,
    },
    h_flex,
    input::{Input, InputEvent, InputState},
    status_bar::StatusBar,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{ClientMessage, DaemonMessage, ThreadSnapshot, WorkspaceSnapshot};

use crate::chat::{AgentStatus, ChatPanel};
use crate::net::{DaemonClient, NetEvent};

const MOCK_THREADS: [&str; 2] = ["Implement authentication", "Refactor API endpoints"];

pub struct Workspace {
    dock_area: Entity<DockArea>,
    chat: Entity<ChatPanel>,
    sidebar: Entity<SidebarPanel>,
    client: DaemonClient,
    connected: bool,
    workspace_title: String,
    _observe_chat: Subscription,
    _sidebar_events: Subscription,
    _net_events: Task<()>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("TramaWorkspace");

        let (client, events) = crate::net::start();
        let chat_client = client.clone();
        let chat = cx.new(|cx| ChatPanel::new(window, cx, chat_client));
        let sidebar = cx.new(|cx| SidebarPanel::new(window, cx));
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
                    DockLayout::tabs().panel_view(panel_handle(sidebar.clone()), cx),
                    None,
                ),
                window,
                cx,
            );
            area.set_dock_size(DockPlacement::Left, px(280.), window, cx);
        });

        let observe_chat = cx.observe(&chat, |_, _, cx| cx.notify());
        let sidebar_events = cx.subscribe(&sidebar, |this, _sidebar, event, cx| {
            this.on_sidebar_event(event, cx);
        });

        let inbound = events.into_receiver();
        let net_events = cx.spawn(async move |this, cx| {
            while let Ok(event) = inbound.recv().await {
                let alive = this.update(cx, |this, cx| this.on_net_event(event, cx));
                if alive.is_err() {
                    break;
                }
            }
        });

        Self {
            dock_area,
            chat,
            sidebar,
            client,
            connected: false,
            workspace_title: "LaFraternita".into(),
            _observe_chat: observe_chat,
            _sidebar_events: sidebar_events,
            _net_events: net_events,
        }
    }

    fn on_sidebar_event(&mut self, event: &SidebarEvent, cx: &mut Context<Self>) {
        match event {
            SidebarEvent::SelectWorkspace(id) => {
                self.client.select_workspace(id.clone());
                self.chat.update(cx, |chat, cx| {
                    chat.set_thread(None, cx);
                });
                self.sync_workspace_title(cx);
            }
            SidebarEvent::SelectThread(id) => {
                self.client.select_thread(id.clone());
                self.chat.update(cx, |chat, cx| {
                    chat.set_thread(Some(id.clone()), cx);
                });
            }
            SidebarEvent::CreateThread { title } => {
                let Some(workspace_id) = self.sidebar.read(cx).selected_workspace.clone() else {
                    return;
                };
                self.client.send(ClientMessage::CreateThread {
                    workspace_id,
                    title: title.clone(),
                });
            }
        }
        cx.notify();
    }

    fn on_net_event(&mut self, event: NetEvent, cx: &mut Context<Self>) {
        match event {
            NetEvent::Connected { .. } => {
                self.connected = true;
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_connected(true, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(true, cx);
                });
            }
            NetEvent::Disconnected => {
                self.connected = false;
                self.workspace_title = "LaFraternita".into();
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_connected(false, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(false, cx);
                });
            }
            NetEvent::Message(message) => self.on_daemon_message(message, cx),
        }
        cx.notify();
    }

    fn on_daemon_message(&mut self, message: DaemonMessage, cx: &mut Context<Self>) {
        match message {
            DaemonMessage::Workspaces(list) => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_workspaces(list, cx);
                });
                self.maybe_select_default_workspace(cx);
                self.sync_workspace_title(cx);
            }
            DaemonMessage::Threads(list) => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_threads(list, cx);
                });
                self.maybe_select_default_thread(cx);
            }
            DaemonMessage::ThreadCreated(snapshot) => {
                let id = snapshot.id.clone();
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.upsert_thread(snapshot, cx);
                });
                if let Some(workspace_id) = self.sidebar.read(cx).selected_workspace.clone() {
                    self.client
                        .send(ClientMessage::ListThreads { workspace_id });
                }
                self.client.select_thread(id.clone());
                self.chat.update(cx, |chat, cx| {
                    chat.set_thread(Some(id), cx);
                });
            }
            other => {
                self.chat.update(cx, |chat, cx| {
                    chat.handle_daemon(other, cx);
                });
            }
        }
    }

    fn maybe_select_default_workspace(&mut self, cx: &mut Context<Self>) {
        let (selected, first) = {
            let sidebar = self.sidebar.read(cx);
            (
                sidebar.selected_workspace.clone(),
                sidebar.workspaces.first().map(|ws| ws.id.clone()),
            )
        };
        if selected.is_none() {
            if let Some(id) = first {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.select_workspace(id.clone(), cx);
                });
                self.client.select_workspace(id);
            }
        }
    }

    fn maybe_select_default_thread(&mut self, cx: &mut Context<Self>) {
        let (selected, first) = {
            let sidebar = self.sidebar.read(cx);
            (
                sidebar.selected_thread.clone(),
                sidebar.threads.first().map(|th| th.id.clone()),
            )
        };
        if selected.is_none() {
            if let Some(id) = first {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.select_thread(id.clone(), cx);
                });
                self.client.select_thread(id.clone());
                self.chat.update(cx, |chat, cx| {
                    chat.set_thread(Some(id), cx);
                });
            }
        }
    }

    fn sync_workspace_title(&mut self, cx: &App) {
        if let Some(title) = self.sidebar.read(cx).selected_workspace_title() {
            self.workspace_title = title;
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = if self.connected {
            "daemon: connected"
        } else {
            "daemon offline — run `cargo run -p trama-daemon`"
        };
        let banner_color = if self.connected {
            cx.theme().success
        } else {
            cx.theme().warning
        };

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
                                .child(self.workspace_title.clone()),
                        )
                        .child(div().text_xs().text_color(banner_color).child(banner)),
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

pub enum SidebarEvent {
    SelectWorkspace(WorkspaceId),
    SelectThread(ThreadId),
    CreateThread { title: String },
}

struct SidebarPanel {
    focus_handle: FocusHandle,
    connected: bool,
    workspaces: Vec<WorkspaceSnapshot>,
    threads: Vec<ThreadSnapshot>,
    selected_workspace: Option<WorkspaceId>,
    selected_thread: Option<ThreadId>,
    mock_thread: usize,
    new_thread: Entity<InputState>,
    _enter: Subscription,
}

impl SidebarPanel {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let new_thread = cx.new(|cx| InputState::new(window, cx).placeholder("New thread"));
        let enter = cx.subscribe_in(&new_thread, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit_new_thread(window, cx);
            }
        });
        Self {
            focus_handle: cx.focus_handle(),
            connected: false,
            workspaces: Vec::new(),
            threads: Vec::new(),
            selected_workspace: None,
            selected_thread: None,
            mock_thread: 0,
            new_thread,
            _enter: enter,
        }
    }

    fn set_connected(&mut self, connected: bool, cx: &mut Context<Self>) {
        self.connected = connected;
        if !connected {
            self.workspaces.clear();
            self.threads.clear();
            self.mock_thread = 0;
        }
        cx.notify();
    }

    fn set_workspaces(&mut self, workspaces: Vec<WorkspaceSnapshot>, cx: &mut Context<Self>) {
        self.workspaces = workspaces;
        if let Some(id) = &self.selected_workspace {
            if !self.workspaces.iter().any(|ws| &ws.id == id) {
                self.selected_workspace = None;
            }
        }
        cx.notify();
    }

    fn set_threads(&mut self, threads: Vec<ThreadSnapshot>, cx: &mut Context<Self>) {
        self.threads = threads;
        if let Some(id) = &self.selected_thread {
            if !self.threads.iter().any(|th| &th.id == id) {
                self.selected_thread = None;
            }
        }
        cx.notify();
    }

    fn upsert_thread(&mut self, snapshot: ThreadSnapshot, cx: &mut Context<Self>) {
        if let Some(existing) = self.threads.iter_mut().find(|th| th.id == snapshot.id) {
            *existing = snapshot.clone();
        } else {
            self.threads.push(snapshot.clone());
        }
        self.selected_thread = Some(snapshot.id);
        cx.notify();
    }

    fn select_workspace(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.selected_workspace = Some(id);
        self.selected_thread = None;
        self.threads.clear();
        cx.notify();
    }

    fn select_thread(&mut self, id: ThreadId, cx: &mut Context<Self>) {
        self.selected_thread = Some(id);
        cx.notify();
    }

    fn selected_workspace_title(&self) -> Option<String> {
        let id = self.selected_workspace.as_ref()?;
        self.workspaces
            .iter()
            .find(|ws| &ws.id == id)
            .map(|ws| ws.title.clone())
    }

    fn submit_new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected || self.selected_workspace.is_none() {
            return;
        }
        let mut title = None;
        self.new_thread.update(cx, |input, cx| {
            let value = input.value().to_string();
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                title = Some(trimmed.to_string());
                input.set_value("", window, cx);
            }
        });
        if let Some(title) = title {
            cx.emit(SidebarEvent::CreateThread { title });
        }
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connected = self.connected;
        let selected_workspace = self.selected_workspace.clone();
        let selected_thread = self.selected_thread.clone();
        let can_create = connected && selected_workspace.is_some();

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .p_3()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(section_label("Workspaces", cx))
                    .when(!connected, |this| {
                        this.child(
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
                    })
                    .when(connected && self.workspaces.is_empty(), |this| {
                        this.child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("No workspaces"),
                        )
                    })
                    .children(self.workspaces.iter().cloned().map(|ws| {
                        let selected = selected_workspace.as_ref() == Some(&ws.id);
                        let id = ws.id.clone();
                        let title = ws.title.clone();
                        div()
                            .id(SharedString::from(format!("ws-{}", ws.id)))
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
                                this.select_workspace(id.clone(), cx);
                                cx.emit(SidebarEvent::SelectWorkspace(id.clone()));
                            }))
                            .child(
                                v_flex().gap_0().child(div().text_sm().child(title)).child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("{} repos", ws.repo_count)),
                                ),
                            )
                    })),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(section_label("Repositories", cx))
                    .when(!connected, |this| {
                        this.child(repo_row(IconName::FolderGit2, "backend", cx))
                            .child(repo_row(IconName::FolderGit2, "frontend", cx))
                    })
                    .when(connected, |this| {
                        let count = selected_workspace
                            .as_ref()
                            .and_then(|id| {
                                self.workspaces
                                    .iter()
                                    .find(|ws| &ws.id == id)
                                    .map(|ws| ws.repo_count)
                            })
                            .unwrap_or(0);
                        this.child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{count} attached")),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(section_label("Threads", cx))
                    .when(!connected, |this| {
                        this.children(MOCK_THREADS.iter().enumerate().map(|(ix, title)| {
                            let selected = self.mock_thread == ix;
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
                                    this.mock_thread = ix;
                                    cx.notify();
                                }))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            Icon::new(IconName::MessageSquare).with_size(px(12.)),
                                        )
                                        .child(div().text_sm().child(*title)),
                                )
                        }))
                    })
                    .when(connected && self.threads.is_empty(), |this| {
                        this.child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("No threads yet"),
                        )
                    })
                    .children(self.threads.iter().cloned().map(|thread| {
                        let selected = selected_thread.as_ref() == Some(&thread.id);
                        let id = thread.id.clone();
                        let title = thread.title.clone();
                        div()
                            .id(SharedString::from(format!("th-{}", thread.id)))
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
                                this.select_thread(id.clone(), cx);
                                cx.emit(SidebarEvent::SelectThread(id.clone()));
                            }))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(IconName::MessageSquare).with_size(px(12.)))
                                    .child(div().text_sm().child(title)),
                            )
                    }))
                    .when(connected, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .pt_1()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Input::new(&self.new_thread).id("new-thread")),
                                )
                                .child(
                                    Button::new("create-thread")
                                        .ghost()
                                        .label("New")
                                        .disabled(!can_create)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.submit_new_thread(window, cx);
                                        })),
                                ),
                        )
                    }),
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
impl EventEmitter<SidebarEvent> for SidebarPanel {}

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
