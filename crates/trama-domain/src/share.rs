//! Sharing contracts (plan `docs/plan/01` and `02`): scope, capabilities,
//! roles, policy, perimeter resolution and join lifecycle. Pure value types:
//! no I/O, no networking, no raw URLs are trusted here.
//!
//! Nothing in this module grants Git access, starts an agent or contacts a
//! server. It only decides *what* a share may contain and *who* may do what.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

macro_rules! gid_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn generate() -> Self {
                Self(ulid::Ulid::new().to_string())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

gid_type!(
    /// Persistent identity of a collaboration server (not its display URL).
    ServerId
);
gid_type!(
    /// Globally unique share id, bound to a server and a policy.
    ShareId
);
gid_type!(
    /// Stable thread identity inside a share, distinct from local `ThreadId`s.
    ThreadGid
);
gid_type!(
    /// Repository identity inside a share. A remote URL is only a hint.
    RepoGid
);
gid_type!(
    /// Persistent cryptographic identity of one installation/device.
    DeviceId
);

/// What a share includes, starting from its root thread(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareScope {
    /// Only the chosen thread: no parent, siblings or children.
    ThreadOnly,
    /// The chosen thread and all its descendants, at any depth.
    Subtree,
    /// The explicitly selected roots of a workspace and their descendants.
    Workspace,
}

impl ShareScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThreadOnly => "thread_only",
            Self::Subtree => "subtree",
            Self::Workspace => "workspace",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "thread_only" => Some(Self::ThreadOnly),
            "subtree" => Some(Self::Subtree),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }

    /// Whether this scope descends into children of its roots.
    pub fn includes_descendants(self) -> bool {
        !matches!(self, Self::ThreadOnly)
    }
}

/// Separate capabilities. Reading history never implies reading code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadHistory,
    ReadCode,
    PublishTurn,
    Comment,
    Review,
    Export,
    ManageShare,
}

/// A role is only a preset of capabilities; checks always use capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareRole {
    Owner,
    Editor,
    Reviewer,
    Reader,
}

impl ShareRole {
    /// Default capabilities of the role. `code_granted` reflects whether a
    /// `read_code` grant was issued (see [`CodeAccessMode`]); it never comes
    /// from a client-side "clone succeeded" boolean.
    pub fn default_capabilities(self, code_granted: bool) -> CapabilitySet {
        use Capability::*;
        let mut caps: BTreeSet<Capability> = match self {
            Self::Owner => [
                ReadHistory,
                ReadCode,
                PublishTurn,
                Comment,
                Review,
                Export,
                ManageShare,
            ]
            .into(),
            Self::Editor => [ReadHistory, PublishTurn, Comment, Review].into(),
            Self::Reviewer => [ReadHistory, Comment, Review].into(),
            Self::Reader => [ReadHistory].into(),
        };
        if code_granted && matches!(self, Self::Editor | Self::Reviewer) {
            caps.insert(ReadCode);
        }
        CapabilitySet(caps)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilitySet(pub BTreeSet<Capability>);

impl CapabilitySet {
    pub fn has(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    /// `Continue coding` needs code access *and* a usable local checkout;
    /// it is never offered from history access alone.
    pub fn can_continue_coding(&self, checkout_ready: bool) -> bool {
        checkout_ready && self.has(Capability::ReadCode) && self.has(Capability::PublishTurn)
    }
}

/// How `read_code` grants relate to Git provider permissions (plan 01 §5bis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeAccessMode {
    /// A verifiable provider/enterprise attestation is required per repo
    /// before any code key is delivered. Recommended for enterprise.
    GitAuthorizationRequired,
    /// Owner explicitly grants code regardless of provider permissions.
    ShareGrantsCodeAccess,
}

/// Three-way setting for exports that need explicit confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportPermission {
    Allowed,
    ConfirmationRequired,
    Denied,
}

/// Enterprise/user sharing policy. Enforced by the daemon independently of
/// the UI; every check fails closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePolicy {
    /// Empty means "no server allowed" — not "any server".
    pub allowed_collab_server_ids: BTreeSet<ServerId>,
    pub allow_trama_cloud: bool,
    pub allow_public_discovery: bool,
    pub allow_public_relay: bool,
    pub allow_direct_p2p: bool,
    /// Lower-case hostnames recipients may clone from. Empty means none.
    pub allowed_git_hosts: BTreeSet<String>,
    pub require_e2ee: bool,
    pub code_access: CodeAccessMode,
    pub allow_untracked_export: ExportPermission,
    pub max_share_bytes: Option<u64>,
    pub include_future_descendants: bool,
    pub auto_include_new_repositories: bool,
}

