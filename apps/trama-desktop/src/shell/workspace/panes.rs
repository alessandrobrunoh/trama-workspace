use super::*;

impl Workspace {
    // -- side pane --------------------------------------------------------

    /// Show `thread` (a subagent) beside the selected thread; `push` keeps
    /// the one shown to go back to.
    pub(super) fn open_pane(&mut self, thread: ThreadId, push: bool, cx: &mut Context<Self>) {
        let main = self.sidebar.read(cx).onboarding.selected_thread.clone();
        let change = if push {
            self.pane.push(thread, main.as_ref())
        } else {
            self.pane.open(thread, main.as_ref())
        };
        self.apply_pane_change(change, cx);
    }

    pub(super) fn close_pane(&mut self, cx: &mut Context<Self>) {
        let change = self.pane.close();
        self.apply_pane_change(change, cx);
    }

    pub(super) fn pane_back(&mut self, cx: &mut Context<Self>) {
        let change = self.pane.go_back();
        self.apply_pane_change(change, cx);
    }

    /// Move the pane's subscription, comments and transcript to the thread
    /// it shows now.
    pub(super) fn apply_pane_change(
        &mut self,
        change: Option<crate::side_pane::PaneChange>,
        cx: &mut Context<Self>,
    ) {
        let Some(change) = change else {
            return;
        };
        self.client.select_pane_thread(change.to.clone());
        let thread = change.to;
        self.pane_comments.update(cx, |comments, cx| {
            comments.set_thread(None, cx);
            comments.set_thread(thread.clone(), cx);
        });
        self.pane_chat.update(cx, |chat, cx| {
            chat.set_thread(None, cx);
            chat.set_thread(thread, cx);
        });
        cx.notify();
    }

    /// What the pane's chrome says about the thread it shows.
    pub(super) fn pane_info(&self, cx: &App) -> Option<crate::side_pane::PaneInfo> {
        let id = self.pane.thread()?;
        let snapshot = self.sidebar.read(cx).onboarding.thread(id);
        let title = snapshot
            .map(|thread| thread.title.clone())
            .unwrap_or_else(|| "Subagent".into());
        let subagent = snapshot.and_then(|thread| thread.subagent.as_ref());
        Some(crate::side_pane::PaneInfo {
            title,
            status: subagent.map(|subagent| subagent.status),
            controlled: snapshot.is_some_and(|thread| thread.parent_thread_id.is_some()),
            icon: subagent
                .map(|subagent| crate::subagents::profile_icon(subagent.spawned_by.profile))
                .unwrap_or(IconName::Bot),
        })
    }

