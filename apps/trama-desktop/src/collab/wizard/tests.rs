use trama_domain::ids::{ThreadId, WorkspaceId};
use trama_domain::share::{Capability, SharedThreadKind};
use trama_protocol::{
    ClientMessage, SecretFinding, SecretRule, ShareBlocker, ShareJoinRequest, SharePreflightReport,
    SharePreflightThread, ShareRole, ShareScope, ShareSnapshot, ShareStatus,
};

use super::blockers::blocker_lines;
use super::join_flow::{JoinFlow, JoinStage, JoinStep, StageMark};
use super::share_flow::{ShareFlow, ShareStep, invite_capabilities};

fn tid(s: &str) -> ThreadId {
    ThreadId::from(s)
}

fn flow() -> ShareFlow {
    ShareFlow::new(tid("t1"), "Auth".into())
}

fn report(blockers: Vec<ShareBlocker>) -> SharePreflightReport {
    SharePreflightReport {
        thread_id: tid("t1"),
        scope: ShareScope::ThreadOnly,
        ok: blockers.is_empty(),
        threads: vec![SharePreflightThread {
            thread_id: tid("t1"),
            title: "Auth".into(),
            kind: SharedThreadKind::Main,
            parent_thread_id: None,
            is_scope_root: true,
        }],
        repositories: vec!["api".into()],
        scanned_files: 3,
        blockers,
    }
}

fn snapshot(id: &str) -> ShareSnapshot {
    ShareSnapshot {
        share_id: id.into(),
        scope: ShareScope::ThreadOnly,
        status: ShareStatus::Preparing,
        role: ShareRole::Owner,
        server_url: None,
        root_thread_id: None,
        snapshot_version: 1,
        key_epoch: Some(0),
    }
}

fn to_checks(f: &mut ShareFlow) {
    assert!(f.next().is_none());
    assert!(matches!(
        f.next(),
        Some(ClientMessage::SharePreflight { .. })
    ));
}

#[test]
fn defaults_and_invites_are_conservative() {
    let mut f = flow();
    assert_eq!(f.scope, ShareScope::ThreadOnly);
    assert!(f.add_invites("bob, @carol bob", Some("alice")).is_ok());
    assert_eq!(f.invites.len(), 2);
    assert!(
        f.invites
            .iter()
            .all(|i| i.role == ShareRole::Reader && !i.read_code)
    );
    assert_eq!(
        invite_capabilities(&f.invites[0]),
        [Capability::ReadHistory]
    );
    assert!(f.add_invites("Alice", Some("alice")).is_err());
    assert!(f.add_invites("bad name!", None).is_err());
    f.set_role("bob", ShareRole::Owner);
    assert_eq!(
        f.invites[0].role,
        ShareRole::Reader,
        "owner is never delegated"
    );
    f.set_role("bob", ShareRole::Writer);
    f.toggle_read_code("bob");
    let caps = invite_capabilities(&f.invites[0]);
    assert!(caps.contains(&Capability::ReadCode) && caps.contains(&Capability::PublishTurn));
    f.remove_invite("carol");
    assert_eq!(f.invites.len(), 1);
}

#[test]
fn blockers_prevent_share_and_recheck_unlocks() {
    let mut f = flow();
    to_checks(&mut f);
    f.add_invites("eve", None).unwrap();
    assert!(f.invites.is_empty(), "choices are frozen while checking");
    f.apply_preflight(report(vec![ShareBlocker::NotConnected]));
    assert_eq!(f.primary_label(), None);
    assert!(f.next().is_none(), "blocked report never creates");
    assert!(f.run_checks().is_some());
    f.apply_preflight(report(vec![]));
    assert_eq!(f.primary_label(), Some("Share"));
    match f.next() {
        Some(ClientMessage::CreateShare {
            thread_id, scope, ..
        }) => {
            assert_eq!((thread_id, scope), (tid("t1"), ShareScope::ThreadOnly))
        }
        other => panic!("{other:?}"),
    }
    // A create the daemon found blocked comes back as a report.
    f.apply_preflight(report(vec![ShareBlocker::NotConnected]));
    assert!(matches!(f.step, ShareStep::Checked(_)));
}

#[test]
fn stale_or_foreign_reports_are_ignored() {
    let mut f = flow();
    to_checks(&mut f);
    let mut other = report(vec![]);
    other.scope = ShareScope::Subtree;
    f.apply_preflight(other);
    assert_eq!(f.step, ShareStep::Checking);
    f.apply_preflight(report(vec![]));
    f.back();
    assert_eq!(f.step, ShareStep::People);
    f.apply_preflight(report(vec![]));
    f.apply_error("late".into());
    assert_eq!(f.step, ShareStep::People);
}