impl Default for SharePolicy {
    /// The most restrictive policy: nothing is allowed until configured.
    fn default() -> Self {
        Self {
            allowed_collab_server_ids: BTreeSet::new(),
            allow_trama_cloud: false,
            allow_public_discovery: false,
            allow_public_relay: false,
            allow_direct_p2p: false,
            allowed_git_hosts: BTreeSet::new(),
            require_e2ee: true,
            code_access: CodeAccessMode::GitAuthorizationRequired,
            allow_untracked_export: ExportPermission::ConfirmationRequired,
            max_share_bytes: None,
            include_future_descendants: false,
            auto_include_new_repositories: false,
        }
    }
}

/// Which transport a share would use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportTarget {
    TramaCloud { server_id: ServerId },
    SelfHosted { server_id: ServerId },
    DirectP2p,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PolicyViolation {
    ServerNotAllowed { server_id: ServerId },
    TramaCloudDisabled,
    DirectP2pDisabled,
    GitHostNotAllowed { host: String },
    E2eeRequired,
    UntrackedExportDenied,
    ShareTooLarge { bytes: u64, max: u64 },
}

impl SharePolicy {
    /// Check a transport before any connection, discovery or DNS lookup.
    /// There is no fallback: a refusal is final.
    pub fn check_transport(&self, target: &TransportTarget) -> Result<(), PolicyViolation> {
        match target {
            TransportTarget::TramaCloud { server_id } => {
                if !self.allow_trama_cloud {
                    return Err(PolicyViolation::TramaCloudDisabled);
                }
                self.check_server(server_id)
            }
            TransportTarget::SelfHosted { server_id } => self.check_server(server_id),
            TransportTarget::DirectP2p => {
                if self.allow_direct_p2p {
                    Ok(())
                } else {
                    Err(PolicyViolation::DirectP2pDisabled)
                }
            }
        }
    }

    fn check_server(&self, server_id: &ServerId) -> Result<(), PolicyViolation> {
        if self.allowed_collab_server_ids.contains(server_id) {
            Ok(())
        } else {
            Err(PolicyViolation::ServerNotAllowed {
                server_id: server_id.clone(),
            })
        }
    }

    /// Check a clone host (already parsed and normalized by the caller).
    pub fn check_git_host(&self, host: &str) -> Result<(), PolicyViolation> {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if !host.is_empty() && self.allowed_git_hosts.contains(&host) {
            Ok(())
        } else {
            Err(PolicyViolation::GitHostNotAllowed { host })
        }
    }

    /// Check whether a share may be created with the given encryption and size.
    pub fn check_export(
        &self,
        e2ee: bool,
        total_bytes: u64,
        has_untracked: bool,
        untracked_confirmed: bool,
    ) -> Result<(), PolicyViolation> {
        if self.require_e2ee && !e2ee {
            return Err(PolicyViolation::E2eeRequired);
        }
        if let Some(max) = self.max_share_bytes
            && total_bytes > max
        {
            return Err(PolicyViolation::ShareTooLarge {
                bytes: total_bytes,
                max,
            });
        }
        if has_untracked {
            match self.allow_untracked_export {
                ExportPermission::Allowed => {}
                ExportPermission::ConfirmationRequired if untracked_confirmed => {}
                _ => return Err(PolicyViolation::UntrackedExportDenied),
            }
        }
        Ok(())
    }
}

