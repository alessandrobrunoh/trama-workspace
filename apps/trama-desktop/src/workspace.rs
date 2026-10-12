use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    kbd::Kbd,
    notification::Notification,
    theme::ThemeMode,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::agent::ApprovalDecision;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{ClientMessage, DaemonMessage, LandOutcome, SubthreadKind, ThreadKind};

use crate::changes::{ChangesEvent, ChangesPanel, DrawerTab, render_drawer};
use crate::chat::ChatPanel;
use crate::comments::CommentStore;
use crate::copies::CopyPanel;
use crate::keymap::{self, CONTEXT, fuzzy_rank};
use crate::net::{DaemonClient, NetEvent};
use crate::onboarding::{Link, OnboardingRequest};
use crate::projects::{NewThreadEvent, NewThreadView, ProjectsEvent, ProjectsPanel};
use crate::sidebar::{SidebarEvent, SidebarPanel, status_dot};
use crate::startup::{DaemonLocate, DaemonStart};
use crate::subthreads::{
    PullSummary, SubthreadBar, land_label, subthread_parent, title_from_prompt,
};
use crate::theme::{self, Palette};

/// What a palette row does when confirmed.
#[derive(Debug, Clone, PartialEq)]
enum PaletteTarget {
    Thread(ThreadId),
    Workspace(WorkspaceId),
    NewThread,
    ToggleDrawer,
    ManageProjects,
    PrepareCheckout,
    ToggleAppearance,
    Shortcuts,
    Review,
    Land,
    PullChanges,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    chat: Entity<ChatPanel>,
    copies: Entity<CopyPanel>,
    sidebar: Entity<SidebarPanel>,
    projects: Entity<ProjectsPanel>,
    new_thread: Entity<NewThreadView>,
    changes: Entity<ChangesPanel>,
    comments: Entity<CommentStore>,
    subthread_bar: Entity<SubthreadBar>,
    palette: Entity<CommandState>,
    palette_query: String,
    drawer_open: bool,
    drawer_tab: DrawerTab,
    /// `ChatPanel::completed_turns` last seen; a change refreshes Changes.
    seen_completed_turns: u64,
    /// Subthread this window asked for, to follow it once created.
    pending_subthread: Option<(ThreadId, SubthreadKind)>,
    client: DaemonClient,
    connected: bool,
    _subscriptions: Vec<Subscription>,
    _net_events: Task<()>,
    _daemon_start: Option<Task<()>>,
    dev: Option<crate::devhooks::DevScript>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("TramaWorkspace");

        let (client, events) = crate::net::start();
        let comments = cx.new(|_| CommentStore::new(client.clone()));
        let chat_client = client.clone();
        let chat_comments = comments.clone();
        let chat = cx.new(|cx| ChatPanel::new(window, cx, chat_client, chat_comments));
        let copies = cx.new(|_| CopyPanel::new());
        let sidebar = cx.new(|_| SidebarPanel::new());
        let projects = cx.new(|cx| ProjectsPanel::new(sidebar.clone(), window, cx));
        let new_thread = cx.new(|cx| NewThreadView::new(sidebar.clone(), window, cx));
        let changes_comments = comments.clone();
        let changes = cx.new(|cx| ChangesPanel::new(changes_comments, cx));
        let bar_client = client.clone();
        let bar_sidebar = sidebar.clone();
        let subthread_bar = cx.new(|cx| SubthreadBar::new(bar_client, bar_sidebar, window, cx));
        let palette = cx.new(|cx| CommandState::new(window, cx));

        let subscriptions = vec![
            cx.observe(&chat, |this, chat, cx| {
                let status = chat.read(cx).thread_status();
                this.sidebar.update(cx, |sidebar, cx| {
                    if sidebar.selected_status != status {
                        sidebar.selected_status = status;
                        cx.notify();
                    }
                });
                let turns = chat.read(cx).completed_turns;
                if turns != this.seen_completed_turns {
                    this.seen_completed_turns = turns;
                    this.refresh_changes(cx);
                }
                cx.notify();
            }),
            cx.subscribe(&changes, |this, _, event, cx| match event {
                ChangesEvent::Refresh => this.refresh_changes(cx),
                ChangesEvent::RevealMessage(message_id) => {
                    let message_id = message_id.clone();
                    this.chat
                        .update(cx, |chat, cx| chat.reveal_message(message_id, cx));
                }
            }),
            cx.observe(&subthread_bar, |_, _, cx| cx.notify()),
            cx.observe(&sidebar, |_, _, cx| cx.notify()),
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| {
                this.on_sidebar_event(event, window, cx);
            }),
            cx.subscribe(&projects, |this, _, event, cx| match event {
                ProjectsEvent::StartDaemon => this.start_daemon(cx),
            }),
            cx.subscribe_in(&new_thread, window, |this, _, event, window, cx| {
                window.close_dialog(cx);
                if let NewThreadEvent::ManageProjects = event {
                    this.open_projects(window, cx);
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                if crate::devhooks::appearance().is_none() {
                    theme::apply(theme::mode_for(window.appearance()), cx);
                }
            }),
        ];

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

