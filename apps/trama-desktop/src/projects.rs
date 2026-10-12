//! Manage projects dialog and the New thread dialog.
//!
//! These are the setup forms that used to sit in the sidebar. All validation
//! and protocol mapping stays in [`crate::onboarding`]; these views only own
//! the text inputs and forward to the shared [`SidebarPanel`] state.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::onboarding::{Feedback, Link};
use crate::sidebar::{SidebarEvent, SidebarPanel};
use crate::theme::{self, Palette};

pub enum ProjectsEvent {
    StartDaemon,
    /// Open the collaboration account dialog.
    OpenCollaboration,
}

pub struct ProjectsPanel {
    sidebar: Entity<SidebarPanel>,
    workspace_name: Entity<InputState>,
    repo_path: Entity<InputState>,
    repo_alias: Entity<InputState>,
    picker_open: bool,
    _picker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProjectsEvent> for ProjectsPanel {}

impl ProjectsPanel {
    pub fn new(sidebar: Entity<SidebarPanel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_name =
            cx.new(|cx| InputState::new(window, cx).placeholder("Project name, e.g. Payments"));
        let repo_path =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/an/existing/local/repo"));
        let repo_alias = cx.new(|cx| InputState::new(window, cx).placeholder("Alias, e.g. api"));
        let subscriptions = vec![
            cx.observe(&sidebar, |_, _, cx| cx.notify()),
            cx.subscribe_in(&workspace_name, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit_workspace(window, cx);
                }
            }),
            cx.subscribe_in(&repo_alias, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit_repository(window, cx);
                }
            }),
        ];
        Self {
            sidebar,
            workspace_name,
            repo_path,
            repo_alias,
            picker_open: false,
            _picker: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus_first(&self, window: &mut Window, cx: &mut App) {
        let has_workspace = self
            .sidebar
            .read(cx)
            .onboarding
            .selected_workspace
            .is_some();
        let input = if has_workspace {
            &self.repo_path
        } else {
            &self.workspace_name
        };
        input.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn submit_workspace(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.workspace_name.read(cx).value().to_string();
        self.sidebar.update(cx, |sidebar, cx| {
            if let Some(request) = sidebar.onboarding.submit_workspace(&name) {
                sidebar.emit_requests(vec![request], cx);
            }
            cx.notify();
        });
    }

    fn submit_repository(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let path = self.repo_path.read(cx).value().to_string();
        let alias = self.repo_alias.read(cx).value().to_string();
        self.sidebar.update(cx, |sidebar, cx| {
            if let Some(request) = sidebar.onboarding.submit_repository(&path, &alias) {
                sidebar.emit_requests(vec![request], cx);
            }
            cx.notify();
        });
    }

    fn select_workspace(&mut self, id: trama_domain::ids::WorkspaceId, cx: &mut Context<Self>) {
        self.sidebar.update(cx, |sidebar, cx| {
            let requests = sidebar.onboarding.select_workspace(id);
            sidebar.emit_requests(requests, cx);
            cx.notify();
        });
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
                        this.sidebar.update(cx, |sidebar, cx| {
                            sidebar.onboarding.note_picker_failure(message);
                            cx.notify();
                        });
                    }
                    crate::dialog::PickerOutcome::Picked(None)
                    | crate::dialog::PickerOutcome::Cancelled => {}
                }
                cx.notify();
            });
        }));
    }

    fn open_prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sidebar = self.sidebar.clone();
        if !sidebar.read(cx).onboarding.can_prepare_checkout() {
            return;
        }
        open_prepare_confirm(sidebar, window, cx);
    }

    fn flush_cleared_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (clear_name, clear_repo) = self.sidebar.update(cx, |sidebar, _| {
            (
                std::mem::take(&mut sidebar.clear_workspace_name),
                std::mem::take(&mut sidebar.clear_repo_fields),
            )
        });
        if clear_name {
            self.workspace_name
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        if clear_repo {
            self.repo_path
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.repo_alias
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }
}

