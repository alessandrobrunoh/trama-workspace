use super::*;

impl Workspace {
    // -- daemon -----------------------------------------------------------

    pub(super) fn on_net_event(
        &mut self,
        event: NetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NetEvent::Connected { .. } => {
                self.connected = true;
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_link(Link::Online, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(true, cx);
                });
                self.pane_chat.update(cx, |chat, cx| {
                    chat.set_connected(true, cx);
                });
                self.collab.update(cx, |collab, cx| collab.on_connected(cx));
                self.settings
                    .update(cx, |settings, cx| settings.set_connected(true, cx));
            }
            NetEvent::Disconnected => {
                self.connected = false;
                self.settings
                    .update(cx, |settings, cx| settings.set_connected(false, cx));
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.set_link(Link::Offline, cx);
                });
                self.chat.update(cx, |chat, cx| {
                    chat.set_connected(false, cx);
                });
                self.pane_chat.update(cx, |chat, cx| {
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
                self.pane_chat.update(cx, |chat, cx| {
                    chat.set_connected(false, cx);
                });
            }
            NetEvent::OutboundRejected(message) => self.on_outbound_rejected(message, window, cx),
            NetEvent::Message(message) => self.on_daemon_message(message, window, cx),
        }
        cx.notify();
    }

    pub(super) fn on_outbound_rejected(
        &mut self,
        message: ClientMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let ClientMessage::SendPrompt {
            thread_id, text, ..
        } = message
        {
            let restore = self.sidebar.update(cx, |sidebar, _cx| {
                sidebar
                    .onboarding
                    .note_rejected_prompt(thread_id.clone(), text)
            });
            if let Some(text) = restore {
                self.chat.update(cx, |chat, cx| {
                    chat.restore_prompt(&thread_id, &text, window, cx);
                });
                self.pane_chat.update(cx, |chat, cx| {
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

    pub(super) fn start_daemon(&mut self, cx: &mut Context<Self>) {
        let mismatch = matches!(
            self.sidebar.read(cx).onboarding.link(),
            Link::ProtocolMismatch { .. }
        );
        if mismatch {
            return;
        }
        match crate::app::startup::locate_daemon() {
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

    pub(super) fn spawn_daemon(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
                .spawn(async move { crate::app::startup::start_daemon(&path) })
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

    pub(super) fn on_daemon_message(
        &mut self,
        message: DaemonMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let main_comment = self
            .comments
            .update(cx, |comments, cx| comments.apply(&message, cx));
        let pane_comment = self
            .pane_comments
            .update(cx, |comments, cx| comments.apply(&message, cx));
        if main_comment || pane_comment {
            return;
        }
        if self
            .collab
            .update(cx, |collab, cx| collab.apply(&message, cx))
        {
            return;
        }
        match message {
            DaemonMessage::SharedThreadJoined { thread, shared } => {
                window.close_all_dialogs(cx);
                let id = thread.id.clone();
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.apply_subthread(thread.clone());
                    if sidebar.onboarding.thread(&id).is_none() {
                        let mut threads = sidebar.onboarding.threads.clone();
                        threads.push(thread);
                        sidebar.onboarding.apply_threads(threads);
                    }
                    cx.notify();
                });
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.select_thread(id, cx));
                window.push_notification(
                    Notification::success(format!(
                        "Joined \u{201c}{}\u{201d}, shared by {}.",
                        shared.title, shared.owner
                    ))
                    .title("Shared thread"),
                    cx,
                );
            }
            DaemonMessage::ThreadCheckouts {
                thread_id,
                checkouts,
            } => {
                self.chat.update(cx, |chat, cx| {
                    chat.set_checkouts(&thread_id, &checkouts, cx)
                });
                self.pane_chat.update(cx, |chat, cx| {
                    chat.set_checkouts(&thread_id, &checkouts, cx)
                });
                self.copies
                    .update(cx, |copies, cx| copies.apply(&thread_id, checkouts, cx));
            }
            DaemonMessage::DiskUsage { items, .. } => {
                self.disk_usage.update(cx, |view, cx| view.apply(items, cx));
            }
            DaemonMessage::ThreadArchived { thread, removed } => {
                let id = thread.id.clone();
                let title = thread.title.clone();
                self.upsert_thread_snapshot(thread, cx);
                self.after_removal(&id, cx);
                let freed = crate::system::disk_usage::freed_total(&removed)
                    .map(|bytes| {
                        format!(" Freed {}.", crate::system::disk_usage::format_bytes(bytes))
                    })
                    .unwrap_or_default();
                window.push_notification(
                    Notification::success(format!(
                        "Archived \u{201c}{title}\u{201d}.{freed} Unarchive it from the sidebar to recreate its checkouts."
                    ))
                    .title("Thread archived"),
                    cx,
                );
            }
            DaemonMessage::ThreadUnarchived(thread) => {
                let title = thread.title.clone();
                self.upsert_thread_snapshot(thread, cx);
                self.refresh_disk_usage(cx);
                window.push_notification(
                    Notification::success(format!("Unarchived \u{201c}{title}\u{201d}."))
                        .title("Thread restored"),
                    cx,
                );
            }
            DaemonMessage::ThreadCheckoutsDeleted { thread_id, removed } => {
                self.after_removal(&thread_id, cx);
                let freed = crate::system::disk_usage::freed_total(&removed)
                    .map(|bytes| {
                        format!(" Freed {}.", crate::system::disk_usage::format_bytes(bytes))
                    })
                    .unwrap_or_default();
                window.push_notification(
                    Notification::success(format!(
                        "Deleted {} cop{}.{freed} They are recreated when the thread needs them.",
                        removed.len(),
                        if removed.len() == 1 { "y" } else { "ies" }
                    ))
                    .title("Checkouts removed"),
                    cx,
                );
            }
            DaemonMessage::CheckoutRemovalRefused {
                thread_id,
                checkouts,
            } => {
                self.confirm_forced_removal(thread_id, checkouts, window, cx);
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
            DaemonMessage::SubthreadCreated(snapshot)
                if self.pending_fork.is_some()
                    && snapshot.parent_thread_id == self.pending_fork =>
            {
                self.pending_fork = None;
                let id = snapshot.id.clone();
                self.sidebar.update(cx, |sidebar, cx| {
                    sidebar.onboarding.apply_subthread(snapshot);
                    sidebar.select_thread(id, cx);
                    cx.notify();
                });
            }
            DaemonMessage::SubagentStatusChanged {
                thread_id,
                status,
                detail,
                ..
            } => {
                self.sidebar.update(cx, |sidebar, cx| {
                    if crate::conversation::subagents::apply_status(
                        &mut sidebar.onboarding.threads,
                        &thread_id,
                        status,
                        detail,
                    ) {
                        cx.notify();
                    }
                });
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
            DaemonMessage::Providers { .. } | DaemonMessage::ProviderTestResult { .. } => {
                self.settings
                    .update(cx, |settings, cx| settings.handle_daemon(&message, cx));
            }
            DaemonMessage::ThreadModelChanged { thread_id, model } => {
                // Keep the snapshot current so the picker is not reset.
                self.sidebar.update(cx, |sidebar, cx| {
                    if let Some(thread) = sidebar
                        .onboarding
                        .threads
                        .iter_mut()
                        .find(|thread| thread.id == thread_id)
                    {
                        thread.model = model.clone();
                        cx.notify();
                    }
                });
                self.chat.update(cx, |chat, cx| {
                    chat.handle_daemon(DaemonMessage::ThreadModelChanged { thread_id, model }, cx);
                });
            }
            DaemonMessage::Error { message, .. }
                if self
                    .changes
                    .update(cx, |changes, cx| changes.fail_loading(&message, cx)) =>
            {
                // The pending GetChanges failed (e.g. no last turn yet).
            }
            DaemonMessage::Error { message, .. }
                if self
                    .disk_usage
                    .update(cx, |view, cx| view.fail_loading(&message, cx)) =>
            {
                // The pending GetDiskUsage failed.
            }
            DaemonMessage::Error { code, message } => {
                self.pending_subthread = None;
                self.pending_fork = None;
                if !self.pending_removals.is_empty() {
                    self.pending_removals.clear();
                    self.disk_usage
                        .update(cx, |view, cx| view.clear_pending(cx));
                }
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
                if self.pane.is_open() {
                    let copy = other.clone();
                    self.pane_chat.update(cx, |chat, cx| {
                        chat.handle_daemon(copy, cx);
                    });
                }
                self.chat.update(cx, |chat, cx| {
                    chat.handle_daemon(other, cx);
                });
            }
        }
    }

    pub(super) fn run_dev_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            Some("share") => self.open_share(window, cx),
            Some("collab") => self.open_account(None, window, cx),
            Some("settings") => self.open_settings(None, window, cx),
            Some("providers") => {
                self.open_settings(Some(crate::settings::Section::Providers), window, cx)
            }
            Some("keybindings") => {
                self.open_settings(Some(crate::settings::Section::Keybindings), window, cx)
            }
            Some("join") => self.open_join(window, cx),
            _ => {}
        }
    }

    pub(super) fn maybe_select_default_workspace(&mut self, cx: &mut Context<Self>) {
        let first = {
            let onboarding = &self.sidebar.read(cx).onboarding;
            if onboarding.selected_workspace.is_some() {
                return;
            }
            // Settings > General > Default project, else the first one.
            let preferred = crate::settings::current(cx).general.default_project;
            onboarding
                .workspaces
                .iter()
                .find(|ws| preferred.as_deref() == Some(ws.id.to_string().as_str()))
                .or(onboarding.workspaces.first())
                .map(|ws| ws.id.clone())
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
