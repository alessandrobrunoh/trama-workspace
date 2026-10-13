//! Opening the vNext Share/Join wizards (servers with protocol ≥ 3 + E2EE).
//! Wizard state lives in the collab store, so closing the dialog keeps it
//! and reopening resumes (replies arriving meanwhile still apply).

use super::*;
use crate::collab::vnext::{DeferredOpen, ServerSupport};
use gpui_kit::component::spinner::Spinner;

impl Workspace {
    /// Any thread can root a vNext share; the scope decides what follows.
    pub(super) fn open_share_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = {
            let onboarding = &self.sidebar.read(cx).onboarding;
            onboarding
                .selected_thread
                .as_ref()
                .and_then(|id| onboarding.thread(id))
                .map(|t| (t.id.clone(), t.title.clone()))
        };
        let Some((thread, title)) = selected else {
            window.push_notification(Notification::info("Select a thread to share."), cx);
            return;
        };
        window.close_all_dialogs(cx);
        self.collab
            .update(cx, |collab, cx| collab.open_share_wizard(thread, title, cx));
        self.share_wizard
            .update(cx, |wizard, cx| wizard.reset(window, cx));
        let view = self.share_wizard.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Share").w(px(560.)).child(view.clone())
        });
    }

    pub(super) fn open_join_wizard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.sidebar.read(cx).onboarding.selected_workspace.clone();
        let Some(workspace) = workspace else {
            window.push_notification(Notification::info("Select a project to join into."), cx);
            return;
        };
        window.close_all_dialogs(cx);
        self.collab
            .update(cx, |collab, cx| collab.open_join_wizard(workspace, cx));
        self.join_wizard
            .update(cx, |wizard, cx| wizard.reset(window, cx));
        let view = self.join_wizard.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Join a share").w(px(520.)).child(view.clone())
        });
    }

    /// Share/Join while the server check is in flight: a short "Checking
    /// server…" dialog; the action reopens once the answer arrives.
    pub(super) fn open_checking(
        &mut self,
        open: DeferredOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.close_all_dialogs(cx);
        self.collab
            .update(cx, |collab, cx| collab.defer_open(open, cx));
        let collab = self.collab.downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = theme::palette(cx);
            let collab = collab.clone();
            // Closing it cancels the deferred action.
            let dialog = dialog.on_close(move |_, _, cx| {
                let _ = collab.update(cx, |collab, _| collab.vnext.deferred = None);
            });
            dialog.title("Collaboration").w(px(360.)).child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Spinner::new().small().color(p.accent))
                    .child("Checking server…"),
            )
        });
    }

    /// Collab store changed: run a deferred Share/Join once decided.
    pub(super) fn resume_deferred(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .collab
            .update(cx, |collab, _| collab.vnext.take_deferred());
        match open {
            Some(DeferredOpen::Share) => self.open_share(window, cx),
            Some(DeferredOpen::Join) => self.open_join(window, cx),
            None => {}
        }
    }

    /// vNext or checking: handled here. `false` means use the v2 dialog.
    pub(super) fn route_share(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.collab.read(cx).server_support() {
            ServerSupport::Checking => self.open_checking(DeferredOpen::Share, window, cx),
            ServerSupport::VNext => self.open_share_wizard(window, cx),
            ServerSupport::Legacy => return false,
        }
        true
    }

    pub(super) fn route_join(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.collab.read(cx).server_support() {
            ServerSupport::Checking => self.open_checking(DeferredOpen::Join, window, cx),
            ServerSupport::VNext => self.open_join_wizard(window, cx),
            ServerSupport::Legacy => return false,
        }
        true
    }
}
