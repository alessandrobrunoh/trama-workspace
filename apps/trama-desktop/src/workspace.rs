use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    status_bar::StatusBar,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{ClientMessage, DaemonMessage, WorkspaceSnapshot};

use crate::chat::{AgentStatus, ChatPanel};
use crate::net::{DaemonClient, NetEvent};
use crate::onboarding::{Feedback, Link, Onboarding, OnboardingRequest};
use crate::startup::{DaemonLocate, DaemonStart};

pub struct Workspace {
    chat: Entity<ChatPanel>,
    sidebar: Entity<SidebarPanel>,
    client: DaemonClient,
    connected: bool,
    _observe_chat: Subscription,
    _sidebar_events: Subscription,
    _net_events: Task<()>,
    _daemon_start: Option<Task<()>>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("TramaWorkspace");

        let (client, events) = crate::net::start();
        let chat_client = client.clone();
        let chat = cx.new(|cx| ChatPanel::new(window, cx, chat_client));
        let sidebar = cx.new(|cx| SidebarPanel::new(window, cx));

        let observe_chat = cx.observe(&chat, |_, _, cx| cx.notify());
        let sidebar_events = cx.subscribe(&sidebar, |this, _sidebar, event, cx| {
            this.on_sidebar_event(event, cx);
        });