/// Confirm, then send `MigrateLegacyThread` for the selected thread.
pub fn open_prepare_confirm(sidebar: Entity<SidebarPanel>, window: &mut Window, cx: &mut App) {
    let weak = sidebar.downgrade();
    crate::dialog::open_migration_confirm(window, cx, move |_window, cx| {
        let _ = weak.update(cx, |sidebar, cx| {
            if let Some(request) = sidebar.onboarding.confirm_migration() {
                cx.emit(SidebarEvent::Requests(vec![request]));
            }
            cx.notify();
        });
    });
}

impl Render for ProjectsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_cleared_inputs(window, cx);
        let p = theme::palette(cx);
        let sidebar = self.sidebar.read(cx);
        let onboarding = &sidebar.onboarding;
        let connected = onboarding.connected();
        let link = onboarding.link().clone();
        let can_start = onboarding.can_start_daemon();
        let can_workspace = onboarding.can_create_workspace();
        let can_repo = onboarding.can_add_repository();
        let can_prepare = onboarding.can_prepare_checkout();
        let workspaces: Vec<_> = onboarding
            .workspaces
            .iter()
            .map(|workspace| {
                (
                    workspace.clone(),
                    onboarding.displayed_repo_count(workspace),
                )
            })
            .collect();
        let selected_workspace = onboarding.selected_workspace.clone();
        let repositories = onboarding.repositories.clone();
        let workspace_feedback = onboarding.workspace_feedback.clone();
        let repo_feedback = onboarding.repo_feedback.clone();
        let migration_feedback = onboarding.migration_feedback.clone();
        let daemon_feedback = onboarding.daemon_feedback.clone();
        let detail = onboarding.status_detail();
        let selected_thread_title = sidebar.selected_thread_title();
        let selected_workspace_title = sidebar.selected_workspace_title();

        let daemon_section = section(
            &p,
            "Daemon",
            v_flex()
                .gap(theme::sp(2.))
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .text_size(theme::text_ui())
                        .child(div().size(px(7.)).rounded_full().bg(match link {
                            Link::Online => p.success,
                            Link::Offline => p.warning,
                            Link::ProtocolMismatch { .. } => p.danger,
                        }))
                        .child(div().flex_1().child(match link {
                            Link::Online => "Connected to the local daemon",
                            Link::Offline => "Offline, reconnecting automatically",
                            Link::ProtocolMismatch { .. } => "Protocol mismatch",
                        }))
                        .when(can_start, |this| {
                            this.child(
                                Button::new("projects-start-daemon")
                                    .small()
                                    .label("Start daemon")
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ProjectsEvent::StartDaemon);
                                    })),
                            )
                        }),
                )
                .when(!detail.is_empty(), |this| {
                    this.child(muted(&p, detail.clone()))
                })
                .children(daemon_feedback.map(|feedback| feedback_line(&feedback, &p))),
        );

        let mut workspace_list = v_flex().gap(px(2.));
        if connected && workspaces.is_empty() {
            workspace_list = workspace_list.child(muted(&p, "No projects yet. Name one below."));
        }
        for (workspace, count) in workspaces {
            let selected = selected_workspace.as_ref() == Some(&workspace.id);
            let id = workspace.id.clone();
            workspace_list = workspace_list.child(
                h_flex()
                    .id(SharedString::from(format!("projects-ws-{}", workspace.id)))
                    .h(px(30.))
                    .px(theme::sp(2.))
                    .gap(theme::sp(2.))
                    .items_center()
                    .rounded(theme::radius())
                    .cursor_pointer()
                    .text_size(theme::text_ui())
                    .when(selected, |this| this.bg(p.selected))
                    .when(!selected, |this| this.hover(|style| style.bg(p.hover)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_workspace(id.clone(), cx);
                    }))
                    .child(
                        Icon::new(IconName::FolderGit2)
                            .with_size(px(14.))
                            .text_color(p.text_muted),
                    )
                    .child(div().flex_1().child(workspace.title.clone()))
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(format!("{count} repos")),
                    )
                    .when(selected, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .with_size(px(13.))
                                .text_color(p.accent_text),
                        )
                    }),
            );
        }
        let projects_section = section(
            &p,
            "Projects",
            v_flex()
                .gap(theme::sp(2.))
                .child(workspace_list)
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.workspace_name).id("workspace-name")),
                        )
                        .child(
                            Button::new("create-workspace")
                                .label("Create project")
                                .disabled(!can_workspace)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_workspace(window, cx);
                                })),
                        ),
                )
                .children(workspace_feedback.map(|feedback| feedback_line(&feedback, &p))),
        );

        let repo_title = match &selected_workspace_title {
            Some(title) => format!("Repositories in {title}"),
            None => "Repositories".to_string(),
        };
        let mut repo_list = v_flex().gap(px(2.));
        if selected_workspace.is_none() {
            repo_list = repo_list.child(muted(&p, "Select a project first."));
        } else if repositories.is_empty() {
            repo_list = repo_list.child(muted(
                &p,
                "No repositories yet. Threads need at least one repository enabled by default.",
            ));
        }
        for repo in &repositories {
            repo_list = repo_list.child(
                h_flex()
                    .px(theme::sp(2.))
                    .py(theme::sp(1.5))
                    .gap(theme::sp(2.))
                    .items_center()
                    .rounded(theme::radius())
                    .border_1()
                    .border_color(p.border)
                    .child(
                        Icon::new(IconName::GitBranch)
                            .with_size(px(14.))
                            .text_color(p.text_muted),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(theme::text_ui()).child(repo.title.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .font_family(theme::MONO_FONT)
                                    .text_size(theme::text_xs())
                                    .text_color(p.text_faint)
                                    .child(repo.path.display().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .px(theme::sp(1.5))
                            .rounded(theme::radius_sm())
                            .border_1()
                            .border_color(p.border)
                            .text_size(theme::text_xs())
                            .text_color(if repo.default_enabled {
                                p.text_muted
                            } else {
                                p.text_faint
                            })
                            .child(if repo.default_enabled {
                                "default"
                            } else {
                                "opt-in"
                            }),
                    ),
            );
        }
        let repos_section = section(
            &p,
            repo_title,
            v_flex()
                .gap(theme::sp(2.))
                .child(repo_list)
                .child(muted(
                    &p,
                    "Attach an existing local Git repository. Remote URLs are not cloned.",
                ))
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.repo_path).id("repo-path")),
                        )
                        .child(
                            Button::new("choose-repo-dir")
                                .icon(IconName::FolderOpen)
                                .label("Choose…")
                                .disabled(!can_repo)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.choose_folder(cx);
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.repo_alias).id("repo-alias")),
                        )
                        .child(
                            Button::new("add-repo")
                                .primary()
                                .label("Add repository")
                                .disabled(!can_repo)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_repository(window, cx);
                                })),
                        ),
                )
                .children(repo_feedback.map(|feedback| feedback_line(&feedback, &p))),
        );

        let checkout_section = section(
            &p,
            "Isolated checkout",
            v_flex()
                .gap(theme::sp(2.))
                .child(muted(
                    &p,
                    "Move the selected thread onto its own isolated checkout. The transcript is kept, your files are untouched, and nothing is sent automatically.",
                ))
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(theme::text_ui())
                                .text_color(if selected_thread_title.is_some() {
                                    p.text
                                } else {
                                    p.text_faint
                                })
                                .child(
                                    selected_thread_title
                                        .unwrap_or_else(|| "No thread selected".into()),
                                ),
                        )
                        .child(
                            Button::new("prepare-isolated-checkout")
                                .label("Prepare isolated checkout")
                                .disabled(!can_prepare)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_prepare(window, cx);
                                })),
                        ),
                )
                .children(migration_feedback.map(|feedback| feedback_line(&feedback, &p))),
        );

        let collaboration_section = section(
            &p,
            "Collaboration",
            h_flex()
                .gap(theme::sp(2.))
                .items_center()
                .child(div().flex_1().child(muted(
                    &p,
                    "Share threads through a collaboration server. Each person's turns run on their own machine with their own Claude Code.",
                )))
                .child(
                    Button::new("projects-collaboration")
                        .icon(IconName::Users)
                        .label("Collaboration…")
                        .disabled(!connected)
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(ProjectsEvent::OpenCollaboration);
                        })),
                ),
        );

        v_flex()
            .w_full()
            .gap(theme::sp(5.))
            .child(daemon_section)
            .child(projects_section)
            .child(repos_section)
            .child(checkout_section)
            .child(collaboration_section)
    }
}

