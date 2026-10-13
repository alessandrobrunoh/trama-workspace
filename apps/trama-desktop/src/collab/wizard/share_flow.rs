//! Share wizard state machine: Scope → People → Checks → Creating → Shared
//! (then admitting devices that asked to join).

use trama_domain::ids::ThreadId;
use trama_domain::share::Capability;
use trama_protocol::{
    ClientMessage, ShareInvite, ShareJoinRequest, ShareMemberSnapshot, SharePreflightReport,
    ShareRole, ShareScope, ShareSnapshot, ShareStatus,
};

use crate::collab::valid_name;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareStep {
    Scope,
    People,
    /// Preflight requested, no report yet.
    Checking,
    /// Report received; `Share` is enabled only when `ok`.
    Checked(SharePreflightReport),
    Creating,
    Shared(ShareSnapshot),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct ShareFlow {
    pub thread_id: ThreadId,
    pub title: String,
    pub step: ShareStep,
    pub scope: ShareScope,
    pub invites: Vec<ShareInvite>,
    /// Devices waiting for admission (owner side, after creation).
    pub requests: Vec<ShareJoinRequest>,
    /// Device being approved, to disable its button.
    pub approving: Option<String>,
    /// Usernames with a grant, owner excluded. From `ShareMembers` once
    /// loaded; until then the invites and admitted devices seen locally.
    pub members: Vec<String>,
    /// Server roster (roles, devices), when `ListShareMembers` answered.
    pub roster: Vec<ShareMemberSnapshot>,
    /// The share ended for this device (closed, or access revoked).
    pub ended: Option<String>,
    /// Member whose removal awaits confirmation.
    pub confirm_remove: Option<String>,
    /// Removal in flight.
    pub removing: Option<String>,
    pub error: Option<String>,
}

/// What an invite with `role`/`read_code` may do (plan 01 §4 presets).
pub fn invite_capabilities(invite: &ShareInvite) -> Vec<Capability> {
    use Capability::*;
    let mut caps = match invite.role {
        ShareRole::Writer => vec![ReadHistory, PublishTurn, Comment, Review],
        _ => vec![ReadHistory],
    };
    if invite.read_code {
        caps.insert(1, ReadCode);
    }
    caps
}

impl ShareFlow {
    /// Conservative defaults: this thread only; invitees are readers
    /// without code until the owner opts in.
    pub fn new(thread_id: ThreadId, title: String) -> Self {
        Self {
            thread_id,
            title,
            step: ShareStep::Scope,
            scope: ShareScope::ThreadOnly,
            invites: Vec::new(),
            requests: Vec::new(),
            approving: None,
            members: Vec::new(),
            roster: Vec::new(),
            ended: None,
            confirm_remove: None,
            removing: None,
            error: None,
        }
    }

    fn editable(&self) -> bool {
        matches!(self.step, ShareStep::Scope | ShareStep::People)
    }

    pub fn set_scope(&mut self, scope: ShareScope) {
        if self.editable() {
            self.scope = scope;
        }
    }

    /// Adds usernames (space/comma separated) as reader invites.
    pub fn add_invites(&mut self, raw: &str, me: Option<&str>) -> Result<(), String> {
        if !self.editable() {
            return Ok(());
        }
        let names: Vec<&str> = raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_start_matches('@'))
            .collect();
        if let Some(bad) = names.iter().find(|n| !valid_name(n)) {
            return Err(format!("\u{201c}{bad}\u{201d} is not a valid username."));
        }
        if let Some(me) = me
            && names.iter().any(|n| n.eq_ignore_ascii_case(me))
        {
            return Err("You are the owner of this share.".into());
        }
        for name in names {
            if !self
                .invites
                .iter()
                .any(|i| i.username.eq_ignore_ascii_case(name))
            {
                self.invites.push(ShareInvite {
                    username: name.to_owned(),
                    role: ShareRole::Reader,
                    read_code: false,
                });
            }
        }
        Ok(())
    }

    fn invite_mut(&mut self, username: &str) -> Option<&mut ShareInvite> {
        if !self.editable() {
            return None;
        }
        self.invites.iter_mut().find(|i| i.username == username)
    }

    /// Owner is never delegated.
    pub fn set_role(&mut self, username: &str, role: ShareRole) {
        if role != ShareRole::Owner
            && let Some(i) = self.invite_mut(username)
        {
            i.role = role;
        }
    }

    pub fn toggle_read_code(&mut self, username: &str) {
        if let Some(i) = self.invite_mut(username) {
            i.read_code = !i.read_code;
        }
    }

    pub fn remove_invite(&mut self, username: &str) {
        if self.editable() {
            self.invites.retain(|i| i.username != username);
        }
    }

    pub fn can_go_back(&self) -> bool {
        matches!(
            self.step,
            ShareStep::People | ShareStep::Checked(_) | ShareStep::Failed(_)
        )
    }

    /// Back also discards the report: it described other choices.
    pub fn back(&mut self) {
        self.step = match self.step {
            ShareStep::People => ShareStep::Scope,
            ShareStep::Checked(_) | ShareStep::Failed(_) => ShareStep::People,
            _ => return,
        };
    }

    pub fn primary_label(&self) -> Option<&'static str> {
        match &self.step {
            ShareStep::Scope => Some("Continue"),
            ShareStep::People => Some("Run checks"),
            ShareStep::Checked(r) if r.ok && r.blockers.is_empty() => Some("Share"),
            _ => None,
        }
    }

    /// Primary button.
    pub fn next(&mut self) -> Option<ClientMessage> {
        match &self.step {
            ShareStep::Scope => {
                self.step = ShareStep::People;
                None
            }
            ShareStep::People => self.run_checks(),
            ShareStep::Checked(r) if r.ok && r.blockers.is_empty() => {
                self.step = ShareStep::Creating;
                Some(ClientMessage::CreateShare {
                    thread_id: self.thread_id.clone(),
                    scope: self.scope,
                    members: self.invites.clone(),
                })
            }
            _ => None,
        }
    }

    /// (Re)runs preflight, e.g. after fixing blockers.
    pub fn run_checks(&mut self) -> Option<ClientMessage> {
        if !matches!(
            self.step,
            ShareStep::People | ShareStep::Checked(_) | ShareStep::Failed(_)
        ) {
            return None;
        }
        self.step = ShareStep::Checking;
        Some(ClientMessage::SharePreflight {
            thread_id: self.thread_id.clone(),
            scope: self.scope,
        })
    }

    /// Also the reply to a `CreateShare` the daemon found blocked.
    pub fn apply_preflight(&mut self, report: SharePreflightReport) {
        let pending = matches!(self.step, ShareStep::Checking | ShareStep::Creating);
        if pending && report.thread_id == self.thread_id && report.scope == self.scope {
            self.step = ShareStep::Checked(report);
        }
    }

    /// Created: load the roster and the admission queue.
    pub fn apply_created(&mut self, share: ShareSnapshot) -> Vec<ClientMessage> {
        if self.step != ShareStep::Creating {
            return Vec::new();
        }
        self.members = self.invites.iter().map(|i| i.username.clone()).collect();
        self.step = ShareStep::Shared(share);
        self.refresh()
    }

    /// Reloads roster and join requests.
    pub fn refresh(&self) -> Vec<ClientMessage> {
        let Some(share_id) = self.share_id().map(str::to_owned) else {
            return Vec::new();
        };
        vec![
            ClientMessage::ListShareMembers {
                share_id: share_id.clone(),
            },
            ClientMessage::ListShareJoinRequests { share_id },
        ]
    }

    pub fn apply_members(&mut self, share_id: &str, members: Vec<ShareMemberSnapshot>) {
        if self.share_id() != Some(share_id) {
            return;
        }
        self.members = members
            .iter()
            .filter(|m| m.role != ShareRole::Owner)
            .map(|m| m.username.clone())
            .collect();
        self.roster = members;
    }

    pub fn member_info(&self, username: &str) -> Option<&ShareMemberSnapshot> {
        self.roster.iter().find(|m| m.username == username)
    }

    /// The share is over for this device: no further actions.
    pub fn end(&mut self, why: String) {
        if self.share_id().is_some() {
            self.ended = Some(why);
            self.confirm_remove = None;
            self.removing = None;
            self.approving = None;
        }
    }

    pub fn apply_status(&mut self, share: &ShareSnapshot) {
        if self.share_id() != Some(share.share_id.as_str()) {
            return;
        }
        if share.status == ShareStatus::Closed {
            self.end("This share was closed.".into());
        }
        if let ShareStep::Shared(s) = &mut self.step {
            s.status = share.status;
        }
    }

    pub fn share_id(&self) -> Option<&str> {
        match &self.step {
            ShareStep::Shared(s) => Some(&s.share_id),
            _ => None,
        }
    }

    pub fn apply_requests(&mut self, share_id: &str, requests: Vec<ShareJoinRequest>) {
        if self.share_id() == Some(share_id) {
            self.requests = requests;
        }
    }

    pub fn approve(&mut self, device_id: &str) -> Option<ClientMessage> {
        let share_id = self.share_id()?.to_owned();
        if self.ended.is_some()
            || self.approving.is_some()
            || !self.requests.iter().any(|r| r.device_id == device_id)
        {
            return None;
        }
        self.approving = Some(device_id.to_owned());
        self.error = None;
        Some(ClientMessage::ApproveShareJoin {
            share_id,
            device_id: device_id.to_owned(),
        })
    }

    /// Returns a roster reload: the server is the source of truth.
    pub fn apply_approved(
        &mut self,
        share_id: &str,
        device_id: &str,
        key_epoch: u64,
    ) -> Option<ClientMessage> {
        if self.share_id() != Some(share_id) {
            return None;
        }
        if let Some(i) = self.requests.iter().position(|r| r.device_id == device_id) {
            let user = self.requests.remove(i).username;
            if !self.members.iter().any(|m| m.eq_ignore_ascii_case(&user)) {
                self.members.push(user);
            }
        }
        self.approving = None;
        if let ShareStep::Shared(s) = &mut self.step {
            s.key_epoch = Some(key_epoch);
        }
        Some(ClientMessage::ListShareMembers {
            share_id: share_id.to_owned(),
        })
    }

    /// First click on "Remove": ask for confirmation.
    pub fn ask_remove(&mut self, username: &str) {
        if self.share_id().is_some()
            && self.ended.is_none()
            && self.removing.is_none()
            && self.members.iter().any(|m| m == username)
        {
            self.confirm_remove = Some(username.to_owned());
        }
    }

    pub fn cancel_remove(&mut self) {
        self.confirm_remove = None;
    }

    /// Confirmed: revoke the grant and evict every device of the member.
    pub fn confirm_remove(&mut self) -> Option<ClientMessage> {
        let share_id = self.share_id()?.to_owned();
        let username = self.confirm_remove.take()?;
        self.removing = Some(username.clone());
        self.error = None;
        Some(ClientMessage::RemoveShareMember { share_id, username })
    }

    pub fn apply_removed(&mut self, share_id: &str, username: &str, key_epoch: u64) {
        if self.share_id() != Some(share_id) {
            return;
        }
        self.members.retain(|m| !m.eq_ignore_ascii_case(username));
        self.roster
            .retain(|m| !m.username.eq_ignore_ascii_case(username));
        self.requests
            .retain(|r| !r.username.eq_ignore_ascii_case(username));
        if self.removing.as_deref() == Some(username) {
            self.removing = None;
        }
        if let ShareStep::Shared(s) = &mut self.step {
            s.key_epoch = Some(key_epoch);
        }
    }

    pub fn apply_error(&mut self, message: String) {
        match self.step {
            ShareStep::Checking | ShareStep::Creating => self.step = ShareStep::Failed(message),
            ShareStep::Shared(_) => {
                self.approving = None;
                self.removing = None;
                self.error = Some(message);
            }
            _ => {}
        }
    }
}
