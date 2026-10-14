//! Join wizard state machine: share id → key package published → owner
//! approval → snapshot downloaded and verified → ready.

use trama_domain::ids::WorkspaceId;
use trama_protocol::{ClientMessage, ShareSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinStage {
    Request,
    Approval,
    Download,
    Ready,
}

pub const JOIN_STAGES: [JoinStage; 4] = [
    JoinStage::Request,
    JoinStage::Approval,
    JoinStage::Download,
    JoinStage::Ready,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinStep {
    Enter,
    Requesting,
    /// Key package published; the owner must admit `device_id`.
    AwaitingApproval {
        device_id: String,
    },
    /// `CompleteShareJoin` in flight: Welcome, manifests, local threads.
    Completing {
        device_id: String,
    },
    Joined(ShareSnapshot),
    /// The key package could not be published.
    RequestFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageMark {
    Done,
    Current,
    Failed,
    Pending,
}

#[derive(Debug, Clone)]
pub struct JoinFlow {
    pub workspace_id: WorkspaceId,
    pub share_id: String,
    pub step: JoinStep,
    /// Last error of a completion attempt (e.g. not approved yet); the
    /// wizard stays waiting and lets the user check again.
    pub error: Option<String>,
}

/// Share ids follow the collab id alphabet: 1–128 of `[A-Za-z0-9-_.:]`.
pub fn validate_share_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("Paste the share ID you received from the owner.".into());
    }
    let ok = id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if !ok {
        return Err("That doesn't look like a share ID.".into());
    }
    Ok(id.to_owned())
}

impl JoinFlow {
    pub fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            workspace_id,
            share_id: String::new(),
            step: JoinStep::Enter,
            error: None,
        }
    }

    pub fn start(&mut self, raw: &str) -> Option<ClientMessage> {
        if !matches!(self.step, JoinStep::Enter | JoinStep::RequestFailed(_)) {
            return None;
        }
        match validate_share_id(raw) {
            Ok(id) => {
                self.share_id = id.clone();
                self.step = JoinStep::Requesting;
                self.error = None;
                Some(ClientMessage::RequestShareJoin { share_id: id })
            }
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }

    /// Retry publishing the key package after a failure.
    pub fn retry_request(&mut self) -> Option<ClientMessage> {
        let id = self.share_id.clone();
        matches!(self.step, JoinStep::RequestFailed(_))
            .then(|| self.start(&id))
            .flatten()
    }

    pub fn apply_requested(&mut self, share_id: &str, device_id: String) {
        if self.step == JoinStep::Requesting && share_id == self.share_id {
            self.step = JoinStep::AwaitingApproval { device_id };
        }
    }

    /// "Check again": try to complete; succeeds once the owner approved.
    pub fn complete(&mut self) -> Option<ClientMessage> {
        let JoinStep::AwaitingApproval { device_id } = &self.step else {
            return None;
        };
        self.step = JoinStep::Completing {
            device_id: device_id.clone(),
        };
        self.error = None;
        Some(ClientMessage::CompleteShareJoin {
            share_id: self.share_id.clone(),
            workspace_id: self.workspace_id.clone(),
        })
    }

    pub fn apply_joined(&mut self, share: ShareSnapshot) {
        if matches!(self.step, JoinStep::Completing { .. }) && share.share_id == self.share_id {
            self.step = JoinStep::Joined(share);
        }
    }

    pub fn apply_error(&mut self, message: String) {
        match &self.step {
            JoinStep::Requesting => self.step = JoinStep::RequestFailed(message),
            JoinStep::Completing { device_id } => {
                self.step = JoinStep::AwaitingApproval {
                    device_id: device_id.clone(),
                };
                self.error = Some(message);
            }
            _ => {}
        }
    }

    /// Back to the start. Nothing partial is usable: the daemon only
    /// creates local threads when `CompleteShareJoin` succeeds.
    pub fn reset(&mut self) {
        *self = Self::new(self.workspace_id.clone());
    }

    pub fn current_stage(&self) -> Option<(JoinStage, bool)> {
        Some(match self.step {
            JoinStep::Enter => return None,
            JoinStep::Requesting => (JoinStage::Request, false),
            JoinStep::RequestFailed(_) => (JoinStage::Request, true),
            JoinStep::AwaitingApproval { .. } => (JoinStage::Approval, self.error.is_some()),
            JoinStep::Completing { .. } => (JoinStage::Download, false),
            JoinStep::Joined(_) => (JoinStage::Ready, false),
        })
    }

    pub fn mark(&self, stage: JoinStage) -> StageMark {
        let Some((current, failed)) = self.current_stage() else {
            return StageMark::Pending;
        };
        let pos = |s| JOIN_STAGES.iter().position(|x| *x == s);
        match pos(stage).cmp(&pos(current)) {
            std::cmp::Ordering::Less => StageMark::Done,
            std::cmp::Ordering::Equal if current == JoinStage::Ready => StageMark::Done,
            std::cmp::Ordering::Equal if failed => StageMark::Failed,
            std::cmp::Ordering::Equal => StageMark::Current,
            std::cmp::Ordering::Greater => StageMark::Pending,
        }
    }
}

pub fn stage_label(stage: JoinStage) -> &'static str {
    match stage {
        JoinStage::Request => "Publish this device's encryption key",
        JoinStage::Approval => "Owner admits this device",
        JoinStage::Download => "Download, decrypt and verify the snapshot",
        JoinStage::Ready => "Ready",
    }
}