    pub(super) fn focus_thread(&mut self, thread_id: Option<ThreadId>, cx: &mut Context<Self>) {
        let selected = thread_id.clone();
        self.collab
            .update(cx, |collab, cx| collab.set_selected(selected, cx));
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

    pub(super) fn open_new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn open_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    // -- disk usage and archiving -----------------------------------------

    pub(super) fn open_disk_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        window.close_all_dialogs(cx);
        self.disk_usage.update(cx, |view, _| view.visible = true);
        self.refresh_disk_usage(cx);
        let view = self.disk_usage.clone();
        let closing = self.disk_usage.downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let closing = closing.clone();
            dialog
                .title("Disk usage")
                .w(px(640.))
                .on_close(move |_, _, cx| {
                    let _ = closing.update(cx, |view, _| view.visible = false);
                })
                .child(view.clone())
        });
    }

    /// Measure all workspaces' checkouts again (disk is shared by all).
    pub(super) fn refresh_disk_usage(&mut self, cx: &mut Context<Self>) {
        if !self.disk_usage.read(cx).visible {
            return;
        }
        if self
            .client
            .send(ClientMessage::GetDiskUsage { workspace_id: None })
            == crate::net::SendStatus::Enqueued
        {
            self.disk_usage.update(cx, |view, cx| view.reset(cx));
        }
    }

    /// Archive (removing checkouts) or delete a thread's copies. Without
    /// `force` the daemon refuses when a copy holds unsaved work, and
    /// [`Self::confirm_forced_removal`] asks before resending.
    pub(super) fn request_removal(
        &mut self,
        thread_id: ThreadId,
        kind: RemovalKind,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.client.send(kind.message(thread_id.clone(), force))
            == crate::net::SendStatus::Rejected
        {
            window.push_notification(
                Notification::error("The daemon is not connected; nothing was removed."),
                cx,
            );
            return;
        }
        self.disk_usage
            .update(cx, |view, cx| view.set_pending(&thread_id, true, cx));
        self.pending_removals.insert(thread_id, kind);
    }

    /// ⌘W: archive the selected thread and remove its checkouts.
    pub(super) fn archive_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.connected {
            return;
        }
        let target = {
            let onboarding = &self.sidebar.read(cx).onboarding;
            onboarding
                .selected_thread
                .as_ref()
                .and_then(|id| onboarding.thread(id))
                .map(|thread| (thread.id.clone(), thread.archived))
        };
        match target {
            Some((id, false)) => self.request_removal(id, RemovalKind::Archive, false, window, cx),
            Some((_, true)) => {
                window.push_notification(Notification::info("This thread is already archived."), cx)
            }
            None => window.push_notification(Notification::info("Select a thread to archive."), cx),
        }
    }

    pub(super) fn unarchive(
        &mut self,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .client
            .send(ClientMessage::UnarchiveThread { thread_id })
            == crate::net::SendStatus::Rejected
        {
            window.push_notification(
                Notification::error("The daemon is not connected; the thread stays archived."),
                cx,
            );
        }
    }

    pub(super) fn selected_archived(&self, cx: &App) -> Option<bool> {
        let onboarding = &self.sidebar.read(cx).onboarding;
        onboarding
            .selected_thread
            .as_ref()
            .and_then(|id| onboarding.thread(id))
            .map(|thread| thread.archived)
    }

    /// The daemon refused to remove copies with unsaved work: show it and
    /// resend with `force` on confirmation.
    pub(super) fn confirm_forced_removal(
        &mut self,
        thread_id: ThreadId,
        checkouts: Vec<trama_protocol::CheckoutRemoval>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kind = self
            .pending_removals
            .remove(&thread_id)
            .unwrap_or(RemovalKind::Archive);
        self.disk_usage
            .update(cx, |view, cx| view.set_pending(&thread_id, false, cx));
        let details = crate::disk_usage::refusal_details(&checkouts);
        let description = format!(
            "These copies hold work that is not in the source repository. Trama keeps a snapshot and restores it when the copy is recreated, but ignored files (build output, local config) are deleted.\n\n{details}"
        );
        let workspace = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            let thread_id = thread_id.clone();
            alert
                .confirm()
                .title(kind.confirm_title())
                .description(description.clone())
                .ok_text(kind.confirm_label())
                .cancel_text("Keep")
                .on_ok(move |_, window, cx| {
                    let thread_id = thread_id.clone();
                    let _ = workspace.update(cx, |this, cx| {
                        this.request_removal(thread_id, kind, true, window, cx);
                    });
                    true
                })
        });
    }

    /// Replace the thread's snapshot in the sidebar (archive state changed).
    pub(super) fn upsert_thread_snapshot(
        &mut self,
        snapshot: trama_protocol::ThreadSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.onboarding.apply_subthread(snapshot);
            cx.notify();
        });
    }

    pub(super) fn after_removal(&mut self, thread_id: &ThreadId, cx: &mut Context<Self>) {
        self.pending_removals.remove(thread_id);
        self.disk_usage
            .update(cx, |view, cx| view.set_pending(thread_id, false, cx));
        self.refresh_disk_usage(cx);
        if self.sidebar.read(cx).onboarding.selected_thread.as_ref() == Some(thread_id) {
            let _ = self.client.send(ClientMessage::GetThreadCheckouts {
                thread_id: thread_id.clone(),
            });
        }
    }
}
