//! Opening the vNext Share/Join wizards (servers with protocol ≥ 3 + E2EE).

use super::*;

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
        let collab = self.collab.downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let collab = collab.clone();
            dialog
                .title("Share")
                .w(px(560.))
                .on_close(move |_, _, cx| {
                    let _ = collab.update(cx, |collab, cx| collab.close_wizards(cx));
                })
                .child(view.clone())
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
        let collab = self.collab.downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let collab = collab.clone();
            dialog
                .title("Join a share")
                .w(px(520.))
                .on_close(move |_, _, cx| {
                    let _ = collab.update(cx, |collab, cx| collab.close_wizards(cx));
                })
                .child(view.clone())
        });
    }
}
