use super::*;

impl Workspace {
    // -- chrome -----------------------------------------------------------

    /// Compact header at the top of the content card: breadcrumb, view
    /// chips and thread actions.
    pub(super) fn render_card_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

        let drawer_tab = self.drawer_tab;
        let chip = |id: &'static str, label: &'static str, active: bool| {
            h_flex()
                .id(id)
                .h(px(24.))
                .px(theme::sp(2.5))
                .items_center()
                .rounded(theme::radius())
                .border_1()
                .text_size(theme::text_sm())
                .cursor_pointer()
                .when(active, |this| {
                    this.bg(p.selected)
                        .border_color(p.border)
                        .text_color(p.text)
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!active, |this| {
                    this.border_color(p.bg)
                        .text_color(p.text_muted)
                        .hover(|style| style.bg(p.hover).text_color(p.text))
                })
                .child(label)
        };
        let chips = h_flex()
            .flex_shrink_0()
            .gap(theme::sp(1.))
            .items_center()
            .child(
                chip("chip-transcript", "Transcript", !drawer_open).on_click(cx.listener(
                    |this, _, _, cx| {
                        if this.drawer_open {
                            this.toggle_drawer(cx);
                        }
                    },
                )),
            )
            .child(
                chip(
                    "chip-changes",
                    "Changes",
                    drawer_open && drawer_tab == DrawerTab::Changes,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.show_drawer_tab(DrawerTab::Changes, cx)),
                ),
            )
            .child(
                chip(
                    "chip-copies",
                    "Copies",
                    drawer_open && drawer_tab == DrawerTab::Copies,
                )
                .on_click(
                    cx.listener(|this, _, _, cx| this.show_drawer_tab(DrawerTab::Copies, cx)),
                ),
            );

        h_flex()
            .w_full()
            .h(theme::card_header_height())
            .flex_shrink_0()
            .border_b_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .size_full()
                    .items_center()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(theme::sp(2.))
                            .px(theme::sp(3.))
                            .items_center()
                            .text_size(theme::text_ui())
                            .when_some(project, |this, project| {
                                this.child(
                                    h_flex()
                                        .flex_shrink_0()
                                        .gap(theme::sp(1.5))
                                        .items_center()
                                        .text_color(p.text_muted)
                                        .child(Icon::new(IconName::FolderGit2).with_size(px(13.)))
                                        .child(project)
                                        .child(
                                            Icon::new(IconName::ChevronRight)
                                                .with_size(px(12.))
                                                .text_color(p.text_faint),
                                        ),
                                )
                            })
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
                            .when(!repos.is_empty(), |this| {
                                this.child(
                                    h_flex()
                                        .flex_shrink_0()
                                        .gap(theme::sp(1.))
                                        .items_center()
                                        .text_size(theme::text_xs())
                                        .text_color(p.text_faint)
                                        .child(Icon::new(IconName::GitBranch).with_size(px(11.)))
                                        .child(theme::mono_id(repos.join(", "), &p)),
                                )
                            }),
                    )
                    .child(chips)
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
                                        .tooltip(format!("Share this thread ({})", crate::app::keymap::label("secondary-shift-s")))
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
                                    .tooltip(format!("Jump to thread ({})", crate::app::keymap::label("secondary-k")))
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
                                    .tooltip(format!("Changes and copies ({})", crate::app::keymap::label("secondary-r")))
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_drawer(cx))),
                            )

                    ),
            )
    }

    /// Window title bar on the canvas: only window chrome and the daemon
    /// dot. macOS reserves room for the traffic lights at the left.
    pub(super) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let link = self.sidebar.read(cx).onboarding.link().clone();
        let (link_color, link_label): (Hsla, SharedString) = match &link {
            Link::Online => (p.success, "Daemon connected".into()),
            Link::Offline => (p.warning, "Daemon offline, reconnecting".into()),
            Link::ProtocolMismatch { daemon_protocol } => (
                p.danger,
                format!("Protocol mismatch (daemon v{daemon_protocol}); restart both").into(),
            ),
        };
        TitleBar::new()
            .h(theme::title_bar_height())
            .bg(p.canvas)
            .border_color(p.canvas)
            .child(
                h_flex()
                    .size_full()
                    .items_center()
                    .when(cfg!(target_os = "macos"), |this| {
                        this.child(div().w(theme::traffic_light_inset()).flex_shrink_0())
                    })
                    .child(
                        div()
                            .px(theme::sp(3.))
                            .text_size(theme::text_sm())
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(p.text_faint)
                            .child("Trama"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("titlebar-link")
                            .p(theme::sp(2.))
                            .child(div().size(px(7.)).rounded_full().bg(link_color))
                            .tooltip(move |window, cx| {
                                Tooltip::new(link_label.clone()).build(window, cx)
                            }),
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
            .bg(p.canvas)
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
            .child(
                [
                    ("secondary-k", "jump"),
                    ("secondary-n", "new"),
                    ("secondary-shift-enter", "subthread"),
                    ("secondary-shift-s", "share"),
                    ("secondary-r", "changes"),
                    ("secondary-/", "shortcuts"),
                ]
                .iter()
                .map(|(keys, what)| format!("{} {what}", crate::app::keymap::label(keys)))
                .collect::<Vec<_>>()
                .join(" · "),
            )
    }
}
