use trama_domain::ids::ThreadId;
use trama_domain::secrets::SecretRule;
use trama_domain::share::{
    Capability, CodeAccessMode, JoinState, ShareRole, ShareScope, ShareStatus,
};

use super::join_flow::{JoinFlow, JoinRemedy, StepMark};
use super::model::*;
use super::share_flow::{ShareFlow, ShareStep};

fn flow() -> ShareFlow {
    ShareFlow::new(ThreadId::from("t1"), "Auth".into())
}

fn blocked() -> PreflightReport {
    PreflightReport {
        conflicts: vec![ConflictBlocker {
            repo: "api".into(),
            paths: vec!["a.rs".into()],
        }],
        secrets: vec![SecretBlocker {
            repo: "api".into(),
            path: ".env".into(),
            line: None,
            rule: SecretRule::DotEnvFile,
            hint: String::new(),
        }],
        scouts_outside: vec![ScoutBlocker {
            thread_title: "s".into(),
            parent_title: "p".into(),
        }],
        ..Default::default()
    }
}

#[test]
fn defaults_are_conservative() {
    let f = flow();
    assert_eq!(f.scope, ShareScope::ThreadOnly);
    assert_eq!(f.role, ShareRole::Reader);
    assert_eq!(f.code_access, CodeAccessMode::GitAuthorizationRequired);
    assert_eq!(f.capabilities(), [Capability::ReadHistory]);
}

#[test]
fn scope_and_capabilities_only_narrow() {
    let mut f = flow();
    f.set_future_descendants(true);
    assert!(
        !f.include_future_descendants,
        "thread_only has no descendants"
    );
    f.set_scope(ShareScope::Subtree);
    f.set_future_descendants(true);
    f.set_scope(ShareScope::ThreadOnly);
    assert!(!f.include_future_descendants);

    f.set_role(ShareRole::Owner);
    assert_eq!(f.role, ShareRole::Reader, "owner is never delegated");
    f.toggle(Capability::ManageShare);
    assert!(!f.has(Capability::ManageShare), "cannot add outside preset");
    f.set_role(ShareRole::Editor);
    assert!(f.has(Capability::ReadCode));
    f.toggle(Capability::PublishTurn);
    f.toggle(Capability::ReadHistory);
    let caps = f.capabilities();
    assert!(!caps.contains(&Capability::PublishTurn));
    assert!(caps.contains(&Capability::ReadHistory));
    f.set_role(ShareRole::Reviewer);
    assert!(
        f.capabilities().contains(&Capability::Review),
        "role change resets narrowing"
    );
}

#[test]
fn blockers_prevent_share_until_rechecked() {
    let mut f = flow();
    let mut out = Vec::new();
    assert!(!f.next(&mut out));
    assert_eq!(f.step, ShareStep::Access);
    assert!(f.next(&mut out));
    assert!(matches!(&out[0], WizardCommand::Preflight(r) if r.scope == ShareScope::ThreadOnly));
    f.apply_preflight(blocked());
    assert_eq!(f.primary_label(), None);
    assert!(!f.next(&mut out), "blocked report never creates");
    assert_eq!(out.len(), 1);

    assert!(f.run_checks(&mut out));
    f.apply_preflight(PreflightReport::default());
    assert_eq!(f.primary_label(), Some("Share"));
    assert!(f.next(&mut out));
    assert!(matches!(out.last(), Some(WizardCommand::CreateShare(_))));
    assert_eq!(f.step, ShareStep::Creating);
}

#[test]
fn back_discards_report_and_stale_replies_are_ignored() {
    let mut f = flow();
    let mut out = Vec::new();
    f.next(&mut out);
    f.next(&mut out);
    f.apply_preflight(PreflightReport::default());
    f.back();
    assert_eq!(f.step, ShareStep::Access);
    f.apply_preflight(PreflightReport::default());
    assert_eq!(
        f.step,
        ShareStep::Access,
        "report without a pending check is dropped"
    );
    f.apply_error("late".into());
    assert_eq!(f.step, ShareStep::Access);
}