#[test]
fn created_share_loads_and_admits_join_requests() {
    let mut f = flow();
    to_checks(&mut f);
    f.apply_preflight(report(vec![]));
    f.next();
    assert!(matches!(
        f.apply_created(snapshot("s1")),
        Some(ClientMessage::ListShareJoinRequests { share_id }) if share_id == "s1"
    ));
    let req = ShareJoinRequest {
        username: "bob".into(),
        device_id: "d1".into(),
    };
    f.apply_requests("other", vec![req.clone()]);
    assert!(f.requests.is_empty());
    f.apply_requests("s1", vec![req]);
    assert!(f.approve("nope").is_none());
    assert!(matches!(
        f.approve("d1"),
        Some(ClientMessage::ApproveShareJoin { .. })
    ));
    assert!(f.approve("d1").is_none(), "one approval at a time");
    f.apply_approved("s1", "d1", 1);
    assert!(f.requests.is_empty() && f.approving.is_none());
    assert!(matches!(&f.step, ShareStep::Shared(s) if s.key_epoch == Some(1)));
}

#[test]
fn blocker_lines_name_threads_and_never_need_the_secret() {
    let r = report(vec![]);
    let lines = blocker_lines(
        &r,
        &ShareBlocker::Secrets {
            findings: vec![SecretFinding {
                path: "src/.env".into(),
                line: Some(3),
                rule: SecretRule::GithubToken,
                hint: "ghp_…(40 chars)".into(),
            }],
        },
    );
    assert_eq!(lines[0].0, "GitHub token");
    assert_eq!(lines[0].1.as_deref(), Some("src/.env:3 · ghp_…(40 chars)"));
    let scout = blocker_lines(
        &r,
        &ShareBlocker::ScoutOutsideShare {
            scout: tid("t1"),
            owner: tid("zz"),
        },
    );
    assert!(scout[0].0.contains("\u{201c}Auth\u{201d}"));
    assert!(scout[0].1.as_deref().unwrap().contains("outside the share"));
    let conflicts = blocker_lines(
        &r,
        &ShareBlocker::GitConflicts {
            thread_id: tid("t1"),
            repository: "api".into(),
            paths: vec!["a.rs".into()],
        },
    );
    assert!(conflicts[0].0.contains("api") && conflicts[0].1.as_deref().unwrap().contains("a.rs"));
}

#[test]
fn join_happy_path_and_waiting_for_approval() {
    let mut j = JoinFlow::new(WorkspaceId::from("w"));
    assert!(j.start("  ").is_none() && j.error.is_some());
    assert!(j.start("bad id").is_none());
    assert!(
        matches!(j.start(" share_1 "), Some(ClientMessage::RequestShareJoin { share_id }) if share_id == "share_1")
    );
    assert_eq!(j.mark(JoinStage::Request), StageMark::Current);
    j.apply_requested("other", "d".into());
    assert_eq!(j.step, JoinStep::Requesting);
    j.apply_requested("share_1", "dev9".into());
    assert_eq!(j.mark(JoinStage::Request), StageMark::Done);
    assert_eq!(j.mark(JoinStage::Approval), StageMark::Current);

    assert!(matches!(
        j.complete(),
        Some(ClientMessage::CompleteShareJoin { .. })
    ));
    j.apply_error("not approved yet".into());
    assert!(matches!(j.step, JoinStep::AwaitingApproval { .. }));
    assert_eq!(j.mark(JoinStage::Approval), StageMark::Failed);

    j.complete();
    assert_eq!(j.mark(JoinStage::Download), StageMark::Current);
    j.apply_joined(snapshot("share_1"));
    assert!(JOIN_ALL_DONE.iter().all(|s| j.mark(*s) == StageMark::Done));
}

const JOIN_ALL_DONE: [JoinStage; 4] = super::join_flow::JOIN_STAGES;

#[test]
fn join_request_failure_can_retry_or_reset() {
    let mut j = JoinFlow::new(WorkspaceId::from("w"));
    j.start("s");
    j.apply_error("offline".into());
    assert_eq!(j.mark(JoinStage::Request), StageMark::Failed);
    assert!(j.complete().is_none());
    assert!(matches!(
        j.retry_request(),
        Some(ClientMessage::RequestShareJoin { .. })
    ));
    j.reset();
    assert_eq!(j.step, JoinStep::Enter);
}

#[test]
fn router_delivers_replies_and_errors_to_the_pending_wizard() {
    use super::route::{WizardOp, route};
    use trama_protocol::DaemonMessage;
    let mut f = flow();
    let mut j = JoinFlow::new(WorkspaceId::from("w"));
    let mut pending = Some(WizardOp::Share);
    to_checks(&mut f);
    let r = route(
        &DaemonMessage::SharePreflightReport(report(vec![])),
        &mut pending,
        Some(&mut f),
        Some(&mut j),
    );
    assert!(r.handled && pending.is_none());
    f.next();
    pending = Some(WizardOp::Share);
    let r = route(
        &DaemonMessage::ShareCreated(snapshot("s1")),
        &mut pending,
        Some(&mut f),
        None,
    );
    assert!(matches!(
        r.follow_up,
        Some(ClientMessage::ListShareJoinRequests { .. })
    ));
    assert_eq!(pending, Some(WizardOp::Share));

    j.start("s9");
    pending = Some(WizardOp::Join);
    let err = DaemonMessage::Error {
        code: "unsupported".into(),
        message: "nope".into(),
    };
    assert!(route(&err, &mut pending, Some(&mut f), Some(&mut j)).handled);
    assert!(matches!(j.step, JoinStep::RequestFailed(_)));
    assert!(
        !route(&err, &mut pending, Some(&mut f), Some(&mut j)).handled,
        "unowned errors pass through"
    );
}
