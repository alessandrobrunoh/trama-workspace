//! Join wizard state machine: paste link → follow daemon progress.

use trama_domain::share::JoinState;

use super::model::{JoinProgress, WizardBackend, WizardCommand};

/// Happy-path steps shown as a checklist, in order.
pub const JOIN_STEPS: [JoinState; 7] = [
    JoinState::Invited,
    JoinState::Authorized,
    JoinState::FetchingGit,
    JoinState::FetchingContent,
    JoinState::Verifying,
    JoinState::Materializing,
    JoinState::Ready,
];

const MAX_LINK_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepMark {
    Done,
    Current,
    Failed,
    Pending,
}

/// What the user can do about a join error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinRemedy {
    Retry,
    ConnectGitHub,
    /// Nothing to do locally (policy, integrity): only cancel.
    None,
}

#[derive(Debug, Clone, Default)]
pub struct JoinFlow {
    pub link: String,
    /// `None` until Join is pressed.
    pub state: Option<JoinState>,
    /// Furthest happy-path step reached, to keep the checklist after errors.
    reached: Option<JoinState>,
    pub detail: Option<String>,
    pub error: Option<String>,
}

pub fn step_index(state: JoinState) -> Option<usize> {
    JOIN_STEPS.iter().position(|s| *s == state)
}

impl JoinFlow {
    /// Links are opaque to the desktop; the daemon parses and verifies them.
    pub fn validate_link(raw: &str) -> Result<String, String> {
        let link = raw.trim();
        if link.is_empty() {
            return Err("Paste the share link you received.".into());
        }
        if link.len() > MAX_LINK_LEN || link.chars().any(char::is_whitespace) {
            return Err("That doesn't look like a share link.".into());
        }
        Ok(link.to_owned())
    }

    pub fn start(&mut self, raw: &str, backend: &mut dyn WizardBackend) -> bool {
        if self.state.is_some() {
            return false;
        }
        match Self::validate_link(raw) {
            Ok(link) => {
                *self = Self {
                    link: link.clone(),
                    state: Some(JoinState::Invited),
                    reached: Some(JoinState::Invited),
                    ..Self::default()
                };
                backend.send(WizardCommand::Join { link });
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }

    /// Applies a daemon progress report; illegal transitions (stale or
    /// reordered reports) are ignored.
    pub fn apply(&mut self, progress: JoinProgress) {
        let Some(current) = self.state else { return };
        if current != progress.state && !current.can_transition_to(progress.state) {
            return;
        }
        self.state = Some(progress.state);
        self.detail = progress.detail;
        if step_index(progress.state).is_some() {
            self.reached = Some(progress.state);
        }
    }

    pub fn mark(&self, step: JoinState) -> StepMark {
        let (Some(state), Some(reached)) = (self.state, self.reached) else {
            return StepMark::Pending;
        };
        let (i, r) = (step_index(step), step_index(reached));
        match (i, r) {
            (Some(i), Some(r)) if i < r => StepMark::Done,
            (Some(i), Some(r)) if i == r && state == JoinState::Ready => StepMark::Done,
            (Some(i), Some(r)) if i == r && state.is_error() => StepMark::Failed,
            (Some(i), Some(r)) if i == r => StepMark::Current,
            _ => StepMark::Pending,
        }
    }

    pub fn remedy(&self) -> Option<JoinRemedy> {
        let state = self.state.filter(|s| s.is_error())?;
        Some(match state {
            JoinState::RetryableNetworkError
            | JoinState::WaitingForHost
            | JoinState::MissingObject => JoinRemedy::Retry,
            JoinState::NeedsGitAccess | JoinState::PartialNoCode => JoinRemedy::ConnectGitHub,
            _ => JoinRemedy::None,
        })
    }

    pub fn retry(&mut self, backend: &mut dyn WizardBackend) {
        if self.remedy() == Some(JoinRemedy::Retry) {
            backend.send(WizardCommand::RetryJoin {
                link: self.link.clone(),
            });
        }
    }

    pub fn connect_github(&mut self, backend: &mut dyn WizardBackend) {
        if self.remedy() == Some(JoinRemedy::ConnectGitHub) {
            backend.send(WizardCommand::ConnectGitHub);
        }
    }

    /// Cancels an unfinished join (staging is private, so nothing partial
    /// is ever left as a usable checkout) and resets the wizard.
    pub fn cancel(&mut self, backend: &mut dyn WizardBackend) {
        if self.state.is_some_and(|s| s != JoinState::Ready) {
            backend.send(WizardCommand::CancelJoin {
                link: self.link.clone(),
            });
        }
        *self = Self::default();
    }

    pub fn is_ready(&self) -> bool {
        self.state.is_some_and(JoinState::is_operational)
    }
}

pub fn step_label(state: JoinState) -> &'static str {
    match state {
        JoinState::Invited => "Link accepted",
        JoinState::Authorized => "Membership authorized",
        JoinState::FetchingGit => "Fetching from Git origin",
        JoinState::FetchingContent => "Downloading encrypted content",
        JoinState::Verifying => "Verifying signatures and integrity",
        JoinState::Materializing => "Preparing isolated checkouts",
        JoinState::Ready => "Ready",
        JoinState::NeedsGitAccess => "Your GitHub account needs access to the repositories.",
        JoinState::MissingObject => {
            "Some Git objects are missing; the host may still be uploading."
        }
        JoinState::ForbiddenByPolicy => "Your organization's policy forbids this share.",
        JoinState::IntegrityFailed => "Integrity check failed. Nothing was materialized.",
        JoinState::WaitingForHost => "Waiting for the host to finish uploading.",
        JoinState::PartialNoCode => "History is available, but code needs GitHub access.",
        JoinState::Conflict => "The share changed in a conflicting way.",
        JoinState::RetryableNetworkError => "Network error. You can retry; progress is kept.",
    }
}
