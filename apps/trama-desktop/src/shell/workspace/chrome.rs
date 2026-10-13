use super::*;

impl Workspace {
    // -- chrome -----------------------------------------------------------

    pub(super) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        let selected = sidebar.onboarding.selected_thread.clone();
        let store = self.collab.read(cx);
        let shared_thread = selected.as_ref().is_some_and(|id| store.is_linked(id));
        let collaborators = selected.as_ref().filter(|_| shared_thread).and_then(|id| {
            let snapshot = store.snapshot(id)?;
            let live = store.presence(id).cloned().unwrap_or_default();
            let people = crate::collab::presence::roster(
                &snapshot.members,
                &snapshot.owner,
                &live,
                store.username(),
                &store.typing(id),
            );
            (!people.is_empty()).then(|| crate::collab::presence::title_avatars(&p, &people))
        });

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
                    .children(collaborators)
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px(theme::sp(2.))
                            .gap(theme::sp(1.))
                            .items_center()
                            .when(can_fork, |this| {
                                this.child(
                                    Button::new("titlebar-share")
                                        .ghost()
                                        .small()
                                        .icon(IconName::Share2)
                                        .label("Share")
                                        .toggled(shared_thread)
                                        .tooltip("Share this thread (\u{2318}\u{21e7}S)")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_share(window, cx);
                                        })),
                                )
                                .child(
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

    pub(super) fn render_status_bar(&self, cx: &App) -> impl IntoElement {
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
            .child("\u{2318}K jump · \u{2318}N new · \u{2318}\u{21e7}\u{23ce} subthread · \u{2318}\u{21e7}S share · \u{2318}R changes · \u{2318}/ shortcuts")
    }
}
