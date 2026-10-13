//! vNext (collab protocol ≥ 3, E2EE) state of the collab store: the
//! negotiated server profile and the open Share/Join wizards.
//!
//! The Share/Join actions use the wizards only when [`supports_vnext`]
//! holds for the connected server; v2 servers keep the legacy dialogs.

use gpui_kit::Context;
use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_protocol::{ClientMessage, CollabCapability, CollabServerInfo, DaemonMessage};

use super::CollabStore;
use super::wizard::join_flow::JoinFlow;
use super::wizard::route::{Correlation, WizardOp, route};

mod history;
use super::wizard::share_flow::ShareFlow;
pub use history::ShareHistory;

/// How long Share/Join waits for the server check before using v2.
pub const SERVER_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// First collab protocol with `/v3` shares.
pub const MIN_VNEXT_PROTOCOL: u32 = 3;

pub fn supports_vnext(info: Option<&CollabServerInfo>) -> bool {
    info.and_then(|i| i.profile.as_ref())
        .is_some_and(|p| p.protocol >= MIN_VNEXT_PROTOCOL && p.supports(CollabCapability::E2ee))
}

/// What the Share/Join actions should open right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSupport {
    /// Discovery in flight: show "Checking server…", never the v2 dialog.
    Checking,
    VNext,
    /// Definitive answer (v2, no e2ee, failed negotiation) or signed out.
    Legacy,
}

/// A Share/Join action waiting for the server check to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredOpen {
    Share,
    Join,
}

#[derive(Debug, Default)]
pub struct VNextState {
    /// Action to open once [`ServerSupport`] is no longer `Checking`.
    pub deferred: Option<DeferredOpen>,
    /// Last discovery result, for the server it names.
    pub server: Option<CollabServerInfo>,
    /// Server URL a discovery is in flight for.
    discovering: Option<String>,
    /// Shown in the v2 dialogs when the check gave up (timeout).
    pub note: Option<String>,
    pub share: Option<ShareFlow>,
    pub join: Option<JoinFlow>,
    corr: Correlation,
    /// Per-thread share state shown in the transcript.
    pub history: ShareHistory,
}

impl VNextState {
    /// Discovery to send after a status change, if the server changed.
    /// Signing out forgets the profile (fail closed to v2 dialogs).
    pub fn on_status(
        &mut self,
        server_url: Option<&str>,
        signed_in: bool,
    ) -> Option<ClientMessage> {
        let Some(url) = server_url.filter(|_| signed_in) else {
            self.server = None;
            self.discovering = None;
            return None;
        };
        let known = self
            .server
            .as_ref()
            .is_some_and(|s| super::same_server(&s.server_url, url))
            || self
                .discovering
                .as_deref()
                .is_some_and(|d| super::same_server(d, url));
        if known {
            return None;
        }
        self.server = None;
        self.note = None;
        self.discovering = Some(url.to_owned());
        Some(ClientMessage::DiscoverCollabServer {
            server_url: Some(url.to_owned()),
        })
    }

    pub fn on_server_info(&mut self, info: &CollabServerInfo) {
        if self
            .discovering
            .as_deref()
            .is_some_and(|d| super::same_server(d, &info.server_url))
        {
            self.discovering = None;
            self.server = Some(info.clone());
        }
    }

    /// The check for `url` took too long: fall back to v2 with a note. A
    /// late answer is ignored until the next status change re-checks.
    pub fn time_out(&mut self, url: &str) -> bool {
        if self.discovering.as_deref() != Some(url) {
            return false;
        }
        self.discovering = None;
        self.note = Some("The server did not answer in time; using classic sharing.".into());
        true
    }

    pub fn supported(&self) -> bool {
        supports_vnext(self.server.as_ref())
    }

    pub fn support(&self) -> ServerSupport {
        if self.discovering.is_some() {
            ServerSupport::Checking
        } else if self.supported() {
            ServerSupport::VNext
        } else {
            ServerSupport::Legacy
        }
    }

    /// The deferred action, once the check is over.
    pub fn take_deferred(&mut self) -> Option<DeferredOpen> {
        if self.support() == ServerSupport::Checking {
            return None;
        }
        self.deferred.take()
    }

    /// Opens (or resumes) the Share wizard for `thread`. A wizard for the
    /// same thread is kept, including a request in flight.
    pub fn open_share(&mut self, thread: ThreadId, title: String) {
        if self.share.as_ref().is_some_and(|f| f.thread_id == thread) {
            return;
        }
        if self.corr.pending == Some(WizardOp::Share) {
            self.corr.pending = None;
        }
        self.share = Some(ShareFlow::new(thread, title));
    }