/// Thread kinds as they appear in a share manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedThreadKind {
    Main,
    Child,
    Fork,
    Review,
    Land,
    Worker,
    Scout,
}

/// Minimal view of a thread family used for perimeter resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyNode {
    pub gid: ThreadGid,
    pub parent: Option<ThreadGid>,
    pub kind: SharedThreadKind,
    /// For scouts sharing another thread's checkout: the owning thread.
    pub checkout_alias_of: Option<ThreadGid>,
}

/// One thread inside a resolved share perimeter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerimeterThread {
    pub gid: ThreadGid,
    /// `None` at the share boundary: an unshared parent is never referenced.
    pub parent: Option<ThreadGid>,
    pub kind: SharedThreadKind,
    pub is_scope_root: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Perimeter {
    pub scope: ShareScope,
    /// Depth-first, roots first, children in input order.
    pub threads: Vec<PerimeterThread>,
}

impl Perimeter {
    pub fn contains(&self, gid: &ThreadGid) -> bool {
        self.threads.iter().any(|thread| &thread.gid == gid)
    }
}

/// How to treat a scout whose aliased checkout owner falls outside scope
/// (plan 01 §3). There is no default: the caller must choose before export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutOfScopeAlias {
    MaterializeIndependent,
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum PerimeterError {
    NoRoots,
    UnknownRoot {
        gid: ThreadGid,
    },
    /// `thread_only` and `subtree` take exactly one root.
    TooManyRoots,
    /// A scout aliases a checkout owned by a thread outside the perimeter.
    AliasOutsideScope {
        scout: ThreadGid,
        owner: ThreadGid,
    },
    Cycle {
        gid: ThreadGid,
    },
}

/// Resolve which threads a share contains. Parents of roots are cut off
/// (`parent = None`), so siblings and ancestors never leak.
pub fn resolve_perimeter(
    family: &[FamilyNode],
    scope: ShareScope,
    roots: &[ThreadGid],
    excluded: &BTreeSet<ThreadGid>,
    out_of_scope_alias: Option<OutOfScopeAlias>,
) -> Result<Perimeter, PerimeterError> {
    if roots.is_empty() {
        return Err(PerimeterError::NoRoots);
    }
    if scope != ShareScope::Workspace && roots.len() > 1 {
        return Err(PerimeterError::TooManyRoots);
    }
    let by_gid: BTreeMap<&ThreadGid, &FamilyNode> =
        family.iter().map(|node| (&node.gid, node)).collect();
    let mut children: BTreeMap<&ThreadGid, Vec<&FamilyNode>> = BTreeMap::new();
    for node in family {
        if let Some(parent) = &node.parent {
            children.entry(parent).or_default().push(node);
        }
    }

    let mut threads = Vec::new();
    let mut seen = BTreeSet::new();
    for root in roots {
        let node = by_gid
            .get(root)
            .ok_or_else(|| PerimeterError::UnknownRoot { gid: root.clone() })?;
        if seen.contains(&node.gid) || excluded.contains(&node.gid) {
            continue;
        }
        let mut stack = vec![(*node, true)];
        while let Some((node, is_root)) = stack.pop() {
            if !seen.insert(node.gid.clone()) {
                return Err(PerimeterError::Cycle {
                    gid: node.gid.clone(),
                });
            }
            threads.push(PerimeterThread {
                gid: node.gid.clone(),
                parent: if is_root { None } else { node.parent.clone() },
                kind: node.kind,
                is_scope_root: is_root,
            });
            if scope.includes_descendants()
                && let Some(kids) = children.get(&node.gid)
            {
                for kid in kids.iter().rev() {
                    if !excluded.contains(&kid.gid) {
                        stack.push((kid, false));
                    }
                }
            }
        }
    }

    let perimeter = Perimeter { scope, threads };
    if out_of_scope_alias != Some(OutOfScopeAlias::MaterializeIndependent) {
        for node in family {
            if !perimeter.contains(&node.gid) {
                continue;
            }
            if let Some(owner) = &node.checkout_alias_of
                && !perimeter.contains(owner)
            {
                return Err(PerimeterError::AliasOutsideScope {
                    scout: node.gid.clone(),
                    owner: owner.clone(),
                });
            }
        }
    }
    Ok(perimeter)
}

