//! Wizard inputs/outputs, independent of the transport.

use trama_domain::ids::ThreadId;
use trama_domain::secrets::SecretRule;
use trama_domain::share::{
    Capability, CodeAccessMode, JoinState, ShareRole, ShareScope, ShareStatus,
};

/// What the owner asks to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRequest {
    pub root: ThreadId,
    pub scope: ShareScope,
    /// New descendants join the share automatically (not for `thread_only`).
    pub include_future_descendants: bool,
    /// Role preset for invitees.
    pub role: ShareRole,
    /// Invitee capabilities: the role preset, possibly narrowed. Never wider.
    pub capabilities: Vec<Capability>,
    pub code_access: CodeAccessMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictBlocker {
    pub repo: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretBlocker {
    pub repo: String,
    pub path: String,
    pub line: Option<u32>,
    pub rule: SecretRule,
    /// Redacted by the scanner; never the secret itself.
    pub hint: String,
}

/// A scout whose checkout owner is outside the share (refused in v1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoutBlocker {
    pub thread_title: String,
    pub parent_title: String,
}

/// Result of the daemon's share preflight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightReport {
    pub threads: usize,
    pub repos: usize,
    pub bytes: u64,
    pub conflicts: Vec<ConflictBlocker>,
    pub secrets: Vec<SecretBlocker>,
    pub scouts_outside: Vec<ScoutBlocker>,
}

impl PreflightReport {
    pub fn blocker_count(&self) -> usize {
        self.conflicts.len() + self.secrets.len() + self.scouts_outside.len()
    }

    pub fn blocks(&self) -> bool {
        self.blocker_count() > 0
    }
}

/// A created share as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedShare {
    pub share_id: String,
    pub status: ShareStatus,
    /// Only present once `status` is `Ready` (links never promise data
    /// that is not uploaded yet, plan 02 §4).
    pub link: Option<String>,
}

/// Progress report for a join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinProgress {
    pub state: JoinState,
    pub detail: Option<String>,
}

/// Everything the wizards ask the daemon to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WizardCommand {
    Preflight(ShareRequest),
    CreateShare(ShareRequest),
    Join {
        link: String,
    },
    /// Resume a failed join from its persisted cursor.
    RetryJoin {
        link: String,
    },
    CancelJoin {
        link: String,
    },
    /// Start GitHub OAuth (the only v1 Git authorization).
    ConnectGitHub,
}

/// Sink for wizard commands; the collab store implements it once the IPC
/// messages exist. Tests record.
pub trait WizardBackend {
    fn send(&mut self, command: WizardCommand);
}

impl WizardBackend for Vec<WizardCommand> {
    fn send(&mut self, command: WizardCommand) {
        self.push(command);
    }
}