        let inbound = events.into_receiver();
        let net_events = cx.spawn(async move |this, cx| {
            while let Ok(event) = inbound.recv().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.on_net_event(event, window, cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        Self {
            chat,
            sidebar,
            client,
            connected: false,
            _observe_chat: observe_chat,
            _sidebar_events: sidebar_events,
            _net_events: net_events,
            _daemon_start: None,
        }
    }

    fn on_sidebar_event(&mut self, event: &SidebarEvent, cx: &mut Context<Self>) {
        match event {
            SidebarEvent::Requests(requests) => {
                for request in requests.clone() {
                    self.dispatch_request(request, cx);
                }
            }
            SidebarEvent::StartDaemon => self.start_daemon(cx),
        }
        cx.notify();
    }

    fn dispatch_request(&mut self, request: OnboardingRequest, cx: &mut Context<Self>) {
        match request {
            OnboardingRequest::SelectWorkspace { workspace_id } => {
                self.client.select_workspace(workspace_id);
                self.focus_thread(None, cx);
            }
            OnboardingRequest::SelectThread { thread_id } => {
                self.client.select_thread(thread_id.clone());
                self.focus_thread(Some(thread_id), cx);
            }
            other => {
                if let Some(message) = other.client_message() {
                    let _ = self.client.send(message);
                }
            }
        }
    }

    fn focus_thread(&mut self, thread_id: Option<ThreadId>, cx: &mut Context<Self>) {
        self.chat.update(cx, |chat, cx| {
            chat.set_thread(None, cx);
            chat.set_thread(thread_id, cx);
            chat.model = None;
            chat.usage = None;
            chat.cost_usd_micros = None;
            chat.turns = None;
        });
    }

    fn on_net_event(&mut self, event: NetEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            NetEvent::Connected { .. } => {
                self.connected = true;
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_link(Link::Online, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(true, cx);
                });
            }
            NetEvent::Disconnected => {
                self.connected = false;
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_link(Link::Offline, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(false, cx);
                });
            }
            NetEvent::ProtocolMismatch { daemon_protocol } => {
                self.connected = false;
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_link(Link::ProtocolMismatch { daemon_protocol }, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(false, cx);
                });
            }
            NetEvent::OutboundRejected(message) => self.on_outbound_rejected(message, window, cx),
            NetEvent::Message(message) => self.on_daemon_message(message, cx),
        }
        cx.notify();
    }

    fn on_outbound_rejected(
        &mut self,
        message: ClientMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let ClientMessage::SendPrompt { thread_id, text } = message {
            let restore = self.sidebar.update(cx, |sidebar, _cx| {
                sidebar
                    .onboarding
                    .note_rejected_prompt(thread_id.clone(), text)
            });
            if let Some(text) = restore {
                self.chat.update(cx, |chat, cx| {
                    chat.restore_prompt(&thread_id, &text, window, cx);
                });
            }
            self.sidebar.update(cx, |sidebar, cx| {
                sidebar.onboarding.note_error(
                    "offline",
                    "The prompt was not sent. The draft stays with that thread.",
                );
                cx.notify();
            });
            return;
        }
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.onboarding.note_error(
                "offline",
                "The daemon disconnected before the change was sent.",
            );
            cx.notify();
        });
    }

    fn start_daemon(&mut self, cx: &mut Context<Self>) {
        let mismatch = matches!(
            self.sidebar.read(cx).onboarding.link(),
            Link::ProtocolMismatch { .. }
        );
        if mismatch {
            return;
        }
        match crate::startup::locate_daemon() {
            DaemonLocate::Missing { instructions } => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.finish_daemon_start(
                        format!(
                            "{instructions} Build and install trama-daemon yourself (`cargo run -p trama-daemon`). This app will not download or compile it."
                        ),
                        false,
                    );
                    cx.notify();
                });
            }
            DaemonLocate::Found(path) => self.spawn_daemon(path, cx),
        }
    }

    fn spawn_daemon(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let began = self.sidebar.update(cx, |sidebar, cx| {
            let began = sidebar.onboarding.begin_daemon_start();
            cx.notify();
            began
        });
        if !began {
            return;
        }
        let background = cx.background_executor().clone();
        self._daemon_start = Some(cx.spawn(async move |this, cx| {
            let outcome = background
                .spawn(async move { crate::startup::start_daemon(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sidebar.update(cx, |sidebar, cx| {
                    match outcome {
                        DaemonStart::Started => sidebar.onboarding.finish_daemon_start(
                            "Daemon process started. Waiting for it to accept connections.",
                            true,
                        ),
                        DaemonStart::AlreadyRunning => sidebar.onboarding.finish_daemon_start(
                            "A daemon is already running. This app did not start another one.",
                            true,
                        ),
                        DaemonStart::Failed { message } => {
                            sidebar.onboarding.finish_daemon_start(message, false);
                        }
                    }
                    cx.notify();
                });
            });
        }));
    }

    fn on_daemon_message(&mut self, message: DaemonMessage, cx: &mut Context<Self>) {
        match message {
            DaemonMessage::Workspaces(list) => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.apply_workspaces(list);
                    cx.notify();
                });
                self.maybe_select_default_workspace(cx);
            }
            DaemonMessage::WorkspaceCreated(snapshot) => {
                let requests = self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.mark_workspace_saved();
                    let requests = sidebar.onboarding.apply_workspace_created(snapshot);
                    cx.notify();
                    requests
                });
                for request in requests {
                    self.dispatch_request(request, cx);
                }
            }
            DaemonMessage::Repositories {
                workspace_id,
                repositories,
            } => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .onboarding
                        .apply_repositories(workspace_id, repositories);
                    cx.notify();
                });
            }
            DaemonMessage::RepositoryAdded {
                workspace_id,
                repository,
            } => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.mark_repository_saved();
                    sidebar
                        .onboarding
                        .apply_repository_added(workspace_id, repository);
                    cx.notify();
                });
            }
            DaemonMessage::Threads(list) => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.apply_threads(list);
                    cx.notify();
                });
            }
            DaemonMessage::ThreadCreated(snapshot) => {
                let requests = self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.mark_thread_saved();
                    let requests = sidebar.onboarding.apply_thread_created(snapshot);
                    cx.notify();
                    requests
                });
                for request in requests {
                    self.dispatch_request(request, cx);
                }
            }
            DaemonMessage::ThreadMigrated(snapshot) => {
                let requests = self.sidebar.update(cx, |sidebar, cx| {
                    let requests = sidebar.onboarding.apply_thread_migrated(snapshot);
                    cx.notify();
                    requests
                });
                for request in requests {
                    self.dispatch_request(request, cx);
                }
            }
            DaemonMessage::Error { code, message } => {
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.note_error(&code, &message);
                    cx.notify();
                });
                self.chat.update(cx, |chat, cx| {
                    chat.handle_daemon(DaemonMessage::Error { code, message }, cx);
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
        let first = {
            let onboarding = &self.sidebar.read(cx).onboarding;
            if onboarding.selected_workspace.is_some() {
                return;
            }
            onboarding.workspaces.first().map(|ws| ws.id.clone())
        };
        if let Some(id) = first {
            let requests = self.sidebar.update(cx, |sidebar, cx| {
                let requests = sidebar.onboarding.select_workspace(id);
                cx.notify();
                requests
            });
            for request in requests {
                self.dispatch_request(request, cx);
            }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let onboarding_banner = self.sidebar.read(cx).onboarding.banner();
        let alert = self.sidebar.read(cx).onboarding.alert().map(str::to_string);
        let link = self.sidebar.read(cx).onboarding.link().clone();
        let banner_color = match link {
            Link::Online => cx.theme().success,
            Link::Offline => cx.theme().warning,
            Link::ProtocolMismatch { .. } => cx.theme().danger,
        };

        let thread_title = self
            .sidebar
            .read(cx)
            .selected_thread_title()
            .unwrap_or_else(|| "Select a thread".into());
        let start_daemon = cx.listener(|this, _, _, cx| this.start_daemon(cx));

        v_flex()
            .size_full()
            .bg(rgb(0x191b1d))
            .text_color(rgb(0xe3e5e8))
            .child(
                TitleBar::new().h(px(32.)).child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .gap_2()
                        .items_center()
                        .px_3()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Icon::new(IconName::MessageSquare)
                                        .with_size(px(13.))
                                        .text_color(cx.theme().muted_foreground),
                                )
                                .child(div().text_sm().child(thread_title)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(banner_color)
                                .child(onboarding_banner),
                        ),
                ),
            )
            .when_some(alert, |this, alert| {
                this.child(
                    div()
                        .w_full()
                        .px_3()
                        .py_1()
                        .bg(cx.theme().danger.opacity(0.15))
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(alert),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(px(320.))
                            .h_full()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(rgb(0x34373a))
                            .child(self.sidebar.clone()),
                    )
                    .child(div().flex_1().min_w_0().h_full().child({
                        if matches!(link, Link::Online) {
                            self.chat.clone().into_any_element()
                        } else {
                            connection_panel(
                                matches!(link, Link::ProtocolMismatch { .. }),
                                start_daemon,
                            )
                            .into_any_element()
                        }
                    })),
            )
            .child(render_status_bar(&self.chat, self.connected, cx))
    }
}

