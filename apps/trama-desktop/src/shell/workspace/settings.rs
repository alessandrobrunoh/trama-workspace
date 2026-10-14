//! Opening the Settings view.

use super::*;
use crate::settings::Section;

impl Workspace {
    pub(super) fn open_settings(
        &mut self,
        section: Option<Section>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.close_all_dialogs(cx);
        let projects = self
            .sidebar
            .read(cx)
            .onboarding
            .workspaces
            .iter()
            .map(|ws| (ws.id.to_string(), ws.title.clone()))
            .collect();
        let connected = self.connected;
        self.settings.update(cx, |settings, cx| {
            settings.reset_for_open(section, projects, connected, window, cx)
        });
        if matches!(section, Some(Section::Collaboration)) {
            self.account
                .update(cx, |account, cx| account.reset(None, window, cx));
        }
        let view = self.settings.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Settings").w(px(920.)).child(view.clone())
        });
    }
}