/// Lifecycle of a share on the publishing side. Links promise data only
/// once `Ready` (plan 02 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareStatus {
    Preparing,
    Incomplete,
    Ready,
    Paused,
    Closed,
}

/// Join lifecycle on the receiving side (plan 02 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinState {
    Invited,
    Authorized,
    FetchingGit,
    FetchingContent,
    Verifying,
    Materializing,
    Ready,
    NeedsGitAccess,
    MissingObject,
    ForbiddenByPolicy,
    IntegrityFailed,
    WaitingForHost,
    PartialNoCode,
    Conflict,
    RetryableNetworkError,
}

impl JoinState {
    /// Only `Ready` enables agent, editor and Pull Changes.
    pub fn is_operational(self) -> bool {
        self == Self::Ready
    }

    pub fn is_error(self) -> bool {
        matches!(
            self,
            Self::NeedsGitAccess
                | Self::MissingObject
                | Self::ForbiddenByPolicy
                | Self::IntegrityFailed
                | Self::WaitingForHost
                | Self::PartialNoCode
                | Self::Conflict
                | Self::RetryableNetworkError
        )
    }

    /// Next state on the happy path, if any.
    fn next(self) -> Option<Self> {
        match self {
            Self::Invited => Some(Self::Authorized),
            Self::Authorized => Some(Self::FetchingGit),
            Self::FetchingGit => Some(Self::FetchingContent),
            Self::FetchingContent => Some(Self::Verifying),
            Self::Verifying => Some(Self::Materializing),
            Self::Materializing => Some(Self::Ready),
            _ => None,
        }
    }

