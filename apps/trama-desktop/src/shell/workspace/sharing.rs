use super::*;

impl Workspace {
    // -- shared threads -----------------------------------------------------

    pub(super) fn open_share(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        if self.route_share(window, cx) {
            return;
        }
        let (thread, title) = {
            let sidebar = self.sidebar.read(cx);
            let onboarding = &sidebar.onboarding;
            match onboarding
                .selected_thread
                .as_ref()
                .and_then(|id| onboarding.thread(id))
            {
                Some(thread) if thread.kind == ThreadKind::Top => {
                    (thread.id.clone(), thread.title.clone())
                }
                Some(_) => {
                    window.push_notification(
                        Notification::info(
                            "Share the top-level thread; its subthreads stay on this machine.",
                        ),
                        cx,
                    );
                    return;
                }
                None => {
                    window.push_notification(Notification::info("Select a thread to share."), cx);
                    return;
                }
            }
        };
        window.close_all_dialogs(cx);
        self.collab
            .update(cx, |collab, cx| collab.open_share(thread, title, cx));
        self.share_dialog
            .update(cx, |dialog, cx| dialog.reset(window, cx));
        let view = self.share_dialog.clone();
        let collab = self.collab.downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let collab = collab.clone();
            dialog
                .title("Share thread")
                .w(px(520.))
                .on_close(move |_, _, cx| {
                    let _ = collab.update(cx, |collab, cx| {
                        collab.share.close();
                        cx.notify();
                    });
                })
                .child(view.clone())
        });
    }

    pub(super) fn open_account(
        &mut self,
        server: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.close_all_dialogs(cx);
        self.account
            .update(cx, |account, cx| account.reset(server, window, cx));
        let view = self.account.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Collaboration")
                .w(px(520.))
                .child(view.clone())
        });
    }

    pub(super) fn open_join(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        if self.route_join(window, cx) {
            return;
        }
        window.close_all_dialogs(cx);
        let (workspace, title) = {
            let sidebar = self.sidebar.read(cx);
            (
                sidebar.onboarding.selected_workspace.clone(),
                sidebar.selected_workspace_title(),
            )
        };
        let view = self.join.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Join shared thread")
                .w(px(520.))
                .child(view.clone())
        });
        // After opening: the dialog takes focus when it opens.
        self.join.update(cx, |join, cx| {
            join.workspace = workspace;
            join.workspace_title = title;
            join.reset(window, cx);
        });
    }

    /// A "Shared with me" row: open its local thread, or join it into the
    /// selected project.
    pub(super) fn open_shared(
        &mut self,
        listing: trama_protocol::SharedThreadListing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(thread_id) = listing.local_thread_id {
            self.sidebar
                .update(cx, |sidebar, cx| sidebar.select_thread(thread_id, cx));
            return;
        }
        let workspace = self.sidebar.read(cx).onboarding.selected_workspace.clone();
        let target = crate::collab::JoinTarget {
            server_url: None,
            thread_id: listing.collab_thread_id,
            link_token: None,
        };
        let sent = self
            .collab
            .update(cx, |collab, cx| collab.join(workspace, &target, cx));
        if !sent {
            let message = self
                .collab
                .read(cx)
                .join_error
                .clone()
                .unwrap_or_else(|| "Could not join the shared thread.".into());
            window.push_notification(Notification::error(message), cx);
        }
    }

    pub(super) fn open_shortcuts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn palette_groups(&self, cx: &App) -> (Vec<CommandGroup>, Vec<Vec<PaletteTarget>>) {
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
                "Share this thread…",
                IconName::Share2,
                PaletteTarget::Share,
                connected && self.selected_thread_kind(cx) == Some(ThreadKind::Top),
            ),
            (
                "Join shared thread…",
                IconName::Link,
                PaletteTarget::JoinShared,
                connected,
            ),
            (
                if self.collab.read(cx).signed_in() {
                    "Collaboration account"
                } else {
                    "Connect to collaboration server"
                },
                IconName::Users,
                PaletteTarget::CollabAccount,
                connected,
            ),
            (
                "Disk usage",
                IconName::HardDrive,
                PaletteTarget::DiskUsage,
                connected,
            ),
            (
                "Archive this thread",
                IconName::Archive,
                PaletteTarget::ArchiveSelected,
                connected && self.selected_archived(cx) == Some(false),
            ),
            (
                "Unarchive this thread",
                IconName::ArchiveRestore,
                PaletteTarget::UnarchiveSelected,
                connected && self.selected_archived(cx) == Some(true),
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
        let mut commands = commands;
        if let Some(message) = self.chat.read(cx).fork_here_target() {
            commands.insert(
                1,
                (
                    "Fork from here",
                    IconName::GitFork,
                    PaletteTarget::ForkHere(message),
                    connected,
                ),
            );
        }
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

    pub(super) fn run_palette_target(
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
                crate::shell::projects::open_prepare_confirm(self.sidebar.clone(), window, cx);
            }
            PaletteTarget::ToggleAppearance => self.toggle_appearance(window, cx),
            PaletteTarget::Shortcuts => self.open_shortcuts(window, cx),
            PaletteTarget::Review => self.start_review(window, cx),
            PaletteTarget::Land => self.confirm_land(window, cx),
            PaletteTarget::PullChanges => self.pull_selected(cx),
            PaletteTarget::Share => self.open_share(window, cx),
            PaletteTarget::JoinShared => self.open_join(window, cx),
            PaletteTarget::CollabAccount => self.open_account(None, window, cx),
            PaletteTarget::ForkHere(message) => self.fork_here(message, cx),
            PaletteTarget::DiskUsage => self.open_disk_usage(window, cx),
            PaletteTarget::ArchiveSelected => self.archive_selected(window, cx),
            PaletteTarget::UnarchiveSelected => {
                if let Some(id) = self.sidebar.read(cx).onboarding.selected_thread.clone() {
                    self.unarchive(id, window, cx);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn toggle_drawer(&mut self, cx: &mut Context<Self>) {
        self.drawer_open = !self.drawer_open;
        self.refresh_changes(cx);
        cx.notify();
    }

    pub(super) fn toggle_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let appearance = if cx.theme_is_dark() {
            theme::AppearancePref::Light
        } else {
            theme::AppearancePref::Dark
        };
        let prefs = theme::ThemePrefs {
            appearance,
            ..theme::prefs(cx)
        };
        theme::set_prefs(prefs, window.appearance(), cx);
        cx.notify();
    }

    pub(super) fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        } else if self.drawer_open {
            self.drawer_open = false;
            cx.notify();
        } else {
            cx.propagate();
        }
    }
}