        if let Some(dev) = crate::devhooks::from_env() {
            if let Some((label, diff)) = &dev.diff {
                changes.update(cx, |changes, cx| changes.set_diff(label.clone(), diff, cx));
            }
        }

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            focus_handle,
            chat,
            copies,
            sidebar,
            projects,
            new_thread,
            changes,
            comments,
            subthread_bar,
            palette,
            palette_query: String::new(),
            drawer_open: crate::devhooks::from_env().is_some_and(|dev| dev.drawer),
            drawer_tab: DrawerTab::default(),
            seen_completed_turns: 0,
            pending_subthread: None,
            client,
            connected: false,
            _subscriptions: subscriptions,
            _net_events: net_events,
            _daemon_start: None,
            dev: crate::devhooks::from_env(),
        }
    }

    /// Ask for the selected thread's changes when the Changes tab is
    /// visible. Called on drawer open, thread switch, base change, and turn
    /// completion.
    fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        if !self.drawer_open || self.drawer_tab != DrawerTab::Changes || !self.connected {
            return;
        }
        let Some(thread_id) = self.sidebar.read(cx).onboarding.selected_thread.clone() else {
            self.changes.update(cx, |changes, cx| changes.clear(cx));
            return;
        };
        let base = self.changes.read(cx).base();
        let message = ClientMessage::GetChanges {
            thread_id: thread_id.clone(),
            base,
        };
        if self.client.send(message) == crate::net::SendStatus::Enqueued {
            self.changes
                .update(cx, |changes, cx| changes.begin_loading(thread_id, cx));
        }
    }

    fn selected_thread_kind(&self, cx: &App) -> Option<ThreadKind> {
        let onboarding = &self.sidebar.read(cx).onboarding;
        let id = onboarding.selected_thread.as_ref()?;
        onboarding.thread(id).map(|thread| thread.kind)
    }

    /// Fork a subthread from the selected thread (or its parent).
    fn create_subthread(
        &mut self,
        kind: SubthreadKind,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let parent = {
            let onboarding = &self.sidebar.read(cx).onboarding;
            onboarding
                .selected_thread
                .as_ref()
                .and_then(|selected| subthread_parent(&onboarding.threads, selected))
        };
        let Some(parent) = parent else {
            return false;
        };
        let title = initial_prompt
            .as_deref()
            .map(title_from_prompt)
            .filter(|title| !title.is_empty());
        let message = ClientMessage::CreateSubthread {
            parent_thread_id: parent.clone(),
            kind,
            title,
            initial_prompt,
        };
        if self.client.send(message) == crate::net::SendStatus::Rejected {
            window.push_notification(
                Notification::error("The daemon is not connected; no subthread was created."),
                cx,
            );
            return false;
        }
        self.pending_subthread = Some((parent, kind));
        true
    }

    /// Delta's "send to subthread": the composer draft becomes the first
    /// prompt of a new plain subthread; this thread stays selected.
    fn send_to_subthread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        let Some(thread_id) = self.sidebar.read(cx).onboarding.selected_thread.clone() else {
            return;
        };
        let Some(draft) = self.chat.update(cx, |chat, cx| chat.take_draft(window, cx)) else {
            window.push_notification(
                Notification::info("Write a prompt first, then press \u{2318}\u{21e7}Enter."),
                cx,
            );
            return;
        };
        if !self.create_subthread(SubthreadKind::Plain, Some(draft.clone()), window, cx) {
            self.chat.update(cx, |chat, cx| {
                chat.restore_prompt(&thread_id, &draft, window, cx);
            });
        }
    }

    fn start_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_thread_kind(cx) == Some(ThreadKind::Top) {
            self.create_subthread(SubthreadKind::Review, None, window, cx);
        }
    }

    fn confirm_land(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_thread_kind(cx) != Some(ThreadKind::Top) {
            return;
        }
        let workspace = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            alert
                .confirm()
                .title("Land this thread?")
                .description(
                    "A land subthread starts from a copy of this thread and follows the repository's land skill, which may commit, push or merge. Without a land skill the agent proposes one instead of landing.",
                )
                .ok_text("Start land")
                .cancel_text("Cancel")
                .on_ok(move |_, window, cx| {
                    let _ = workspace.update(cx, |this, cx| {
                        this.create_subthread(SubthreadKind::Land, None, window, cx);
                    });
                    true
                })
        });
    }

    fn pull_selected(&mut self, cx: &mut Context<Self>) {
        let onboarding = &self.sidebar.read(cx).onboarding;
        let Some(id) = onboarding.selected_thread.clone() else {
            return;
        };
        if onboarding
            .thread(&id)
            .is_some_and(|thread| thread.parent_thread_id.is_some())
        {
            self.subthread_bar.update(cx, |bar, cx| bar.pull(id, cx));
        }
    }

    /// ⌘⌥M: the picked diff lines when the drawer has some, else the
    /// selected transcript text.
    fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drawer_open && self.changes.read(cx).has_selection() {
            self.changes
                .update(cx, |changes, cx| changes.comment_on_selection(window, cx));
        } else {
            self.chat
                .update(cx, |chat, cx| chat.comment_on_selection(window, cx));
        }
    }

    fn answer_approval(&mut self, decision: ApprovalDecision, cx: &mut Context<Self>) {
        self.chat
            .update(cx, |chat, cx| chat.answer_oldest_approval(decision, cx));
    }

    fn on_sidebar_event(
        &mut self,
        event: &SidebarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::Requests(requests) => {
                for request in requests.clone() {
                    self.dispatch_request(request, cx);
                }
            }
            SidebarEvent::NewThread => self.open_new_thread(window, cx),
            SidebarEvent::ManageProjects => self.open_projects(window, cx),
            SidebarEvent::QuickSwitch => self.open_palette(window, cx),
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
        self.copies
            .update(cx, |copies, cx| copies.select(thread_id.clone(), cx));
        self.subthread_bar.update(cx, |bar, cx| bar.reset_form(cx));
        // Reset even for the same thread: selection resends ListComments.
        self.comments.update(cx, |comments, cx| {
            comments.set_thread(None, cx);
            comments.set_thread(thread_id.clone(), cx);
        });
        self.chat.update(cx, |chat, cx| {
            chat.set_thread(None, cx);
            chat.set_thread(thread_id, cx);
            chat.model = None;
            chat.usage = None;
            chat.cost_usd_micros = None;
            chat.turns = None;
        });
        self.refresh_changes(cx);
    }

    // -- overlays ---------------------------------------------------------

    fn open_new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        if self
            .sidebar
            .read(cx)
            .onboarding
            .selected_workspace
            .is_none()
        {
            self.open_projects(window, cx);
            return;
        }
        window.close_all_dialogs(cx);
        self.new_thread
            .update(cx, |view, cx| view.reset(window, cx));
        let view = self.new_thread.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("New thread").w(px(520.)).child(view.clone())
        });
    }

    fn open_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_all_dialogs(cx);
        let view = self.projects.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Manage projects")
                .w(px(600.))
                .child(view.clone())
        });
        self.projects
            .update(cx, |projects, cx| projects.focus_first(window, cx));
    }

    fn open_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_all_dialogs(cx);
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = theme::palette(cx);
            dialog.title("Keyboard shortcuts").w(px(460.)).child(
                v_flex()
                    .gap(px(2.))
                    .pb(theme::sp(1.))
                    .children(keymap::SHORTCUTS.iter().map(|shortcut| {
                        h_flex()
                            .h(px(30.))
                            .items_center()
                            .text_size(theme::text_ui())
                            .text_color(p.text)
                            .child(div().flex_1().child(shortcut.label))
                            .children(
                                shortcut
                                    .keys
                                    .iter()
                                    .filter_map(|keys| Keystroke::parse(keys).ok().map(Kbd::new)),
                            )
                    })),
            )
        });
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_all_dialogs(cx);
        }
        self.palette_query.clear();
        self.palette
            .update(cx, |state, cx| state.set_query("", window, cx));
        let workspace = cx.weak_entity();
        let state = self.palette.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(this) = workspace.upgrade() else {
                return dialog;
            };
            let (groups, targets) = this.read(cx).palette_groups(cx);
            let on_query = workspace.clone();
            let on_confirm = workspace.clone();
            let mut command = Command::new(&state)
                .bordered(false)
                .filterable(false)
                .placeholder("Jump to a thread or run a command…")
                .max_h(px(380.))
                .on_query(move |query, _, cx| {
                    let _ = on_query.update(cx, |this, cx| {
                        this.palette_query = query.to_string();
                        cx.notify();
                    });
                })
                .on_confirm(move |index, window, cx| {
                    let target = targets
                        .get(index.section)
                        .and_then(|group| group.get(index.row))
                        .cloned();
                    window.close_dialog(cx);
                    if let Some(target) = target {
                        let workspace = on_confirm.clone();
                        window.defer(cx, move |window, cx| {
                            let _ = workspace.update(cx, |this, cx| {
                                this.run_palette_target(target, window, cx);
                            });
                        });
                    }
                })
                .empty(|_, _, cx| {
                    let p = theme::palette(cx);
                    div()
                        .p(theme::sp(4.))
                        .text_size(theme::text_ui())
                        .text_color(p.text_faint)
                        .child("No matching threads or commands")
                });
            for group in groups {
                command = command.group(group);
            }
            dialog
                .close_button(false)
                .w(px(560.))
                .margin_top(px(96.))
                .p_0()
                .child(command)
        });
        self.palette.update(cx, |state, cx| state.focus(window, cx));
    }

    fn palette_groups(&self, cx: &App) -> (Vec<CommandGroup>, Vec<Vec<PaletteTarget>>) {
        let query = self.palette_query.trim().to_string();
        let sidebar = self.sidebar.read(cx);
        let onboarding = &sidebar.onboarding;
        let selected_thread = onboarding.selected_thread.clone();
        let selected_workspace = onboarding.selected_workspace.clone();
        let connected = onboarding.connected();
        let mut groups = Vec::new();
        let mut targets = Vec::new();

        let thread_titles: Vec<&str> = onboarding
            .threads
            .iter()
            .map(|thread| thread.title.as_str())
            .collect();
        let ranked = fuzzy_rank(&query, thread_titles.iter().copied());
        if !ranked.is_empty() {
            let mut group = CommandGroup::new().label("Threads");
            let mut rows = Vec::new();
            for index in ranked {
                let thread = &onboarding.threads[index];
                group = group.item(
                    CommandItem::new()
                        .label(thread.title.clone())
                        .icon(IconName::MessageSquare)
                        .checked(selected_thread.as_ref() == Some(&thread.id)),
                );
                rows.push(PaletteTarget::Thread(thread.id.clone()));
            }
            groups.push(group);
            targets.push(rows);
        }

        let workspace_titles: Vec<&str> = onboarding
            .workspaces
            .iter()
            .map(|workspace| workspace.title.as_str())
            .collect();
        let ranked = fuzzy_rank(&query, workspace_titles.iter().copied());
        if !ranked.is_empty() && onboarding.workspaces.len() > 1 {
            let mut group = CommandGroup::new().label("Projects");
            let mut rows = Vec::new();
            for index in ranked {
                let workspace = &onboarding.workspaces[index];
                group = group.item(
                    CommandItem::new()
                        .label(workspace.title.clone())
                        .icon(IconName::FolderGit2)
                        .checked(selected_workspace.as_ref() == Some(&workspace.id)),
                );
                rows.push(PaletteTarget::Workspace(workspace.id.clone()));
            }
            groups.push(group);
            targets.push(rows);
        }

        let commands: Vec<(&str, IconName, PaletteTarget, bool)> = vec![
            (
                "New thread",
                IconName::SquarePen,
                PaletteTarget::NewThread,
                connected,
            ),
            (
                if self.drawer_open {
                    "Hide changes drawer"
                } else {
                    "Show changes drawer"
                },
                IconName::FileDiff,
                PaletteTarget::ToggleDrawer,
                true,
            ),
            (
                "Manage projects",
                IconName::Settings,
                PaletteTarget::ManageProjects,
                true,
            ),
            (
                "Review this thread in a subthread",
                IconName::MessageSquareDiff,
                PaletteTarget::Review,
                connected && self.selected_thread_kind(cx) == Some(ThreadKind::Top),
            ),
            (
                "Land this thread",
                IconName::PlaneLanding,
                PaletteTarget::Land,
                connected && self.selected_thread_kind(cx) == Some(ThreadKind::Top),
            ),
            (
                "Pull subthread changes into parent",
                IconName::GitMerge,
                PaletteTarget::PullChanges,
                connected
                    && self
                        .selected_thread_kind(cx)
                        .is_some_and(|kind| kind != ThreadKind::Top),
            ),
            (
                "Prepare isolated checkout for this thread",
                IconName::GitBranch,
                PaletteTarget::PrepareCheckout,
                onboarding.can_prepare_checkout(),
            ),
            (
                if cx.theme_is_dark() {
                    "Switch to light appearance"
                } else {
                    "Switch to dark appearance"
                },
                if cx.theme_is_dark() {
                    IconName::Sun
                } else {
                    IconName::Moon
                },
                PaletteTarget::ToggleAppearance,
                true,
            ),
            (
                "Keyboard shortcuts",
                IconName::Keyboard,
                PaletteTarget::Shortcuts,
                true,
            ),
        ];
        let available: Vec<_> = commands.into_iter().filter(|c| c.3).collect();
        let ranked = fuzzy_rank(&query, available.iter().map(|c| c.0));
        if !ranked.is_empty() {
            let mut group = CommandGroup::new().label("Commands");
            let mut rows = Vec::new();
            for index in ranked {
                let (label, icon, target, _) = &available[index];
                group = group.item(CommandItem::new().label(label.to_string()).icon(*icon));
                rows.push(target.clone());
            }
            groups.push(group);
            targets.push(rows);
        }
        (groups, targets)
    }

    fn run_palette_target(
        &mut self,
        target: PaletteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match target {
            PaletteTarget::Thread(id) => {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_thread(id, cx));
            }
            PaletteTarget::Workspace(id) => {
                let requests = self.sidebar.update(cx, |sidebar, cx| {
                    let requests = sidebar.onboarding.select_workspace(id);
                    cx.notify();
                    requests
                });
                for request in requests {
                    self.dispatch_request(request, cx);
                }
            }
            PaletteTarget::NewThread => self.open_new_thread(window, cx),
            PaletteTarget::ToggleDrawer => self.toggle_drawer(cx),
            PaletteTarget::ManageProjects => self.open_projects(window, cx),
            PaletteTarget::PrepareCheckout => {
                crate::projects::open_prepare_confirm(self.sidebar.clone(), window, cx);
            }
            PaletteTarget::ToggleAppearance => self.toggle_appearance(cx),
            PaletteTarget::Shortcuts => self.open_shortcuts(window, cx),
            PaletteTarget::Review => self.start_review(window, cx),
            PaletteTarget::Land => self.confirm_land(window, cx),
            PaletteTarget::PullChanges => self.pull_selected(cx),
        }
        cx.notify();
    }

    fn toggle_drawer(&mut self, cx: &mut Context<Self>) {
        self.drawer_open = !self.drawer_open;
        self.refresh_changes(cx);
        cx.notify();
    }

    fn toggle_appearance(&mut self, cx: &mut Context<Self>) {
        let mode = if cx.theme_is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        theme::apply(mode, cx);
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        } else if self.drawer_open {
            self.drawer_open = false;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    // -- daemon -----------------------------------------------------------

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
            NetEvent::Message(message) => self.on_daemon_message(message, window, cx),
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

    fn on_daemon_message(
        &mut self,
        message: DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .comments
            .update(cx, |comments, cx| comments.apply(&message, cx))
        {
            return;
        }
        match message {
            DaemonMessage::ThreadCheckouts {
                thread_id,
                checkouts,
            } => {
                self.copies
                    .update(cx, |copies, cx| copies.apply(&thread_id, checkouts, cx));
            }
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
                self.run_dev_script(window, cx);
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
                self.chat
                    .update(cx, |chat, cx| chat.focus_composer(window, cx));
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
            DaemonMessage::SubthreadCreated(snapshot) => {
                let ours = self
                    .pending_subthread
                    .as_ref()
                    .is_some_and(|(parent, kind)| {
                        snapshot.parent_thread_id.as_ref() == Some(parent)
                            && kind.thread_kind() == snapshot.kind
                    });
                let id = snapshot.id.clone();
                let kind = snapshot.kind;
                let title = snapshot.title.clone();
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.apply_subthread(snapshot);
                    cx.notify();
                });
                if ours {
                    self.pending_subthread = None;
                    if kind == ThreadKind::Sub {
                        window.push_notification(
                            Notification::success(format!(
                                "Started subthread \u{201c}{title}\u{201d}."
                            ))
                            .title("Sent to subthread"),
                            cx,
                        );
                    } else {
                        self.sidebar
                            .update(cx, |sidebar, cx| sidebar.select_thread(id, cx));
                    }
                }
            }
            DaemonMessage::Subthreads { threads, .. } => {
                self.sidebar.update(cx, |sidebar, cx| {
                    for thread in threads {
                        sidebar.onboarding.apply_subthread(thread);
                    }
                    cx.notify();
                });
            }
            DaemonMessage::Changes {
                thread_id,
                base,
                repos,
            } => {
                self.changes.update(cx, |changes, cx| {
                    changes.set_changes(&thread_id, base, &repos, cx);
                });
            }
            DaemonMessage::ChangesPulled {
                subthread_id,
                parent_thread_id,
                repos,
            } => {
                let summary = PullSummary::from_repos(&repos);
                let note = if summary.conflicts > 0 {
                    Notification::warning(summary.text())
                } else {
                    Notification::success(summary.text())
                };
                window.push_notification(note.title("Pulled changes into parent"), cx);
                self.subthread_bar.update(cx, |bar, cx| {
                    bar.apply_pull(subthread_id.clone(), repos, cx);
                });
                let selected = self.sidebar.read(cx).onboarding.selected_thread.clone();
                if selected == Some(parent_thread_id) || selected == Some(subthread_id) {
                    self.refresh_changes(cx);
                }
            }
            DaemonMessage::ReviewVerdictPosted {
                review_thread_id,
                verdict,
                ..
            } => {
                let approved = matches!(verdict, trama_protocol::ReviewVerdict::Approve);
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar
                        .onboarding
                        .set_review_verdict(&review_thread_id, verdict);
                    cx.notify();
                });
                self.subthread_bar
                    .update(cx, |bar, cx| bar.apply_verdict(&review_thread_id, cx));
                window.push_notification(
                    if approved {
                        Notification::success("Review approved; posted to the parent thread.")
                    } else {
                        Notification::warning("Changes requested; posted to the parent thread.")
                    },
                    cx,
                );
            }
            DaemonMessage::LandResult {
                land_thread_id,
                outcome,
                summary,
                ..
            } => {
                let note = match outcome {
                    LandOutcome::Landed => Notification::success(summary.clone()),
                    LandOutcome::Failed => Notification::error(summary.clone()),
                    LandOutcome::Proposed => Notification::warning(summary.clone()),
                };
                window.push_notification(note.title(land_label(outcome)), cx);
                self.subthread_bar.update(cx, |bar, cx| {
                    bar.apply_land(land_thread_id, outcome, summary, cx);
                });
            }
            DaemonMessage::Error { message, .. }
                if self
                    .changes
                    .update(cx, |changes, cx| changes.fail_loading(&message, cx)) =>
            {
                // The pending GetChanges failed (e.g. no last turn yet).
            }
            DaemonMessage::Error { code, message } => {
                self.pending_subthread = None;
                self.subthread_bar.update(cx, |bar, cx| bar.note_error(cx));
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

    fn run_dev_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dev) = self.dev.take() else {
            return;
        };
        if let Some(needle) = &dev.select_thread {
            let found = self
                .sidebar
                .read(cx)
                .onboarding
                .threads
                .iter()
                .find(|thread| thread.title.contains(needle.as_str()))
                .map(|thread| thread.id.clone());
            if let Some(id) = found {
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_thread(id, cx));
            }
        }
        if let Some(index) = dev.expand_tool {
            let chat = self.chat.clone();
            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(800))
                    .await;
                let _ = chat.update(cx, |chat, cx| chat.expand_tool_call(index, cx));
            })
            .detach();
        }
        match dev.overlay.as_deref() {
            Some(overlay) if overlay.starts_with("palette") => {
                self.open_palette(window, cx);
                if let Some(query) = overlay.strip_prefix("palette:") {
                    self.palette_query = query.to_string();
                    self.palette.update(cx, |state, cx| {
                        state.set_query(query.to_string(), window, cx)
                    });
                }
            }
            Some("projects") => self.open_projects(window, cx),
            Some("new-thread") => self.open_new_thread(window, cx),
            Some("shortcuts") => self.open_shortcuts(window, cx),
            _ => {}
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

    // -- chrome -----------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let sidebar = self.sidebar.read(cx);
        let link = sidebar.onboarding.link().clone();
        let thread_title = sidebar.selected_thread_title();
        let project = sidebar.selected_workspace_title();
        let repos: Vec<String> = sidebar
            .onboarding
            .repositories
            .iter()
            .filter(|repo| repo.default_enabled)
            .map(|repo| repo.title.clone())
            .collect();
        let status = sidebar.selected_status;
        let (link_color, link_label): (Hsla, SharedString) = match &link {
            Link::Online => (p.success, "Daemon connected".into()),
            Link::Offline => (p.warning, "Daemon offline, reconnecting".into()),
            Link::ProtocolMismatch { daemon_protocol } => (
                p.danger,
                format!("Protocol mismatch (daemon v{daemon_protocol}); restart both").into(),
            ),
        };
        let drawer_open = self.drawer_open;
        let can_fork =
            link == Link::Online && self.selected_thread_kind(cx) == Some(ThreadKind::Top);

        TitleBar::new()
            .h(theme::title_bar_height())
            .bg(p.panel)
            .border_color(p.border)
            .child(
                h_flex()
                    .size_full()
                    .items_center()
                    .child(
                        // Sidebar column: traffic lights live here.
                        div().w(theme::sidebar_width() - px(80.)).flex_shrink_0(),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(theme::sp(2.))
                            .px(theme::sp(3.))
                            .items_center()
                            .text_size(theme::text_ui())
                            .when_some(thread_title.clone(), |this, _| {
                                this.child(status_dot(status, &p))
                            })
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(if thread_title.is_some() {
                                        p.text
                                    } else {
                                        p.text_faint
                                    })
                                    .child(thread_title.unwrap_or_else(|| "No thread".into())),
                            )
                            .when_some(project, |this, project| {
                                this.child(
                                    h_flex()
                                        .flex_shrink_0()
                                        .gap(theme::sp(1.5))
                                        .items_center()
                                        .text_size(theme::text_sm())
                                        .text_color(p.text_faint)
                                        .child("/")
                                        .child(Icon::new(IconName::FolderGit2).with_size(px(12.)))
                                        .child(project)
                                        .when(!repos.is_empty(), |this| {
                                            this.child(
                                                Icon::new(IconName::GitBranch).with_size(px(12.)),
                                            )
                                            .child(repos.join(", "))
                                        }),
                                )
                            }),
                    )
                    .child(
                        // Collaborator avatars will go here once sharing exists.
                        h_flex().id("collaborators").flex_shrink_0(),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px(theme::sp(2.))
                            .gap(theme::sp(1.))
                            .items_center()
                            .when(can_fork, |this| {
                                this.child(
                                    Button::new("titlebar-review")
                                        .ghost()
                                        .small()
                                        .icon(IconName::MessageSquareDiff)
                                        .label("Review")
                                        .tooltip("Start a review subthread for this thread's changes")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.start_review(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("titlebar-land")
                                        .ghost()
                                        .small()
                                        .icon(IconName::PlaneLanding)
                                        .label("Land")
                                        .tooltip("Land this thread's changes with the repository's land skill")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm_land(window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("titlebar-palette")
                                    .ghost()
                                    .small()
                                    .icon(IconName::Search)
                                    .tooltip("Jump to thread (\u{2318}K)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_palette(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("titlebar-drawer")
                                    .ghost()
                                    .small()
                                    .icon(if drawer_open {
                                        IconName::PanelRightClose
                                    } else {
                                        IconName::PanelRightOpen
                                    })
                                    .toggled(drawer_open)
                                    .tooltip("Changes and copies (\u{2318}R)")
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_drawer(cx))),
                            )
                            .child(
                                div()
                                    .id("titlebar-link")
                                    .p(theme::sp(2.))
                                    .child(div().size(px(7.)).rounded_full().bg(link_color))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(link_label.clone()).build(window, cx)
                                    }),
                            ),
                    ),
            )
    }

    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        let p = theme::palette(cx);
        let sidebar = self.sidebar.read(cx);
        let onboarding = &sidebar.onboarding;
        let (color, label) = match onboarding.link() {
            Link::Online => (p.success, "Connected"),
            Link::Offline => (p.warning, "Offline"),
            Link::ProtocolMismatch { .. } => (p.danger, "Restart required"),
        };
        let counts = if onboarding.connected() {
            format!(
                "{} project{} · {} thread{}",
                onboarding.workspaces.len(),
                if onboarding.workspaces.len() == 1 {
                    ""
                } else {
                    "s"
                },
                onboarding.threads.len(),
                if onboarding.threads.len() == 1 {
                    ""
                } else {
                    "s"
                },
            )
        } else {
            String::new()
        };
        h_flex()
            .h(theme::status_bar_height())
            .flex_shrink_0()
            .px(theme::sp(3.))
            .gap(theme::sp(3.))
            .items_center()
            .bg(p.panel)
            .border_t_1()
            .border_color(p.border)
            .text_size(theme::text_xs())
            .text_color(p.text_faint)
            .child(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .child(div().size(px(6.)).rounded_full().bg(color))
                    .child(label),
            )
            .child(counts)
            .child(div().flex_1())
            .child("\u{2318}K jump · \u{2318}N new · \u{2318}\u{21e7}\u{23ce} subthread · \u{2318}R changes · \u{2318}/ shortcuts")
    }
}