fn connection_panel(
    mismatch: bool,
    start_daemon: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    v_flex()
        .size_full()
        .bg(rgb(0x191b1d))
        .items_center()
        .justify_center()
        .px_10()
        .gap_3()
        .child(
            div().text_sm().child(if mismatch {
                "The daemon refused the handshake."
            } else {
                "The local daemon is not connected."
            }),
        )
        .child(div().text_xs().text_color(rgb(0x9aa0a6)).child(if mismatch {
            "Quit and restart both TramaWorkspace and the daemon. Do not start a second copy. Stored threads are not migrated. Send stays disabled."
        } else {
            "This is not a demo. Send stays disabled until a real daemon accepts the connection. Start one below, or run `cargo run -p trama-daemon` yourself. Nothing is downloaded."
        }))
        .child(
            Button::new("start-daemon-main")
                .label(if mismatch {
                    "Restart required"
                } else {
                    "Start daemon"
                })
                .disabled(mismatch)
                .on_click(start_daemon),
        )
}

fn render_status_bar(chat: &Entity<ChatPanel>, connected: bool, cx: &App) -> impl IntoElement {
    let chat = chat.read(cx);
    let status = match chat.status {
        AgentStatus::Idle => "idle",
        AgentStatus::Running => "running",
    };
    let usage = if connected { chat.usage.as_ref() } else { None };
    let tokens = match usage {
        Some(u) => format!(
            "in {}  out {}  cache {}/{}",
            format_tokens(u.input_tokens),
            format_tokens(u.output_tokens),
            format_tokens(u.cache_read_tokens),
            format_tokens(u.cache_write_tokens),
        ),
        None => "in --  out --  cache --".to_string(),
    };
    let cost = if connected {
        chat.cost_usd_micros
            .map(format_cost)
            .unwrap_or_else(|| "$--".into())
    } else {
        "$--".into()
    };
    let model = if connected {
        chat.model.clone().unwrap_or_else(|| "no model".into())
    } else {
        "no model".into()
    };
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
    Requests(Vec<OnboardingRequest>),
    StartDaemon,
}

struct SidebarPanel {
    onboarding: Onboarding,
    workspace_name: Entity<InputState>,
    repo_path: Entity<InputState>,
    repo_alias: Entity<InputState>,
    new_thread: Entity<InputState>,
    clear_workspace_name: bool,
    clear_repo_fields: bool,
    clear_thread_title: bool,
    picker_open: bool,
    _picker: Option<Task<()>>,
    _workspace_enter: Subscription,
    _repo_enter: Subscription,
    _thread_enter: Subscription,
}