#[test]
fn link_only_when_ready() {
    let mut f = flow();
    let mut out = Vec::new();
    f.next(&mut out);
    f.next(&mut out);
    f.apply_preflight(PreflightReport::default());
    f.next(&mut out);
    let created = |status| CreatedShare {
        share_id: "s".into(),
        status,
        link: Some("L".into()),
    };
    f.apply_created(created(ShareStatus::Preparing));
    assert!(matches!(&f.step, ShareStep::Created(c) if c.link.is_none()));
    f.apply_created(created(ShareStatus::Ready));
    assert!(matches!(&f.step, ShareStep::Created(c) if c.link.as_deref() == Some("L")));
}

fn progress(state: JoinState) -> JoinProgress {
    JoinProgress {
        state,
        detail: None,
    }
}

#[test]
fn join_validates_link_and_follows_progress() {
    let mut j = JoinFlow::default();
    let mut out = Vec::new();
    assert!(!j.start("  ", &mut out));
    assert!(j.error.is_some());
    assert!(!j.start("two words", &mut out));
    assert!(j.start(" trama://x/s#k ", &mut out));
    assert_eq!(
        out,
        [WizardCommand::Join {
            link: "trama://x/s#k".into()
        }]
    );
    assert!(!j.start("again", &mut out), "one join at a time");

    j.apply(progress(JoinState::FetchingGit));
    assert_eq!(j.state, Some(JoinState::Invited), "skipped step ignored");
    for s in [
        JoinState::Authorized,
        JoinState::FetchingGit,
        JoinState::FetchingContent,
    ] {
        j.apply(progress(s));
    }
    assert_eq!(j.mark(JoinState::FetchingGit), StepMark::Done);
    assert_eq!(j.mark(JoinState::FetchingContent), StepMark::Current);
    assert_eq!(j.mark(JoinState::Ready), StepMark::Pending);
    j.apply(progress(JoinState::Authorized));
    assert_eq!(
        j.state,
        Some(JoinState::FetchingContent),
        "stale report ignored"
    );
}

#[test]
fn join_errors_offer_the_right_remedy() {
    let mut j = JoinFlow::default();
    let mut out = Vec::new();
    j.start("L", &mut out);
    j.apply(progress(JoinState::Authorized));
    j.apply(progress(JoinState::RetryableNetworkError));
    assert_eq!(j.mark(JoinState::Authorized), StepMark::Failed);
    assert_eq!(j.remedy(), Some(JoinRemedy::Retry));
    j.retry(&mut out);
    assert_eq!(
        out.last(),
        Some(&WizardCommand::RetryJoin { link: "L".into() })
    );
    j.apply(progress(JoinState::Authorized));
    j.apply(progress(JoinState::NeedsGitAccess));
    assert_eq!(j.remedy(), Some(JoinRemedy::ConnectGitHub));
    j.retry(&mut out);
    assert_eq!(
        out.last(),
        Some(&WizardCommand::RetryJoin { link: "L".into() }),
        "no retry here"
    );
    j.connect_github(&mut out);
    assert_eq!(out.last(), Some(&WizardCommand::ConnectGitHub));
    j.apply(progress(JoinState::Authorized));
    j.apply(progress(JoinState::IntegrityFailed));
    assert_eq!(j.remedy(), Some(JoinRemedy::None));
    j.cancel(&mut out);
    assert_eq!(
        out.last(),
        Some(&WizardCommand::CancelJoin { link: "L".into() })
    );
    assert!(j.state.is_none());
}

#[test]
fn join_reaches_ready_and_cancel_after_ready_sends_nothing() {
    let mut j = JoinFlow::default();
    let mut out = Vec::new();
    j.start("L", &mut out);
    for s in &super::join_flow::JOIN_STEPS[1..] {
        j.apply(progress(*s));
    }
    assert!(j.is_ready());
    assert_eq!(j.mark(JoinState::Ready), StepMark::Done);
    j.cancel(&mut out);
    assert_eq!(out.len(), 1);
}
