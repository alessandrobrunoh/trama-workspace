//! Routes daemon replies to the open wizards. Kept pure so the collab
//! store only has to call [`route`] and send the follow-ups it returns.
//!
//! Errors: protocol 16 sends `ShareFailed { operation, correlation_id }`
//! right before the matching `Error` (whose text ends with
//! `[ref <correlation_id>]`). `ShareFailed` is routed by its operation and
//! the paired `Error` is swallowed; a bare `Error` (older daemons, other
//! requests) still goes to the wizard awaiting a reply.

use trama_protocol::{ClientMessage, DaemonMessage};

use super::join_flow::JoinFlow;
use super::share_flow::ShareFlow;

/// Which wizard sent the request currently awaiting a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardOp {
    Share,
    Join,
}

#[derive(Debug, Default)]
pub struct Routed {
    pub handled: bool,
    pub follow_ups: Vec<ClientMessage>,
}

/// Error-correlation state kept between messages.
#[derive(Debug, Default)]
pub struct Correlation {
    pub pending: Option<WizardOp>,
    /// `ShareFailed` already delivered; its `Error` is a duplicate.
    swallow: Option<String>,
}

impl Correlation {
    pub fn sent(&mut self, op: WizardOp) {
        self.pending = Some(op);
    }

    /// A `ShareFailed` handled elsewhere: drop its paired `Error`.
    pub fn swallow_error(&mut self, correlation_id: &str) {
        self.swallow = Some(correlation_id.to_owned());
    }
}

/// Wizard of a share command, by its client message tag.
pub fn op_of(operation: &str) -> Option<WizardOp> {
    match operation {
        "share_preflight"
        | "create_share"
        | "list_share_join_requests"
        | "approve_share_join"
        | "remove_share_member"
        | "list_share_members"
        | "publish_share_snapshot" => Some(WizardOp::Share),
        "request_share_join" | "complete_share_join" => Some(WizardOp::Join),
        _ => None,
    }
}

/// Codes after which the share is over for this device.
pub fn ending(code: &str) -> Option<&'static str> {
    match code {
        "share_closed" => Some("This share was closed."),
        "access_revoked" | "not_a_member" => Some("Your access to this share was revoked."),
        _ => None,
    }
}

fn fail(
    op: WizardOp,
    code: Option<&str>,
    text: String,
    share: Option<&mut ShareFlow>,
    join: Option<&mut JoinFlow>,
) {
    match op {
        WizardOp::Share => {
            if let Some(f) = share {
                if let Some(why) = code.and_then(ending) {
                    f.end(why.into());
                }
                f.apply_error(text);
            }
        }
        WizardOp::Join => {
            if let Some(f) = join {
                f.apply_error(code.and_then(ending).map(str::to_owned).unwrap_or(text));
            }
        }
    }
}

pub fn route(
    message: &DaemonMessage,
    corr: &mut Correlation,
    mut share: Option<&mut ShareFlow>,
    join: Option<&mut JoinFlow>,
) -> Routed {
    let mut out = Routed::default();
    let mut on_share = |f: &mut dyn FnMut(&mut ShareFlow) -> Vec<ClientMessage>| {
        if let Some(flow) = share.as_deref_mut() {
            let follow = f(flow);
            (true, follow)
        } else {
            (false, Vec::new())
        }
    };
    let (handled, follow_ups) = match message {
        DaemonMessage::SharePreflightReport(report) => on_share(&mut |f| {
            f.apply_preflight(report.clone());
            Vec::new()
        }),
        DaemonMessage::ShareCreated(snapshot) => {
            on_share(&mut |f| f.apply_created(snapshot.clone()))
        }
        DaemonMessage::Shares(list) => {
            // Status refresh; never consumes the reply (others may list).
            on_share(&mut |f| {
                list.iter().for_each(|s| f.apply_status(s));
                Vec::new()
            });
            return out;
        }
        DaemonMessage::ShareJoinRequests { share_id, requests } => on_share(&mut |f| {
            f.apply_requests(share_id, requests.clone());
            Vec::new()
        }),
        DaemonMessage::ShareMembers { share_id, members } => on_share(&mut |f| {
            f.apply_members(share_id, members.clone());
            Vec::new()
        }),
        DaemonMessage::ShareJoinApproved {
            share_id,
            device_id,
            key_epoch,
        } => on_share(&mut |f| {
            f.apply_approved(share_id, device_id, *key_epoch)
                .into_iter()
                .collect()
        }),
        DaemonMessage::ShareMemberRemoved {
            share_id,
            username,
            key_epoch,
            ..
        } => on_share(&mut |f| {
            f.apply_removed(share_id, username, *key_epoch);
            Vec::new()
        }),
        DaemonMessage::ShareJoinRequested {
            share_id,
            device_id,
        } => match join {
            Some(f) => {
                f.apply_requested(share_id, device_id.clone());
                (true, Vec::new())
            }
            None => (false, Vec::new()),
        },
        DaemonMessage::ShareJoined(snapshot) => match join {
            Some(f) => {
                f.apply_joined(snapshot.clone());
                (true, Vec::new())
            }
            None => (false, Vec::new()),
        },
        DaemonMessage::ShareFailed {
            operation,
            correlation_id,
            code,
            message,
            ..
        } => {
            let Some(op) = op_of(operation) else {
                return out; // not a wizard command: leave both to others
            };
            corr.swallow = Some(correlation_id.clone());
            {
                let text = format!("{message} (ref {correlation_id})");
                fail(op, Some(code), text, share, join);
                if corr.pending == Some(op) {
                    corr.pending = None;
                }
            }
            out.handled = true;
            return out;
        }
        DaemonMessage::Error { message, .. } => {
            if let Some(id) = corr.swallow.take()
                && message.ends_with(&format!("[ref {id}]"))
            {
                out.handled = true;
                return out;
            }
            match corr.pending.take() {
                Some(op) => {
                    fail(op, None, message.clone(), share, join);
                    (true, Vec::new())
                }
                None => (false, Vec::new()),
            }
        }
        _ => return out,
    };
    out.handled = handled;
    out.follow_ups = follow_ups;
    if handled {
        corr.pending = None;
    }
    if !out.follow_ups.is_empty() {
        corr.pending = Some(WizardOp::Share);
    }
    out
}
