//! Share wizard state machine: Scope → Access → Checks → Creating → Done.

use trama_domain::ids::ThreadId;
use trama_domain::share::{Capability, CodeAccessMode, ShareRole, ShareScope, ShareStatus};

use super::model::{CreatedShare, PreflightReport, ShareRequest, WizardBackend, WizardCommand};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareStep {
    Scope,
    Access,
    /// Preflight requested, no report yet.
    Checking,
    /// Report received; `Share` is enabled only without blockers.
    Checked(PreflightReport),
    Creating,
    Created(CreatedShare),
    Failed(String),
}

/// Roles an owner may hand out (owner itself is never delegated).
pub const INVITEE_ROLES: [ShareRole; 3] =
    [ShareRole::Editor, ShareRole::Reviewer, ShareRole::Reader];

#[derive(Debug, Clone)]
pub struct ShareFlow {
    pub root: ThreadId,
    pub title: String,
    pub step: ShareStep,
    pub scope: ShareScope,
    pub include_future_descendants: bool,
    pub role: ShareRole,
    /// Capabilities removed from the role preset.
    removed: Vec<Capability>,
    pub code_access: CodeAccessMode,
}

impl ShareFlow {
    /// Conservative defaults: only this thread, read-only role, code only
    /// after a verified Git permission proof.
    pub fn new(root: ThreadId, title: String) -> Self {
        Self {
            root,
            title,
            step: ShareStep::Scope,
            scope: ShareScope::ThreadOnly,
            include_future_descendants: false,
            role: ShareRole::Reader,
            removed: Vec::new(),
            code_access: CodeAccessMode::GitAuthorizationRequired,
        }
    }

    pub fn set_scope(&mut self, scope: ShareScope) {
        self.scope = scope;
        if !scope.includes_descendants() {
            self.include_future_descendants = false;
        }
    }

    pub fn set_future_descendants(&mut self, on: bool) {
        self.include_future_descendants = on && self.scope.includes_descendants();
    }

    pub fn set_role(&mut self, role: ShareRole) {
        if INVITEE_ROLES.contains(&role) {
            self.role = role;
            self.removed.clear();
        }
    }

    /// Capabilities of the preset. `read_code` is listed when the mode
    /// can grant it; with `GitAuthorizationRequired` it is only issued
    /// after the proof, which the view states.
    pub fn preset(&self) -> Vec<Capability> {
        self.role.default_capabilities(true).0.into_iter().collect()
    }

    pub fn has(&self, cap: Capability) -> bool {
        self.preset().contains(&cap) && !self.removed.contains(&cap)
    }

    /// Toggles a preset capability off/on. `read_history` is the base of
    /// every share and cannot be removed; nothing outside the preset can
    /// be added.
    pub fn toggle(&mut self, cap: Capability) {
        if cap == Capability::ReadHistory || !self.preset().contains(&cap) {
            return;
        }
        if let Some(i) = self.removed.iter().position(|c| *c == cap) {
            self.removed.remove(i);
        } else {
            self.removed.push(cap);
        }
    }

    pub fn capabilities(&self) -> Vec<Capability> {
        self.preset().into_iter().filter(|c| self.has(*c)).collect()
    }

    pub fn request(&self) -> ShareRequest {
        ShareRequest {
            root: self.root.clone(),
            scope: self.scope,
            include_future_descendants: self.include_future_descendants,
            role: self.role,
            capabilities: self.capabilities(),
            code_access: self.code_access,
        }
    }

    pub fn can_go_back(&self) -> bool {
        matches!(
            self.step,
            ShareStep::Access | ShareStep::Checked(_) | ShareStep::Failed(_)
        )
    }

    /// Back also discards a preflight report: it described other choices.
    pub fn back(&mut self) {
        self.step = match self.step {
            ShareStep::Access => ShareStep::Scope,
            ShareStep::Checked(_) | ShareStep::Failed(_) => ShareStep::Access,
            _ => return,
        };
    }

    /// Primary button. Returns whether a command was sent.
    pub fn next(&mut self, backend: &mut dyn WizardBackend) -> bool {
        match &self.step {
            ShareStep::Scope => {
                self.step = ShareStep::Access;
                false
            }
            ShareStep::Access => self.run_checks(backend),
            ShareStep::Checked(report) if !report.blocks() => {
                self.step = ShareStep::Creating;
                backend.send(WizardCommand::CreateShare(self.request()));
                true
            }
            _ => false,
        }
    }

    /// (Re)runs preflight, e.g. after the user fixed blockers.
    pub fn run_checks(&mut self, backend: &mut dyn WizardBackend) -> bool {
        if matches!(
            self.step,
            ShareStep::Checking | ShareStep::Creating | ShareStep::Created(_)
        ) {
            return false;
        }
        self.step = ShareStep::Checking;
        backend.send(WizardCommand::Preflight(self.request()));
        true
    }

    pub fn primary_label(&self) -> Option<&'static str> {
        match &self.step {
            ShareStep::Scope => Some("Continue"),
            ShareStep::Access => Some("Run checks"),
            ShareStep::Checked(r) if r.blocks() => None,
            ShareStep::Checked(_) => Some("Share"),
            _ => None,
        }
    }

    pub fn apply_preflight(&mut self, report: PreflightReport) {
        if self.step == ShareStep::Checking {
            self.step = ShareStep::Checked(report);
        }
    }

    /// Daemon confirmed creation, or later status updates of the share.
    pub fn apply_created(&mut self, created: CreatedShare) {
        if matches!(self.step, ShareStep::Creating | ShareStep::Created(_)) {
            let link = (created.status == ShareStatus::Ready)
                .then_some(created.link)
                .flatten();
            self.step = ShareStep::Created(CreatedShare { link, ..created });
        }
    }

    pub fn apply_error(&mut self, message: String) {
        if matches!(self.step, ShareStep::Checking | ShareStep::Creating) {
            self.step = ShareStep::Failed(message);
        }
    }
}
