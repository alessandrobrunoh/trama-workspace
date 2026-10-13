//! Routes daemon replies to the open wizards. Kept pure so the collab
//! store only has to call [`route`] and send the follow-up it returns.

use trama_protocol::{ClientMessage, DaemonMessage};

use super::join_flow::JoinFlow;
use super::share_flow::ShareFlow;

/// Which wizard sent the request currently awaiting a reply; a
/// `DaemonMessage::Error` carries no correlation id, so it goes to this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardOp {
    Share,
    Join,
}

#[derive(Debug, Default)]
pub struct Routed {
    pub handled: bool,
    pub follow_up: Option<ClientMessage>,
}

pub fn route(
    message: &DaemonMessage,
    pending: &mut Option<WizardOp>,
    share: Option<&mut ShareFlow>,
    join: Option<&mut JoinFlow>,
) -> Routed {
    let mut out = Routed::default();
    match message {
        DaemonMessage::SharePreflightReport(report) => {
            if let Some(f) = share {
                f.apply_preflight(report.clone());
                out.handled = true;
            }
        }
        DaemonMessage::ShareCreated(snapshot) => {
            if let Some(f) = share {
                out.follow_up = f.apply_created(snapshot.clone());
                out.handled = true;
            }
        }
        DaemonMessage::ShareJoinRequests { share_id, requests } => {
            if let Some(f) = share {
                f.apply_requests(share_id, requests.clone());
                out.handled = true;
            }
        }
        DaemonMessage::ShareJoinApproved {
            share_id,
            device_id,
            key_epoch,
        } => {
            if let Some(f) = share {
                f.apply_approved(share_id, device_id, *key_epoch);
                out.handled = true;
            }
        }
        DaemonMessage::ShareJoinRequested {
            share_id,
            device_id,
        } => {
            if let Some(f) = join {
                f.apply_requested(share_id, device_id.clone());
                out.handled = true;
            }
        }
        DaemonMessage::ShareJoined(snapshot) => {
            if let Some(f) = join {
                f.apply_joined(snapshot.clone());
                out.handled = true;
            }
        }
        DaemonMessage::Error { message, .. } => match *pending {
            Some(WizardOp::Share) => {
                if let Some(f) = share {
                    f.apply_error(message.clone());
                }
                out.handled = true;
            }
            Some(WizardOp::Join) => {
                if let Some(f) = join {
                    f.apply_error(message.clone());
                }
                out.handled = true;
            }
            None => {}
        },
        _ => return out,
    }
    if out.handled {
        *pending = None;
    }
    if out.follow_up.is_some() {
        *pending = Some(WizardOp::Share);
    }
    out
}