fn section(p: &Palette, title: impl Into<String>, body: impl IntoElement) -> Div {
    let title: String = title.into();
    v_flex()
        .gap(theme::sp(2.))
        .when(!title.is_empty(), |this| {
            this.child(
                div()
                    .text_size(theme::text_xs())
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_faint)
                    .child(title.to_uppercase()),
            )
        })
        .child(body)
}

fn muted(p: &Palette, text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(theme::text_sm())
        .text_color(p.text_muted)
        .child(text.into())
}

pub fn feedback_line(feedback: &Feedback, p: &Palette) -> impl IntoElement + use<> {
    let color = match feedback {
        Feedback::Progress(_) => p.warning,
        Feedback::Error(_) => p.danger,
        Feedback::Notice(_) => p.success,
    };
    div()
        .text_size(theme::text_sm())
        .text_color(color)
        .child(feedback.text().to_string())
}

/// Title input for a new thread in the selected project.
pub struct NewThreadView {
    sidebar: Entity<SidebarPanel>,
    title: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

pub enum NewThreadEvent {
    Submitted,
    ManageProjects,
}

impl EventEmitter<NewThreadEvent> for NewThreadView {}

impl NewThreadView {
    pub fn new(sidebar: Entity<SidebarPanel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title =
            cx.new(|cx| InputState::new(window, cx).placeholder("What should the agent work on?"));
        let subscriptions = vec![
            cx.observe(&sidebar, |_, _, cx| cx.notify()),
            cx.subscribe_in(&title, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(window, cx);
                }
            }),
        ];
        Self {
            sidebar,
            title,
            _subscriptions: subscriptions,
        }
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.title
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.sidebar.update(cx, |sidebar, _| {
            if sidebar
                .onboarding
                .thread_feedback
                .as_ref()
                .is_some_and(|feedback| !feedback.is_error())
            {
                sidebar.onboarding.thread_feedback = None;
            }
        });
        self.title.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub fn submit(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().to_string();
        let sent = self.sidebar.update(cx, |sidebar, cx| {
            let request = sidebar.onboarding.submit_thread(&title);
            let sent = request.is_some();
            if let Some(request) = request {
                sidebar.emit_requests(vec![request], cx);
            }
            cx.notify();
            sent
        });
        if sent {
            cx.emit(NewThreadEvent::Submitted);
        }
    }
}