trait ThemeIsDark {
    fn theme_is_dark(&self) -> bool;
}

impl ThemeIsDark for App {
    fn theme_is_dark(&self) -> bool {
        use gpui_kit::component::ActiveTheme as _;
        self.theme().is_dark()
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let link = self.sidebar.read(cx).onboarding.link().clone();
        let alert = self.sidebar.read(cx).onboarding.alert().map(str::to_string);
        let can_start = self.sidebar.read(cx).onboarding.can_start_daemon();
        let daemon_feedback = self.sidebar.read(cx).onboarding.daemon_feedback.clone();

        let center = if matches!(link, Link::Online) {
            v_flex()
                .size_full()
                .child(self.subthread_bar.clone())
                .child(div().flex_1().min_h_0().child(self.chat.clone()))
                .into_any_element()
        } else {
            connection_panel(
                &p,
                matches!(link, Link::ProtocolMismatch { .. }),
                can_start,
                daemon_feedback.map(|feedback| feedback.text().to_string()),
                cx.listener(|this, _, _, cx| this.start_daemon(cx)),
            )
            .into_any_element()
        };

        v_flex()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &keymap::NewThread, window, cx| {
                this.open_new_thread(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ToggleDrawer, _, cx| {
                this.toggle_drawer(cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::QuickSwitch, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ShowShortcuts, window, cx| {
                this.open_shortcuts(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ManageProjects, window, cx| {
                this.open_projects(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ToggleAppearance, _, cx| {
                this.toggle_appearance(cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::CloseOverlay, window, cx| {
                this.close_overlay(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keymap::SendToSubthread, window, cx| {
                    this.send_to_subthread(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &keymap::AllowOnce, _, cx| {
                this.answer_approval(ApprovalDecision::AllowOnce, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::AllowSession, _, cx| {
                this.answer_approval(ApprovalDecision::AllowSession, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::DenyApproval, _, cx| {
                this.answer_approval(ApprovalDecision::Deny, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keymap::CommentOnSelection, window, cx| {
                    this.comment_on_selection(window, cx);
                }),
            )
            // Text inputs (the composer) handle Escape first; close the
            // drawer on the way down so Esc works with the composer focused.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Escape, window, cx| {
                    // An open diff comment composer takes Escape itself.
                    if this.drawer_open
                        && !window.has_active_dialog(cx)
                        && !this.changes.read(cx).has_draft()
                    {
                        this.drawer_open = false;
                        cx.notify();
                    }
                },
            ))
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .font_family(theme::UI_FONT)
            .text_size(theme::text_ui())
            .child(self.render_title_bar(cx))
            .when_some(alert, |this, alert| {
                this.child(
                    h_flex()
                        .w_full()
                        .flex_shrink_0()
                        .px(theme::sp(3.))
                        .py(theme::sp(1.5))
                        .gap(theme::sp(2.))
                        .items_center()
                        .bg(p.danger.opacity(0.10))
                        .border_b_1()
                        .border_color(p.danger.opacity(0.25))
                        .text_size(theme::text_sm())
                        .text_color(p.text)
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .with_size(px(13.))
                                .text_color(p.danger),
                        )
                        .child(div().flex_1().child(alert)),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(theme::sidebar_width())
                            .h_full()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(p.border)
                            .child(self.sidebar.clone()),
                    )
                    .child(div().flex_1().min_w_0().h_full().child(center))
                    .when(self.drawer_open, |this| {
                        let body: AnyView = match self.drawer_tab {
                            DrawerTab::Changes => self.changes.clone().into(),
                            DrawerTab::Copies => self.copies.clone().into(),
                        };
                        let weak = cx.weak_entity();
                        this.child(render_drawer(
                            self.drawer_tab,
                            body,
                            move |tab, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.drawer_tab = tab;
                                    this.refresh_changes(cx);
                                    cx.notify();
                                });
                            },
                            cx.listener(|this, _, _, cx| this.toggle_drawer(cx)),
                            cx,
                        ))
                    }),
            )
            .child(self.render_status_bar(cx))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn connection_panel(
    p: &Palette,
    mismatch: bool,
    can_start: bool,
    feedback: Option<String>,
    start_daemon: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    v_flex()
        .size_full()
        .bg(p.bg)
        .items_center()
        .justify_center()
        .px(theme::sp(10.))
        .gap(theme::sp(3.))
        .child(
            div()
                .size(px(44.))
                .rounded(theme::radius_lg())
                .border_1()
                .border_color(p.border)
                .bg(p.elevated)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(if mismatch {
                        IconName::TriangleAlert
                    } else {
                        IconName::HardDrive
                    })
                    .with_size(px(20.))
                    .text_color(if mismatch { p.danger } else { p.text_muted }),
                ),
        )
        .child(
            div()
                .text_size(theme::text_title())
                .font_weight(FontWeight::SEMIBOLD)
                .child(if mismatch {
                    "The daemon refused the handshake"
                } else {
                    "The local daemon is not connected"
                }),
        )
        .child(
            div()
                .max_w(px(460.))
                .text_center()
                .text_size(theme::text_ui())
                .line_height(relative(1.5))
                .text_color(p.text_muted)
                .child(if mismatch {
                    "Quit and restart both TramaWorkspace and the daemon. Do not start a second copy. Stored threads are not migrated. Send stays disabled."
                } else {
                    "Send stays disabled until a real daemon accepts the connection. Start one here, or run `cargo run -p trama-daemon` yourself. Nothing is downloaded."
                }),
        )
        .child(
            Button::new("start-daemon-main")
                .primary()
                .label(if mismatch {
                    "Restart required"
                } else {
                    "Start daemon"
                })
                .disabled(mismatch || !can_start)
                .on_click(start_daemon),
        )
        .children(feedback.map(|text| {
            div()
                .max_w(px(460.))
                .text_center()
                .text_size(theme::text_sm())
                .text_color(p.text_faint)
                .child(text)
        }))
}
