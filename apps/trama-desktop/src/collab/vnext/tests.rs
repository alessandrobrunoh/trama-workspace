use super::*;
use trama_protocol::NegotiatedProfile;

fn info(url: &str, protocol: u32, caps: Vec<CollabCapability>) -> CollabServerInfo {
    CollabServerInfo {
        server_url: url.into(),
        discovery: None,
        profile: Some(NegotiatedProfile {
            server_id: "s".into(),
            protocol,
            capabilities: caps,
            limits: Default::default(),
            policy: Default::default(),
        }),
        error: None,
    }
}

#[test]
fn vnext_needs_protocol_3_and_e2ee() {
    assert!(!supports_vnext(None));
    assert!(!supports_vnext(Some(&info(
        "u",
        2,
        vec![CollabCapability::E2ee]
    ))));
    assert!(!supports_vnext(Some(&info("u", 3, vec![]))));
    assert!(supports_vnext(Some(&info(
        "u",
        3,
        vec![CollabCapability::E2ee]
    ))));
    let mut legacy = info("u", 3, vec![CollabCapability::E2ee]);
    legacy.profile = None;
    assert!(
        !supports_vnext(Some(&legacy)),
        "failed negotiation falls back to v2"
    );
}

#[test]
fn discovery_runs_once_per_server_and_resets_on_sign_out() {
    let mut v = VNextState::default();
    assert!(v.on_status(Some("https://a"), false).is_none());
    assert!(matches!(
        v.on_status(Some("https://a"), true),
        Some(ClientMessage::DiscoverCollabServer { server_url: Some(u) }) if u == "https://a"
    ));
    assert!(v.on_status(Some("https://a/"), true).is_none(), "in flight");
    v.on_server_info(&info("https://other", 3, vec![CollabCapability::E2ee]));
    assert!(!v.supported(), "reply for another server is ignored");
    let (handled, _) = v.apply(&DaemonMessage::CollabServerInfo(info(
        "https://a",
        3,
        vec![CollabCapability::E2ee],
    )));
    assert!(handled && v.supported());
    assert!(v.on_status(Some("https://a"), true).is_none());
    assert!(
        v.on_status(Some("https://b"), true).is_some(),
        "server changed"
    );
    assert!(!v.supported());
    v.on_status(None, false);
    assert!(!v.supported());
}

#[test]
fn errors_only_reach_a_wizard_that_is_waiting() {
    let mut v = VNextState::default();
    v.join = Some(JoinFlow::new(WorkspaceId::from("w")));
    let err = DaemonMessage::Error {
        code: "x".into(),
        message: "boom".into(),
    };
    assert!(!v.apply(&err).0, "v2 errors pass through");
    let start = v.join.as_mut().unwrap().start("s1");
    assert!(start.is_some());
    v.sent(WizardOp::Join);
    assert!(v.apply(&err).0);
    assert!(matches!(
        v.join.as_ref().unwrap().step,
        super::super::wizard::join_flow::JoinStep::RequestFailed(_)
    ));
    assert!(!v.apply(&err).0);
}

#[test]
fn dead_connection_fails_the_step() {
    let mut v = VNextState::default();
    v.share = Some(ShareFlow::new(ThreadId::from("t"), "T".into()));
    let f = v.share.as_mut().unwrap();
    f.next();
    f.next();
    v.fail(WizardOp::Share, "down".into());
    assert!(matches!(
        v.share.as_ref().unwrap().step,
        super::super::wizard::share_flow::ShareStep::Failed(_)
    ));
}

#[test]
fn support_is_checking_until_a_definitive_answer() {
    let mut v = VNextState::default();
    assert_eq!(v.support(), ServerSupport::Legacy, "signed out");
    v.on_status(Some("https://a"), true);
    assert_eq!(v.support(), ServerSupport::Checking);
    v.deferred = Some(DeferredOpen::Share);
    assert_eq!(v.take_deferred(), None, "still checking");
    let mut failed = info("https://a", 3, vec![]);
    failed.profile = None;
    failed.error = Some("timeout".into());
    v.apply(&DaemonMessage::CollabServerInfo(failed));
    assert_eq!(v.support(), ServerSupport::Legacy);
    assert_eq!(v.take_deferred(), Some(DeferredOpen::Share));
    assert_eq!(v.take_deferred(), None);

    v.on_status(Some("https://b"), true);
    let err = DaemonMessage::Error {
        code: "x".into(),
        message: "y".into(),
    };
    assert!(!v.apply(&err).0, "error still reaches its owner");
    assert_eq!(
        v.support(),
        ServerSupport::Legacy,
        "failed check falls back"
    );
    v.on_status(Some("https://c"), true);
    v.apply(&DaemonMessage::CollabServerInfo(info(
        "https://c",
        3,
        vec![CollabCapability::E2ee],
    )));
    assert_eq!(v.support(), ServerSupport::VNext);
}