impl Render for NewThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let sidebar = self.sidebar.read(cx);
        let onboarding = &sidebar.onboarding;
        let project = sidebar.selected_workspace_title();
        let can_thread = onboarding.can_create_thread();
        let feedback = onboarding
            .thread_feedback
            .clone()
            .filter(Feedback::is_error);
        let has_value = !self.title.read(cx).value().trim().is_empty();

        v_flex()
            .gap(theme::sp(3.))
            .child(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Icon::new(IconName::FolderGit2).with_size(px(13.)))
                    .child(project.unwrap_or_else(|| "No project selected".into())),
            )
            .child(Input::new(&self.title).id("new-thread-title"))
            .when(!can_thread, |this| {
                this.child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .child(muted(
                            &p,
                            "Add an existing local repository to this project first.",
                        ))
                        .child(
                            Button::new("new-thread-manage")
                                .small()
                                .label("Manage projects")
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(NewThreadEvent::ManageProjects);
                                })),
                        ),
                )
            })
            .children(feedback.map(|feedback| feedback_line(&feedback, &p)))
            .child(
                h_flex()
                    .justify_end()
                    .gap(theme::sp(2.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child("Enter to create \u{00b7} Esc to cancel"),
                    )
                    .child(
                        Button::new("new-thread-create")
                            .primary()
                            .label("Create thread")
                            .disabled(!can_thread || !has_value)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
