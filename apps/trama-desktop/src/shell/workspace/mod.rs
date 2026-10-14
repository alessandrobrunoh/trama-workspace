mod chrome;
mod daemon;
mod panes;
mod render;
mod settings;
mod sharing;
mod wizards;
#[allow(unused_imports)]
pub(crate) use self::chrome::*;
#[allow(unused_imports)]
pub(crate) use self::daemon::*;
#[allow(unused_imports)]
pub(crate) use self::panes::*;
#[allow(unused_imports)]
pub(crate) use self::render::*;
#[allow(unused_imports)]
pub(crate) use self::sharing::*;

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    kbd::Kbd,
    notification::Notification,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::agent::ApprovalDecision;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{ClientMessage, DaemonMessage, LandOutcome, SubthreadKind, ThreadKind};

use crate::app::keymap::{self, CONTEXT, fuzzy_rank};
use crate::app::startup::{DaemonLocate, DaemonStart};
use crate::app::theme::{self, Palette};
use crate::collab::CollabStore;
use crate::collab::account::{AccountView, JoinEvent, JoinView};
use crate::collab::share_dialog::{ShareDialog, ShareDialogEvent};
use crate::collab::wizard::{join_view::JoinWizard, share_view::ShareWizard};
use crate::conversation::chat::{ChatEvent, ChatPanel};
use crate::conversation::subthreads::{
    PullSummary, SubthreadBar, land_label, subthread_parent, title_from_prompt,
};
use crate::review::changes::{ChangesEvent, ChangesPanel, DrawerTab, render_drawer};
use crate::review::comments::CommentStore;
use crate::shell::onboarding::{Link, OnboardingRequest};
use crate::shell::projects::{NewThreadEvent, NewThreadView, ProjectsEvent, ProjectsPanel};
use crate::shell::sidebar::{SidebarEvent, SidebarPanel, status_dot};
use crate::system::copies::CopyPanel;
use crate::system::disk_usage::{DiskUsageEvent, DiskUsageView, RemovalKind};
use crate::system::net::{DaemonClient, NetEvent};

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
    Share,
    JoinShared,
    CollabAccount,
    Settings,
    Providers,
    /// Fork the selected thread at this message ("Fork from here").
    ForkHere(trama_domain::ids::MessageId),
    DiskUsage,
    ArchiveSelected,
    UnarchiveSelected,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    chat: Entity<ChatPanel>,
    /// A subagent's transcript beside the selected thread's.
    pane: crate::shell::side_pane::SidePane,
    pane_chat: Entity<ChatPanel>,
    pane_comments: Entity<CommentStore>,
    copies: Entity<CopyPanel>,
    sidebar: Entity<SidebarPanel>,
    projects: Entity<ProjectsPanel>,
    new_thread: Entity<NewThreadView>,
    changes: Entity<ChangesPanel>,
    comments: Entity<CommentStore>,
    subthread_bar: Entity<SubthreadBar>,
    collab: Entity<CollabStore>,
    share_dialog: Entity<ShareDialog>,
    /// vNext wizards, used when the server supports them.
    share_wizard: Entity<ShareWizard>,
    join_wizard: Entity<JoinWizard>,
    account: Entity<AccountView>,
    /// The Settings view (opened as a large dialog).
    settings: Entity<crate::settings::SettingsView>,
    join: Entity<JoinView>,
    palette: Entity<CommandState>,
    palette_query: String,
    drawer_open: bool,
    drawer_tab: DrawerTab,
    /// `ChatPanel::completed_turns` last seen; a change refreshes Changes.
    seen_completed_turns: u64,
    /// Subthread this window asked for, to follow it once created.
    pending_subthread: Option<(ThreadId, SubthreadKind)>,
    /// The fork-at-a-message dialog.
    fork_view: Entity<crate::conversation::forks::ForkView>,
    /// Parent of a fork this window asked for; the fork opens when created.
    pending_fork: Option<ThreadId>,
    /// "Disk usage" dialog body.
    disk_usage: Entity<DiskUsageView>,
    /// Archive / delete-copy requests in flight, to resend with `force`
    /// after the daemon refuses and the user confirms.
    pending_removals: HashMap<ThreadId, RemovalKind>,
    client: DaemonClient,
    connected: bool,
    _subscriptions: Vec<Subscription>,
    _net_events: Task<()>,
    _daemon_start: Option<Task<()>>,
    dev: Option<crate::app::devhooks::DevScript>,
    /// Showing the scripted demo transcript (debug builds).
    dev_demo: bool,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.set_window_title("TramaWorkspace");

        let (client, events) = crate::system::net::start();
        let comments = cx.new(|_| CommentStore::new(client.clone()));
        let chat_client = client.clone();
        let chat_comments = comments.clone();
        let chat = cx.new(|cx| ChatPanel::new(window, cx, chat_client, chat_comments));
        let pane_comments = cx.new(|_| CommentStore::new(client.clone()));
        let pane_client = client.clone();
        let pane_store = pane_comments.clone();
        let pane_chat = cx.new(|cx| {
            let mut chat = ChatPanel::new(window, cx, pane_client, pane_store);
            chat.set_pane_mode(true);
            chat
        });
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
        let collab_client = client.clone();
        let collab = cx.new(|_| CollabStore::new(collab_client));
        chat.update(cx, |chat, _| chat.set_collab(collab.clone()));
        let share_store = collab.clone();
        let share_dialog = cx.new(|cx| ShareDialog::new(share_store, window, cx));
        let wizard_store = collab.clone();
        let share_wizard = cx.new(|cx| ShareWizard::new(wizard_store, window, cx));
        let wizard_store = collab.clone();
        let join_wizard = cx.new(|cx| JoinWizard::new(wizard_store, window, cx));
        let account_store = collab.clone();
        let account = cx.new(|cx| AccountView::new(account_store, window, cx));
        let settings_client = client.clone();
        let settings_account = account.clone();
        let settings = cx.new(|cx| {
            crate::settings::SettingsView::new(settings_client, settings_account, window, cx)
        });
        let join_store = collab.clone();
        let join = cx.new(|cx| JoinView::new(join_store, window, cx));
        let composer_input = chat.read(cx).composer_input();
        let fork_view = cx.new(|cx| crate::conversation::forks::ForkView::new(window, cx));
        let disk_usage = cx.new(|_| DiskUsageView::new());

        let subscriptions = vec![
            cx.observe_in(&collab, window, |this, _, window, cx| {
                this.resume_deferred(window, cx);
            }),
            cx.observe(&collab, |this, collab, cx| {
                let view = collab.read(cx).sidebar();
                this.sidebar.update(cx, |sidebar, cx| {
                    if sidebar.collab != view {
                        sidebar.collab = view;
                        cx.notify();
                    }
                });
                this.chat.update(cx, |_, cx| cx.notify());
                cx.notify();
            }),
            cx.subscribe(&composer_input, |this, input, event, cx| {
                if let gpui_kit::component::input::InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    this.collab
                        .update(cx, |collab, cx| collab.note_draft(&text, cx));
                }
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                this.collab
                    .update(cx, |collab, cx| collab.set_window_active(active, cx));
            }),
            cx.subscribe_in(
                &share_dialog,
                window,
                |this, _, event, window, cx| match event {
                    ShareDialogEvent::OpenAccount => this.open_account(None, window, cx),
                },
            ),
            cx.subscribe_in(&join, window, |this, _, event, window, cx| match event {
                JoinEvent::OpenAccount(server) => this.open_account(server.clone(), window, cx),
            }),
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
            cx.subscribe_in(
                &chat,
                window,
                |this, _, event: &ChatEvent, window, cx| match event {
                    ChatEvent::OpenThread(thread) => {
                        let thread = thread.clone();
                        this.sidebar
                            .update(cx, |sidebar, cx| sidebar.select_thread(thread, cx));
                    }
                    ChatEvent::OpenInPane(thread) => this.open_pane(thread.clone(), false, cx),
                    ChatEvent::Fork(target) => this.open_fork(target.clone(), window, cx),
                },
            ),
            cx.observe(&pane_chat, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &pane_chat,
                window,
                |this, _, event: &ChatEvent, window, cx| match event {
                    ChatEvent::OpenThread(thread) => {
                        let thread = thread.clone();
                        this.sidebar
                            .update(cx, |sidebar, cx| sidebar.select_thread(thread, cx));
                    }
                    // A subagent's own subagent: stack it in the pane.
                    ChatEvent::OpenInPane(thread) => this.open_pane(thread.clone(), true, cx),
                    ChatEvent::Fork(target) => this.open_fork(target.clone(), window, cx),
                },
            ),
            cx.subscribe_in(
                &fork_view,
                window,
                |this, _, event: &crate::conversation::forks::ForkEvent, window, cx| {
                    window.close_dialog(cx);
                    if let crate::conversation::forks::ForkEvent::Submit(message) = event {
                        this.send_fork(message.clone(), window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &disk_usage,
                window,
                |this, _, event: &DiskUsageEvent, window, cx| match event {
                    DiskUsageEvent::Refresh => this.refresh_disk_usage(cx),
                    DiskUsageEvent::Remove(thread_id, kind) => {
                        this.request_removal(thread_id.clone(), *kind, false, window, cx);
                    }
                },
            ),
            cx.observe(&subthread_bar, |_, _, cx| cx.notify()),
            cx.observe(&sidebar, |this, sidebar, cx| {
                let threads: Vec<_> = sidebar
                    .read(cx)
                    .onboarding
                    .threads
                    .iter()
                    .map(|thread| (thread.id.clone(), thread.title.clone()))
                    .collect();
                let models: Vec<_> = sidebar
                    .read(cx)
                    .onboarding
                    .threads
                    .iter()
                    .map(|thread| (thread.id.clone(), thread.model.clone()))
                    .collect();
                let index = crate::conversation::subagents::ThreadIndex::from_threads(
                    &sidebar.read(cx).onboarding.threads,
                );
                this.chat.update(cx, |chat, cx| {
                    chat.set_known_threads(threads.clone(), cx);
                    chat.set_thread_models(models.clone(), cx);
                    chat.set_thread_index(index.clone(), cx);
                });
                this.pane_chat.update(cx, |chat, cx| {
                    chat.set_known_threads(threads, cx);
                    chat.set_thread_models(models, cx);
                    chat.set_thread_index(index, cx);
                });
                cx.notify();
            }),
            cx.subscribe_in(&sidebar, window, |this, _, event, window, cx| {
                this.on_sidebar_event(event, window, cx);
            }),
            cx.subscribe_in(
                &projects,
                window,
                |this, _, event, window, cx| match event {
                    ProjectsEvent::StartDaemon => this.start_daemon(cx),
                    ProjectsEvent::OpenCollaboration => this.open_account(None, window, cx),
                    ProjectsEvent::OpenDiskUsage => this.open_disk_usage(window, cx),
                },
            ),
            cx.subscribe_in(&new_thread, window, |this, _, event, window, cx| {
                window.close_dialog(cx);
                if let NewThreadEvent::ManageProjects = event {
                    this.open_projects(window, cx);
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| {
                if crate::app::devhooks::appearance().is_none() {
                    theme::apply(
                        theme::resolve_mode(theme::prefs(cx).appearance, window.appearance()),
                        cx,
                    );
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

        let mut demo_pane = crate::shell::side_pane::SidePane::default();
        if let Some(dev) = crate::app::devhooks::from_env() {
            if let Some((label, diff)) = &dev.diff {
                changes.update(cx, |changes, cx| changes.set_diff(label.clone(), diff, cx));
            }
            if dev.demo {
                let parent = ThreadId::from(crate::app::demo::PARENT);
                let index = crate::conversation::subagents::ThreadIndex::from_threads(
                    &crate::app::demo::threads(),
                );
                sidebar.update(cx, |sidebar, _| {
                    sidebar
                        .onboarding
                        .apply_threads(crate::app::demo::threads());
                });
                comments.update(cx, |comments, _| {
                    comments.load_demo(parent.clone(), crate::app::demo::parent_comments());
                });
                chat.update(cx, |chat, cx| {
                    chat.set_thread_index(index.clone(), cx);
                    chat.load_demo(parent, crate::app::demo::parent_entries(), Vec::new(), cx);
                });
                if dev.pane {
                    let child = ThreadId::from(crate::app::demo::CHILD);
                    demo_pane.open(child.clone(), None);
                    pane_chat.update(cx, |chat, cx| {
                        chat.set_thread_index(index, cx);
                        chat.load_demo(
                            child,
                            crate::app::demo::child_entries(),
                            crate::app::demo::child_received(),
                            cx,
                        );
                    });
                }
            }
        }

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            focus_handle,
            chat,
            pane: demo_pane,
            pane_chat,
            pane_comments,
            copies,
            sidebar,
            projects,
            new_thread,
            changes,
            comments,
            subthread_bar,
            collab,
            share_dialog,
            share_wizard,
            join_wizard,
            account,
            settings,
            join,
            palette,
            palette_query: String::new(),
            drawer_open: crate::app::devhooks::from_env().is_some_and(|dev| dev.drawer),
            drawer_tab: DrawerTab::default(),
            seen_completed_turns: 0,
            pending_subthread: None,
            fork_view,
            pending_fork: None,
            disk_usage,
            pending_removals: HashMap::new(),
            client,
            connected: false,
            _subscriptions: subscriptions,
            _net_events: net_events,
            _daemon_start: None,
            dev: crate::app::devhooks::from_env(),
            dev_demo: crate::app::devhooks::from_env().is_some_and(|dev| dev.demo),
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
        if self.client.send(message) == crate::system::net::SendStatus::Enqueued {
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
        if self.client.send(message) == crate::system::net::SendStatus::Rejected {
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
                Notification::info(format!(
                    "Write a prompt first, then press {}.",
                    crate::app::keymap::label("secondary-shift-enter")
                )),
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

    /// Ask for a title, a first prompt and a kind, then fork at the message.
    fn open_fork(
        &mut self,
        target: crate::conversation::forks::ForkTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.connected {
            return;
        }
        window.close_all_dialogs(cx);
        self.fork_view
            .update(cx, |view, cx| view.reset(target, window, cx));
        let view = self.fork_view.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Fork from here")
                .w(px(480.))
                .child(view.clone())
        });
    }

    fn send_fork(&mut self, message: ClientMessage, window: &mut Window, cx: &mut Context<Self>) {
        let ClientMessage::ForkThread { thread_id, .. } = &message else {
            return;
        };
        let parent = thread_id.clone();
        if self.client.send(message) == crate::system::net::SendStatus::Rejected {
            window.push_notification(
                Notification::error("The daemon is not connected; nothing was forked."),
                cx,
            );
            return;
        }
        self.pending_fork = Some(parent);
    }

    /// ⌘K "Fork from here": the turn under the rail cursor.
    fn fork_here(&mut self, message_id: trama_domain::ids::MessageId, cx: &mut Context<Self>) {
        self.chat
            .update(cx, |chat, cx| chat.request_fork(message_id, cx));
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
        } else if self.pane.is_open() && self.pane_chat.read(cx).has_comment_target() {
            self.pane_chat
                .update(cx, |chat, cx| chat.comment_on_selection(window, cx));
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
            SidebarEvent::CreateProject => self.open_projects_with(true, window, cx),
            SidebarEvent::QuickSwitch => self.open_palette(window, cx),
            SidebarEvent::OpenShared(listing) => self.open_shared(listing.clone(), window, cx),
            SidebarEvent::RefreshShared => {
                self.collab
                    .update(cx, |collab, cx| collab.refresh_shared(cx));
            }
            SidebarEvent::JoinShared => self.open_join(window, cx),
            SidebarEvent::OpenInPane(thread) => self.open_pane(thread.clone(), false, cx),
            SidebarEvent::ArchiveThread(thread_id) => {
                self.request_removal(thread_id.clone(), RemovalKind::Archive, false, window, cx);
            }
            SidebarEvent::UnarchiveThread(thread_id) => {
                self.unarchive(thread_id.clone(), window, cx);
            }
        }
        cx.notify();
    }

    fn dispatch_request(&mut self, request: OnboardingRequest, cx: &mut Context<Self>) {
        match request {
            OnboardingRequest::SelectWorkspace { workspace_id } => {
                self.close_pane(cx);
                self.client.select_workspace(workspace_id);
                self.focus_thread(None, cx);
            }
            OnboardingRequest::SelectThread { thread_id } => {
                // Before subscribing: the pane may follow this thread.
                self.close_pane(cx);
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