    /// Whether `self -> to` is a legal transition. Steps cannot be skipped;
    /// any non-terminal step may fail; retryable errors resume from the
    /// persisted cursor (`Authorized` re-derives what is still missing).
    pub fn can_transition_to(self, to: Self) -> bool {
        if self.next() == Some(to) {
            return true;
        }
        if to.is_error() {
            // A Ready replica can only hit live-update problems.
            return self == to
                || (!self.is_error()
                    && (self != Self::Ready
                        || matches!(to, Self::Conflict | Self::RetryableNetworkError)));
        }
        match self {
            Self::RetryableNetworkError | Self::WaitingForHost | Self::MissingObject => {
                to == Self::Authorized
            }
            // History-only participants can still be authorized later.
            Self::PartialNoCode | Self::NeedsGitAccess => to == Self::Authorized,
            // New content after Ready restarts verification from fetch.
            Self::Ready => to == Self::FetchingContent,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(gid: &str, parent: Option<&str>, kind: SharedThreadKind) -> FamilyNode {
        FamilyNode {
            gid: gid.into(),
            parent: parent.map(ThreadGid::from),
            kind,
            checkout_alias_of: None,
        }
    }

    /// main -> {auth_review, dashboard -> {dash_child}}
    fn family() -> Vec<FamilyNode> {
        vec![
            node("main", None, SharedThreadKind::Main),
            node("auth_review", Some("main"), SharedThreadKind::Review),
            node("dashboard", Some("main"), SharedThreadKind::Child),
            node("dash_child", Some("dashboard"), SharedThreadKind::Child),
        ]
    }

    fn gids(perimeter: &Perimeter) -> Vec<&str> {
        perimeter.threads.iter().map(|t| t.gid.0.as_str()).collect()
    }

    #[test]
    fn thread_only_child_cuts_parent_and_siblings() {
        let p = resolve_perimeter(
            &family(),
            ShareScope::ThreadOnly,
            &["auth_review".into()],
            &BTreeSet::new(),
            None,
        )
        .unwrap();
        assert_eq!(gids(&p), ["auth_review"]);
        assert_eq!(p.threads[0].parent, None);
        assert!(p.threads[0].is_scope_root);
    }

    #[test]
    fn subtree_includes_all_descendants_only() {
        let p = resolve_perimeter(
            &family(),
            ShareScope::Subtree,
            &["dashboard".into()],
            &BTreeSet::new(),
            None,
        )
        .unwrap();
        assert_eq!(gids(&p), ["dashboard", "dash_child"]);
        assert_eq!(p.threads[0].parent, None);
        assert_eq!(p.threads[1].parent, Some("dashboard".into()));
        let full = resolve_perimeter(
            &family(),
            ShareScope::Subtree,
            &["main".into()],
            &BTreeSet::new(),
            None,
        )
        .unwrap();
        assert_eq!(
            gids(&full),
            ["main", "auth_review", "dashboard", "dash_child"]
        );
    }

    #[test]
    fn exclusions_prune_whole_branch() {
        let excluded = BTreeSet::from(["dashboard".into()]);
        let p = resolve_perimeter(
            &family(),
            ShareScope::Subtree,
            &["main".into()],
            &excluded,
            None,
        )
        .unwrap();
        assert_eq!(gids(&p), ["main", "auth_review"]);
    }

    #[test]
    fn root_count_rules() {
        let empty = resolve_perimeter(&family(), ShareScope::Subtree, &[], &BTreeSet::new(), None);
        assert_eq!(empty, Err(PerimeterError::NoRoots));
        let two = ["auth_review".into(), "dashboard".into()];
        assert_eq!(
            resolve_perimeter(&family(), ShareScope::Subtree, &two, &BTreeSet::new(), None),
            Err(PerimeterError::TooManyRoots)
        );
        let ws = resolve_perimeter(
            &family(),
            ShareScope::Workspace,
            &two,
            &BTreeSet::new(),
            None,
        )
        .unwrap();
        assert_eq!(gids(&ws), ["auth_review", "dashboard", "dash_child"]);
        assert!(matches!(
            resolve_perimeter(
                &family(),
                ShareScope::ThreadOnly,
                &["nope".into()],
                &BTreeSet::new(),
                None
            ),
            Err(PerimeterError::UnknownRoot { .. })
        ));
    }

    #[test]
    fn scout_alias_outside_scope_requires_explicit_choice() {
        let mut fam = family();
        let mut scout = node("scout", Some("dashboard"), SharedThreadKind::Scout);
        scout.checkout_alias_of = Some("dashboard".into());
        fam.push(scout);
        let roots = ["scout".into()];
        assert_eq!(
            resolve_perimeter(&fam, ShareScope::ThreadOnly, &roots, &BTreeSet::new(), None),
            Err(PerimeterError::AliasOutsideScope {
                scout: "scout".into(),
                owner: "dashboard".into()
            })
        );
        assert!(
            resolve_perimeter(
                &fam,
                ShareScope::ThreadOnly,
                &roots,
                &BTreeSet::new(),
                Some(OutOfScopeAlias::MaterializeIndependent)
            )
            .is_ok()
        );
        // Owner inside the perimeter: fine without a choice.
        assert!(
            resolve_perimeter(
                &fam,
                ShareScope::Subtree,
                &["dashboard".into()],
                &BTreeSet::new(),
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn cycles_are_rejected() {
        let fam = vec![
            node("a", Some("b"), SharedThreadKind::Child),
            node("b", Some("a"), SharedThreadKind::Child),
        ];
        assert!(matches!(
            resolve_perimeter(
                &fam,
                ShareScope::Subtree,
                &["a".into()],
                &BTreeSet::new(),
                None
            ),
            Err(PerimeterError::Cycle { .. })
        ));
    }

    #[test]
    fn capabilities_are_separate() {
        let reader = ShareRole::Reader.default_capabilities(true);
        assert!(reader.has(Capability::ReadHistory));
        assert!(!reader.has(Capability::ReadCode));
        let editor = ShareRole::Editor.default_capabilities(false);
        assert!(!editor.has(Capability::ReadCode));
        assert!(!editor.can_continue_coding(true));
        let editor = ShareRole::Editor.default_capabilities(true);
        assert!(editor.can_continue_coding(true));
        assert!(!editor.can_continue_coding(false));
        assert!(!editor.has(Capability::ManageShare));
        let reviewer = ShareRole::Reviewer.default_capabilities(true);
        assert!(reviewer.has(Capability::Review) && !reviewer.has(Capability::PublishTurn));
        assert!(
            ShareRole::Owner
                .default_capabilities(false)
                .has(Capability::ManageShare)
        );
    }

    #[test]
    fn default_policy_fails_closed() {
        let policy = SharePolicy::default();
        let server = ServerId::from("company-prod");
        assert_eq!(
            policy.check_transport(&TransportTarget::SelfHosted {
                server_id: server.clone()
            }),
            Err(PolicyViolation::ServerNotAllowed {
                server_id: server.clone()
            })
        );
        assert_eq!(
            policy.check_transport(&TransportTarget::TramaCloud { server_id: server }),
            Err(PolicyViolation::TramaCloudDisabled)
        );
        assert_eq!(
            policy.check_transport(&TransportTarget::DirectP2p),
            Err(PolicyViolation::DirectP2pDisabled)
        );
        assert!(policy.check_git_host("github.com").is_err());
        assert_eq!(
            policy.check_export(false, 1, false, false),
            Err(PolicyViolation::E2eeRequired)
        );
        assert_eq!(
            policy.check_export(true, 1, true, false),
            Err(PolicyViolation::UntrackedExportDenied)
        );
        assert!(policy.check_export(true, 1, true, true).is_ok());
    }

    #[test]
    fn enterprise_policy_allows_only_listed() {
        let policy = SharePolicy {
            allowed_collab_server_ids: BTreeSet::from(["company-prod".into()]),
            allowed_git_hosts: BTreeSet::from(["git.company.example".into()]),
            max_share_bytes: Some(100),
            ..SharePolicy::default()
        };
        assert!(
            policy
                .check_transport(&TransportTarget::SelfHosted {
                    server_id: "company-prod".into()
                })
                .is_ok()
        );
        // Even an allowed server id is refused when it is Trama Cloud.
        assert!(
            policy
                .check_transport(&TransportTarget::TramaCloud {
                    server_id: "company-prod".into()
                })
                .is_err()
        );
        assert!(policy.check_git_host("Git.Company.Example.").is_ok());
        assert!(policy.check_git_host("evil.example").is_err());
        assert_eq!(
            policy.check_export(true, 101, false, false),
            Err(PolicyViolation::ShareTooLarge {
                bytes: 101,
                max: 100
            })
        );
    }

    #[test]
    fn join_state_machine() {
        use JoinState::*;
        let path = [
            Invited,
            Authorized,
            FetchingGit,
            FetchingContent,
            Verifying,
            Materializing,
            Ready,
        ];
        for pair in path.windows(2) {
            assert!(pair[0].can_transition_to(pair[1]), "{pair:?}");
        }
        assert!(!Invited.can_transition_to(Ready));
        assert!(!FetchingGit.can_transition_to(Materializing));
        assert!(FetchingGit.can_transition_to(NeedsGitAccess));
        assert!(Verifying.can_transition_to(IntegrityFailed));
        assert!(!IntegrityFailed.can_transition_to(Ready));
        assert!(!IntegrityFailed.can_transition_to(Authorized));
        assert!(RetryableNetworkError.can_transition_to(Authorized));
        assert!(Ready.is_operational() && !Materializing.is_operational());
    }

    #[test]
    fn scope_string_roundtrip() {
        for scope in [
            ShareScope::ThreadOnly,
            ShareScope::Subtree,
            ShareScope::Workspace,
        ] {
            assert_eq!(ShareScope::parse(scope.as_str()), Some(scope));
            assert_eq!(
                serde_json::to_string(&scope).unwrap(),
                format!("\"{}\"", scope.as_str())
            );
        }
        assert_eq!(ShareScope::parse("all"), None);
    }
}