    /// Opens (or resumes) the Join wizard. An unfinished join is resumed;
    /// a finished one starts over.
    pub fn open_join(&mut self, workspace: WorkspaceId) {
        use super::wizard::join_flow::JoinStep;
        let resumable = self
            .join
            .as_ref()
            .is_some_and(|f| !matches!(f.step, JoinStep::Enter | JoinStep::Joined(_)));
        if !resumable {
            if self.corr.pending == Some(WizardOp::Join) {
                self.corr.pending = None;
            }
            self.join = Some(JoinFlow::new(workspace));
        }
    }

    /// Routes a wizard reply. Returns `(handled, follow_ups)`.
    pub fn apply(&mut self, message: &DaemonMessage) -> (bool, Vec<ClientMessage>) {
        match message {
            DaemonMessage::CollabServerInfo(info) => {
                self.on_server_info(info);
                return (true, Vec::new());
            }
            DaemonMessage::ShareHistoryStatus { thread_id, entries } => {
                self.history.apply(thread_id, entries);
                return (true, Vec::new());
            }
            DaemonMessage::Shares(list) => self.history.apply_shares(list),
            _ => {}
        }
        let r = route(
            message,
            &mut self.corr,
            self.share.as_mut(),
            self.join.as_mut(),
        );
        if !r.handled && matches!(message, DaemonMessage::Error { .. }) {
            // An uncorrelated error while checking: treat the check as
            // failed (v2 fallback) but let the error reach its owner.
            self.discovering = None;
        }
        (r.handled, r.follow_ups)
    }

    /// Marks `op` as awaiting a reply (for uncorrelated errors).
    pub fn sent(&mut self, op: WizardOp) {
        self.corr.sent(op);
    }

    pub fn fail(&mut self, op: WizardOp, message: String) {
        match op {
            WizardOp::Share => {
                if let Some(f) = self.share.as_mut() {
                    f.apply_error(message);
                }
            }
            WizardOp::Join => {
                if let Some(f) = self.join.as_mut() {
                    f.apply_error(message);
                }
            }
        }
    }
}

impl CollabStore {
    fn arm_check_timeout(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.vnext.discovering.clone() else {
            return;
        };
        self._check_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SERVER_CHECK_TIMEOUT).await;
            let _ = this.update(cx, |this, cx| {
                if this.vnext.time_out(&url) {
                    cx.notify();
                }
            });
        }));
    }

    pub fn server_support(&self) -> ServerSupport {
        self.vnext.support()
    }

    pub fn open_share_wizard(&mut self, thread: ThreadId, title: String, cx: &mut Context<Self>) {
        self.vnext.open_share(thread, title);
        cx.notify();
    }

    pub fn open_join_wizard(&mut self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        self.vnext.open_join(workspace);
        cx.notify();
    }

    /// Remember an action to open once the server check is over.
    pub fn defer_open(&mut self, open: DeferredOpen, cx: &mut Context<Self>) {
        self.vnext.deferred = Some(open);
        cx.notify();
    }

    /// Sends a wizard message; a dead daemon connection fails the step.
    pub fn dispatch_wizard(
        &mut self,
        op: WizardOp,
        message: Option<ClientMessage>,
        cx: &mut Context<Self>,
    ) {
        if let Some(message) = message {
            if self.send(message) {
                self.vnext.sent(op);
            } else {
                self.vnext.fail(op, "The daemon is not connected.".into());
            }
        }
        cx.notify();
    }

    /// Per-message share state of the selected thread (vNext servers).
    pub(super) fn request_history(&mut self, cx: &mut Context<Self>) {
        if !self.vnext.supported() {
            return;
        }
        if let Some(thread_id) = self.selected.clone() {
            self.send(ClientMessage::GetShareHistoryStatus { thread_id });
            cx.notify();
        }
    }

    /// Hook for [`CollabStore::apply`]: status changes trigger discovery,
    /// wizard replies are routed. Returns whether the message is consumed.
    pub(super) fn apply_vnext(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        if let DaemonMessage::CollabStatus(status) = message {
            if let Some(discover) = self
                .vnext
                .on_status(status.server_url.as_deref(), status.signed_in)
            {
                self.send(discover);
                self.arm_check_timeout(cx);
            }
            return false;
        }
        let was_checking = self.vnext.support() == ServerSupport::Checking;
        let (handled, follow_ups) = self.vnext.apply(message);
        if let DaemonMessage::ShareFailed {
            share_id: Some(_), ..
        } = message
            && self.vnext.supported()
        {
            // The daemon re-checks access after a failure and may have
            // closed the share locally: refresh share states.
            self.send(ClientMessage::ListShares);
        }
        if was_checking && self.vnext.support() != ServerSupport::Checking {
            if self.vnext.supported() {
                // Share states (closed shares) for the transcript banners.
                self.send(ClientMessage::ListShares);
                self.request_history(cx);
            }
            cx.notify();
        }
        for message in follow_ups {
            self.dispatch_wizard(WizardOp::Share, Some(message), cx);
        }
        if handled {
            cx.notify();
        }
        handled
    }
}

#[cfg(test)]
mod tests;