#[test]
fn reopening_resumes_wizards() {
    use super::super::wizard::join_flow::JoinStep;
    use super::super::wizard::share_flow::ShareStep;
    let mut v = VNextState::default();
    v.open_share(ThreadId::from("t"), "T".into());
    let f = v.share.as_mut().unwrap();
    f.next();
    f.next();
    v.sent(WizardOp::Share);
    v.open_share(ThreadId::from("t"), "T".into());
    assert_eq!(
        v.share.as_ref().unwrap().step,
        ShareStep::Checking,
        "resumed"
    );
    assert_eq!(
        v.corr.pending,
        Some(WizardOp::Share),
        "reply still expected"
    );
    v.open_share(ThreadId::from("u"), "U".into());
    assert_eq!(
        v.share.as_ref().unwrap().step,
        ShareStep::Scope,
        "other thread starts over"
    );
    assert_eq!(v.corr.pending, None);

    v.open_join(WorkspaceId::from("w"));
    v.join.as_mut().unwrap().start("s1");
    v.open_join(WorkspaceId::from("w"));
    assert_eq!(v.join.as_ref().unwrap().step, JoinStep::Requesting);
    v.join.as_mut().unwrap().reset();
    v.open_join(WorkspaceId::from("w2"));
    assert_eq!(
        v.join.as_ref().unwrap().workspace_id,
        WorkspaceId::from("w2")
    );
}

#[test]
fn slow_server_check_times_out_to_v2_with_a_note() {
    let mut v = VNextState::default();
    v.on_status(Some("https://a"), true);
    v.deferred = Some(DeferredOpen::Join);
    assert!(!v.time_out("https://other"));
    assert!(v.time_out("https://a"));
    assert_eq!(v.support(), ServerSupport::Legacy);
    assert!(v.note.is_some());
    assert_eq!(v.take_deferred(), Some(DeferredOpen::Join));
    // A late answer is ignored; a new check clears the note.
    v.apply(&DaemonMessage::CollabServerInfo(info(
        "https://a",
        3,
        vec![CollabCapability::E2ee],
    )));
    assert_eq!(v.support(), ServerSupport::Legacy);
    assert!(!v.time_out("https://a"));
    v.on_status(Some("https://b"), true);
    assert!(v.note.is_none());
}

#[test]
fn history_marks_unleased_messages_and_closed_shares() {
    use trama_domain::ids::MessageId;
    use trama_protocol::{
        ShareHistoryEntry, ShareHistoryState, ShareRole, ShareScope, ShareSnapshot, ShareStatus,
    };
    let mut v = VNextState::default();
    let t = ThreadId::from("t");
    let entries = vec![
        ShareHistoryEntry {
            message_id: MessageId::from("m1"),
            state: ShareHistoryState::Shared,
        },
        ShareHistoryEntry {
            message_id: MessageId::from("m2"),
            state: ShareHistoryState::NotSharedNoLease,
        },
    ];
    let (handled, _) = v.apply(&DaemonMessage::ShareHistoryStatus {
        thread_id: t.clone(),
        entries,
    });
    assert!(handled);
    assert_eq!(
        v.history.unshared(&t).into_iter().collect::<Vec<_>>(),
        [MessageId::from("m2")]
    );
    v.apply(&DaemonMessage::ShareHistoryStatus {
        thread_id: t.clone(),
        entries: vec![],
    });
    assert!(v.history.unshared(&t).is_empty());

    let share = |status| ShareSnapshot {
        share_id: "s".into(),
        scope: ShareScope::ThreadOnly,
        status,
        role: ShareRole::Reader,
        server_url: None,
        root_thread_id: Some(t.clone()),
        snapshot_version: 1,
        key_epoch: None,
    };
    let (handled, _) = v.apply(&DaemonMessage::Shares(vec![share(ShareStatus::Closed)]));
    assert!(!handled, "the share list stays available to others");
    assert!(v.history.ended(&t).is_some());
    v.apply(&DaemonMessage::Shares(vec![share(ShareStatus::Ready)]));
    assert!(v.history.ended(&t).is_none());
}