impl SidebarPanel {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_name = cx.new(|cx| InputState::new(window, cx).placeholder("New workspace"));
        let repo_path =
            cx.new(|cx| InputState::new(window, cx).placeholder("Existing local repo path"));
        let repo_alias = cx.new(|cx| InputState::new(window, cx).placeholder("Alias"));
        let new_thread = cx.new(|cx| InputState::new(window, cx).placeholder("New thread"));
        let workspace_enter =
            cx.subscribe_in(&workspace_name, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit_workspace(window, cx);
                }
            });
        let repo_enter = cx.subscribe_in(&repo_alias, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit_repository(window, cx);
            }
        });
        let thread_enter = cx.subscribe_in(&new_thread, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.submit_thread(window, cx);
            }
        });
        Self {
            onboarding: Onboarding::new(),
            workspace_name,
            repo_path,
            repo_alias,
            new_thread,
            clear_workspace_name: false,
            clear_repo_fields: false,
            clear_thread_title: false,
            picker_open: false,
            _picker: None,
            _workspace_enter: workspace_enter,
            _repo_enter: repo_enter,
            _thread_enter: thread_enter,
        }
    }

    fn set_link(&mut self, link: Link, cx: &mut Context<Self>) {
        self.onboarding.set_link(link);
        cx.notify();
    }

    fn mark_workspace_saved(&mut self) {
        self.clear_workspace_name = true;
    }

    fn mark_repository_saved(&mut self) {
        self.clear_repo_fields = true;
    }

    fn mark_thread_saved(&mut self) {
        self.clear_thread_title = true;
    }

    fn selected_thread_title(&self) -> Option<String> {
        let id = self.onboarding.selected_thread.as_ref()?;
        self.onboarding
            .threads
            .iter()
            .find(|thread| &thread.id == id)
            .map(|thread| thread.title.clone())
    }

    fn emit_requests(&mut self, requests: Vec<OnboardingRequest>, cx: &mut Context<Self>) {
        if !requests.is_empty() {
            cx.emit(SidebarEvent::Requests(requests));
        }
    }

    fn submit_workspace(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.workspace_name.read(cx).value().to_string();
        if let Some(request) = self.onboarding.submit_workspace(&name) {
            self.emit_requests(vec![request], cx);
        }
        cx.notify();
    }

    fn submit_repository(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let path = self.repo_path.read(cx).value().to_string();
        let alias = self.repo_alias.read(cx).value().to_string();
        if let Some(request) = self.onboarding.submit_repository(&path, &alias) {
            self.emit_requests(vec![request], cx);
        }
        cx.notify();
    }

    fn submit_thread(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let title = self.new_thread.read(cx).value().to_string();
        if let Some(request) = self.onboarding.submit_thread(&title) {
            self.emit_requests(vec![request], cx);
        }
        cx.notify();
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        if self.picker_open {
            return;
        }
        self.picker_open = true;
        let outcome = crate::dialog::prompt_for_repository_directory(cx);
        self._picker = Some(cx.spawn(async move |this, cx| {
            let outcome = outcome.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.picker_open = false;
                match outcome {
                    crate::dialog::PickerOutcome::Picked(Some(path)) => {
                        let text = path.display().to_string();
                        this.repo_path.update(cx, |input, cx| {
                            input.set_value(text, window, cx);
                        });
                    }
                    crate::dialog::PickerOutcome::Failed(message) => {
                        this.onboarding.note_picker_failure(message);
                    }
                    crate::dialog::PickerOutcome::Picked(None)
                    | crate::dialog::PickerOutcome::Cancelled => {}
                }
                cx.notify();
            });
        }));
    }

    fn open_prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.onboarding.can_prepare_checkout() {
            return;
        }
        let panel = cx.weak_entity();
        crate::dialog::open_migration_confirm(window, cx, move |_window, cx| {
            let _ = panel.update(cx, |this, cx| {
                if let Some(request) = this.onboarding.confirm_migration() {
                    cx.emit(SidebarEvent::Requests(vec![request]));
                }
                cx.notify();
            });
        });
    }

    fn flush_cleared_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.clear_workspace_name {
            self.clear_workspace_name = false;
            self.workspace_name
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        if self.clear_repo_fields {
            self.clear_repo_fields = false;
            self.repo_path
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.repo_alias
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        if self.clear_thread_title {
            self.clear_thread_title = false;
            self.new_thread
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }
}

impl Render for SidebarPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_cleared_inputs(window, cx);
        let connected = self.onboarding.connected();
        let mismatch = matches!(self.onboarding.link(), Link::ProtocolMismatch { .. });
        let detail = self.onboarding.status_detail();
        let workspaces = self.onboarding.workspaces.clone();
        let selected_workspace = self.onboarding.selected_workspace.clone();
        let repositories = self.onboarding.repositories.clone();
        let repos_loaded = self.onboarding.repositories_loaded();
        let threads = self.onboarding.threads.clone();
        let selected_thread = self.onboarding.selected_thread.clone();
        let can_workspace = self.onboarding.can_create_workspace();
        let can_repo = self.onboarding.can_add_repository();
        let can_thread = self.onboarding.can_create_thread();
        let can_prepare = self.onboarding.can_prepare_checkout();
        let can_start = self.onboarding.can_start_daemon();
        let workspace_feedback = self.onboarding.workspace_feedback.clone();
        let repo_feedback = self.onboarding.repo_feedback.clone();
        let thread_feedback = self.onboarding.thread_feedback.clone();
        let migration_feedback = self.onboarding.migration_feedback.clone();
        let daemon_feedback = self.onboarding.daemon_feedback.clone();
        let workspace_rows: Vec<_> = workspaces
            .iter()
            .map(|workspace| {
                (
                    workspace.clone(),
                    self.onboarding.displayed_repo_count(workspace),
                )
            })
            .collect();

        v_flex()
            .size_full()
            .bg(rgb(0x202225))
            .text_color(rgb(0xe3e5e8))
            .px_3()
            .py_3()
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_4()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex().gap_1().child(section_label("Pinned", cx)).child(
                            div()
                                .px_2()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Pinning unavailable"),
                        ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(section_label("Projects", cx))
                            .when(!detail.is_empty(), |this| {
                                this.child(feedback_line(
                                    &Feedback::Error(detail.clone()),
                                    cx,
                                ))
                            })
                            .when(can_start, |this| {
                                this.child(
                                    Button::new("start-daemon")
                                        .ghost()
                                        .label("Start daemon")
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(SidebarEvent::StartDaemon);
                                        })),
                                )
                            })
                            .when(mismatch, |this| {
                                this.child(
                                    Button::new("start-daemon-blocked")
                                        .ghost()
                                        .label("Restart required")
                                        .disabled(true),
                                )
                            })
                            .children(daemon_feedback.clone().map(|feedback| {
                                feedback_line(&feedback, cx).into_any_element()
                            }))
                            .when(connected && workspaces.is_empty(), |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No workspaces yet. Name one below."),
                                )
                            })
                            .children(workspace_rows.into_iter().map(|(ws, count)| {
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
                                        this.hover(|s| {
                                            s.bg(cx.theme().sidebar_accent.opacity(0.45))
                                        })
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let requests =
                                            this.onboarding.select_workspace(id.clone());
                                        this.emit_requests(requests, cx);
                                    }))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Icon::new(IconName::FolderGit2).with_size(px(14.)),
                                            )
                                            .child(
                                                v_flex()
                                                    .gap_0()
                                                    .child(div().text_sm().child(title))
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(
                                                                cx.theme().muted_foreground,
                                                            )
                                                            .child(format!("{count} repos")),
                                                    ),
                                            ),
                                    )
                            }))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .pt_1()
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Input::new(&self.workspace_name).id("workspace-name"),
                                        ),
                                    )
                                    .child(
                                        Button::new("create-workspace")
                                            .ghost()
                                            .label("New workspace")
                                            .disabled(!can_workspace)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit_workspace(window, cx);
                                            })),
                                    ),
                            )
                            .children(workspace_feedback.clone().map(|feedback| {
                                feedback_line(&feedback, cx).into_any_element()
                            })),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(section_label("Repositories", cx))
                            .child(
                                div()
                                    .px_2()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(repo_section_label(
                                        connected,
                                        repos_loaded,
                                        repositories.len(),
                                        &workspaces,
                                        selected_workspace.as_ref(),
                                    )),
                            )
                            .when(connected && selected_workspace.is_some() && repositories.is_empty(), |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No repositories yet. Add an existing local repo."),
                                )
                            })
                            .children(repositories.iter().cloned().map(|repo| {
                                let alias = repo.title.clone();
                                let path = repo.path.display().to_string();
                                let enabled = if repo.default_enabled {
                                    "default"
                                } else {
                                    "not default"
                                };
                                div()
                                    .id(SharedString::from(format!("repo-{}", repo.id)))
                                    .px_2()
                                    .py_1()
                                    .child(
                                        v_flex()
                                            .gap_0()
                                            .child(div().text_sm().child(alias))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(path),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(enabled),
                                            ),
                                    )
                            }))
                            .child(
                                div()
                                    .px_2()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Existing local repo. Remote URLs are not cloned."),
                            )
                            .child(Input::new(&self.repo_path).id("repo-path"))
                            .child(
                                h_flex().gap_1().items_center().child(
                                    Button::new("choose-repo-dir")
                                        .ghost()
                                        .label("Choose folder")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_folder(cx);
                                        })),
                                ),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Input::new(&self.repo_alias).id("repo-alias"),
                                        ),
                                    )
                                    .child(
                                        Button::new("add-repo")
                                            .ghost()
                                            .label("Add repo")
                                            .disabled(!can_repo)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit_repository(window, cx);
                                            })),
                                    ),
                            )
                            .children(
                                repo_feedback
                                    .clone()
                                    .map(|feedback| feedback_line(&feedback, cx).into_any_element()),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(section_label("Threads", cx))
                            .when(!connected, |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Connect the local daemon to browse threads"),
                                )
                            })
                            .when(connected && threads.is_empty(), |this| {
                                this.child(
                                    div()
                                        .px_2()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No threads yet"),
                                )
                            })
                            .children(threads.iter().cloned().map(|thread| {
                                let selected = selected_thread.as_ref() == Some(&thread.id);
                                let id = thread.id.clone();
                                let title = thread.title.clone();
                                div()
                                    .id(SharedString::from(format!("th-{}", thread.id)))
                                    .pl_2()
                                    .px_2()
                                    .py_1()
                                    .rounded(cx.theme().radius)
                                    .cursor_pointer()
                                    .when(selected, |this| {
                                        this.bg(cx.theme().sidebar_accent)
                                            .text_color(cx.theme().sidebar_accent_foreground)
                                    })
                                    .when(!selected, |this| {
                                        this.hover(|s| {
                                            s.bg(cx.theme().sidebar_accent.opacity(0.45))
                                        })
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let requests = this.onboarding.select_thread(id.clone());
                                        this.emit_requests(requests, cx);
                                    }))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Icon::new(IconName::MessageSquare)
                                                    .with_size(px(12.)),
                                            )
                                            .child(div().text_sm().child(title)),
                                    )
                            }))
                            .child(
                                div()
                                    .px_2()
                                    .pt_2()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Prepare isolated checkout is available for the selected thread even when it is not marked legacy."),
                            )
                            .child(
                                Button::new("prepare-isolated-checkout")
                                    .ghost()
                                    .label("Prepare isolated checkout")
                                    .disabled(!can_prepare)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_prepare(window, cx);
                                    })),
                            )
                            .children(migration_feedback.clone().map(|feedback| {
                                feedback_line(&feedback, cx).into_any_element()
                            }))
                            .when(connected, |this| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .pt_1()
                                        .child(
                                            div().flex_1().min_w_0().child(
                                                Input::new(&self.new_thread).id("new-thread"),
                                            ),
                                        )
                                        .child(
                                            Button::new("create-thread")
                                                .ghost()
                                                .label("New")
                                                .disabled(!can_thread)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.submit_thread(window, cx);
                                                })),
                                        ),
                                )
                            })
                            .children(thread_feedback.clone().map(|feedback| {
                                feedback_line(&feedback, cx).into_any_element()
                            })),
                    ),
            )
    }
}

fn feedback_line(feedback: &Feedback, cx: &App) -> impl IntoElement {
    let color = match feedback {
        Feedback::Progress(_) => cx.theme().warning,
        Feedback::Error(_) => cx.theme().danger,
        Feedback::Notice(_) => cx.theme().success,
    };
    div()
        .px_2()
        .text_xs()
        .text_color(color)
        .child(feedback.text().to_string())
}

fn repo_section_label(
    connected: bool,
    loaded: bool,
    loaded_count: usize,
    workspaces: &[WorkspaceSnapshot],
    selected: Option<&WorkspaceId>,
) -> String {
    if !connected {
        return "Repository data unavailable".to_string();
    }
    let Some(id) = selected else {
        return "Select a workspace".to_string();
    };
    if loaded {
        return format!("{loaded_count} attached");
    }
    let count = workspaces
        .iter()
        .find(|workspace| &workspace.id == id)
        .map(|workspace| workspace.repo_count)
        .unwrap_or(0);
    format!("{count} attached")
}

fn section_label(label: &'static str, cx: &App) -> impl IntoElement {
    div()
        .px_2()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

impl EventEmitter<SidebarEvent> for SidebarPanel {}
