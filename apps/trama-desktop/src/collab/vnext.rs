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
use super::wizard::route::{WizardOp, route};
use super::wizard::share_flow::ShareFlow;

/// First collab protocol with `/v3` shares.
pub const MIN_VNEXT_PROTOCOL: u32 = 3;

pub fn supports_vnext(info: Option<&CollabServerInfo>) -> bool {
    info.and_then(|i| i.profile.as_ref())
        .is_some_and(|p| p.protocol >= MIN_VNEXT_PROTOCOL && p.supports(CollabCapability::E2ee))
}

#[derive(Debug, Default)]
pub struct VNextState {
    /// Last discovery result, for the server it names.
    pub server: Option<CollabServerInfo>,
    /// Server URL a discovery is in flight for.
    discovering: Option<String>,
    pub share: Option<ShareFlow>,
    pub join: Option<JoinFlow>,
    pending: Option<WizardOp>,
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

    pub fn supported(&self) -> bool {
        supports_vnext(self.server.as_ref())
    }

    /// Routes a wizard reply. Returns `(handled, follow_up)`.
    pub fn apply(&mut self, message: &DaemonMessage) -> (bool, Option<ClientMessage>) {
        if let DaemonMessage::CollabServerInfo(info) = message {
            self.on_server_info(info);
            return (true, None);
        }
        if matches!(message, DaemonMessage::Error { .. }) && self.pending.is_none() {
            return (false, None);
        }
        let r = route(
            message,
            &mut self.pending,
            self.share.as_mut(),
            self.join.as_mut(),
        );
        (r.handled, r.follow_up)
    }

    /// Marks `op` as awaiting a reply (for uncorrelated errors).
    pub fn sent(&mut self, op: WizardOp) {
        self.pending = Some(op);
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
    pub fn vnext_supported(&self) -> bool {
        self.vnext.supported()
    }

    pub fn open_share_wizard(&mut self, thread: ThreadId, title: String, cx: &mut Context<Self>) {
        self.vnext.share = Some(ShareFlow::new(thread, title));
        cx.notify();
    }

    pub fn open_join_wizard(&mut self, workspace: WorkspaceId, cx: &mut Context<Self>) {
        self.vnext.join = Some(JoinFlow::new(workspace));
        cx.notify();
    }

    pub fn close_wizards(&mut self, cx: &mut Context<Self>) {
        self.vnext.share = None;
        self.vnext.join = None;
        self.vnext.pending = None;
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

    /// Hook for [`CollabStore::apply`]: status changes trigger discovery,
    /// wizard replies are routed. Returns whether the message is consumed.
    pub(super) fn apply_vnext(&mut self, message: &DaemonMessage, cx: &mut Context<Self>) -> bool {
        if let DaemonMessage::CollabStatus(status) = message {
            if let Some(discover) = self
                .vnext
                .on_status(status.server_url.as_deref(), status.signed_in)
            {
                self.send(discover);
            }
            return false;
        }
        let (handled, follow_up) = self.vnext.apply(message);
        if follow_up.is_some() {
            self.dispatch_wizard(WizardOp::Share, follow_up, cx);
        }
        if handled {
            cx.notify();
        }
        handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_protocol::NegotiatedProfile;

    fn info(url: &str, protocol: u32, caps: Vec<CollabCapability>) -> CollabServerInfo {
        CollabServerInfo {
            server_url: url.into(),
            discovery: None,
            profile: Some(NegotiatedProfile {
                server_id: "s".into(),
                protocol,
                capabilities: caps,
                limits: Default::default(),
                policy: Default::default(),
            }),
            error: None,
        }
    }

    #[test]
    fn vnext_needs_protocol_3_and_e2ee() {
        assert!(!supports_vnext(None));
        assert!(!supports_vnext(Some(&info(
            "u",
            2,
            vec![CollabCapability::E2ee]
        ))));
        assert!(!supports_vnext(Some(&info("u", 3, vec![]))));
        assert!(supports_vnext(Some(&info(
            "u",
            3,
            vec![CollabCapability::E2ee]
        ))));
        let mut legacy = info("u", 3, vec![CollabCapability::E2ee]);
        legacy.profile = None;
        assert!(
            !supports_vnext(Some(&legacy)),
            "failed negotiation falls back to v2"
        );
    }

    #[test]
    fn discovery_runs_once_per_server_and_resets_on_sign_out() {
        let mut v = VNextState::default();
        assert!(v.on_status(Some("https://a"), false).is_none());
        assert!(matches!(
            v.on_status(Some("https://a"), true),
            Some(ClientMessage::DiscoverCollabServer { server_url: Some(u) }) if u == "https://a"
        ));
        assert!(v.on_status(Some("https://a/"), true).is_none(), "in flight");
        v.on_server_info(&info("https://other", 3, vec![CollabCapability::E2ee]));
        assert!(!v.supported(), "reply for another server is ignored");
        let (handled, _) = v.apply(&DaemonMessage::CollabServerInfo(info(
            "https://a",
            3,
            vec![CollabCapability::E2ee],
        )));
        assert!(handled && v.supported());
        assert!(v.on_status(Some("https://a"), true).is_none());
        assert!(
            v.on_status(Some("https://b"), true).is_some(),
            "server changed"
        );
        assert!(!v.supported());
        v.on_status(None, false);
        assert!(!v.supported());
    }

    #[test]
    fn errors_only_reach_a_wizard_that_is_waiting() {
        let mut v = VNextState::default();
        v.join = Some(JoinFlow::new(WorkspaceId::from("w")));
        let err = DaemonMessage::Error {
            code: "x".into(),
            message: "boom".into(),
        };
        assert!(!v.apply(&err).0, "v2 errors pass through");
        let start = v.join.as_mut().unwrap().start("s1");
        assert!(start.is_some());
        v.sent(WizardOp::Join);
        assert!(v.apply(&err).0);
        assert!(matches!(
            v.join.as_ref().unwrap().step,
            super::super::wizard::join_flow::JoinStep::RequestFailed(_)
        ));
        assert!(!v.apply(&err).0);
    }

    #[test]
    fn dead_connection_fails_the_step() {
        let mut v = VNextState::default();
        v.share = Some(ShareFlow::new(ThreadId::from("t"), "T".into()));
        let f = v.share.as_mut().unwrap();
        f.next();
        f.next();
        v.fail(WizardOp::Share, "down".into());
        assert!(matches!(
            v.share.as_ref().unwrap().step,
            super::super::wizard::share_flow::ShareStep::Failed(_)
        ));
    }
}
