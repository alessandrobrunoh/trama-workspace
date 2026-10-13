use super::events::*;
use super::replay::*;

fn event(id: &str, seq: u64, prev: Option<&str>) -> ShareEventEnvelope {
    ShareEventEnvelope {
        event_id: id.into(),
        share_id: "sh1".into(),
        kind: ShareEventKind::HistoryAppended,
        thread_gid: Some("t1".into()),
        author_device: "dev1".into(),
        author_seq: seq,
        prev_author_event: prev.map(Into::into),
        parents: vec![],
        server_seq: None,
        key_epoch: 0,
        cipher_payload: "AAAA".into(),
        signature: "c2ln".into(),
        created_at: 0,
        received_at: None,
    }
}

#[test]
fn kinds_are_snake_case_and_unknown_tags_denied() {
    let json = serde_json::to_value(ShareEventKind::CheckoutSnapshotCommitted).unwrap();
    assert_eq!(json, "checkout_snapshot_committed");
    assert!(serde_json::from_str::<ShareEventKind>(r#""share_exploded""#).is_err());
    let e = event("e1", 1, None);
    let round: ShareEventEnvelope =
        serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(round, e);
}

#[test]
fn shape_rules() {
    assert!(event("e1", 1, None).validate_shape().is_ok());
    assert_eq!(
        event("e1", 1, Some("e0")).validate_shape(),
        Err(EventShapeError::BadAuthorChain)
    );
    assert_eq!(
        event("e2", 2, None).validate_shape(),
        Err(EventShapeError::BadAuthorChain)
    );
    let mut e = event("e1", 1, None);
    e.thread_gid = None;
    assert_eq!(e.validate_shape(), Err(EventShapeError::MissingThread));
    let mut e = event("e1", 1, None);
    e.parents = vec!["e1".into()];
    assert_eq!(e.validate_shape(), Err(EventShapeError::SelfParent));
    let mut e = event("bad id", 1, None);
    e.event_id = "bad id".into();
    assert!(matches!(
        e.validate_shape(),
        Err(EventShapeError::BadId("event_id"))
    ));
}

#[test]
fn replay_guard_is_idempotent_and_detects_gaps_and_forks() {
    let mut guard = ReplayGuard::new("sh1");
    assert_eq!(guard.admit(&event("e1", 1, None)), Admission::Accept);
    assert_eq!(guard.admit(&event("e1", 1, None)), Admission::Duplicate);
    assert_eq!(
        guard.admit(&event("e3", 3, Some("e2"))),
        Admission::Gap { expected_seq: 2 }
    );
    assert!(matches!(
        guard.admit(&event("x1", 1, None)),
        Admission::Reject(_)
    ));
    assert!(matches!(
        guard.admit(&event("e2", 2, Some("zz"))),
        Admission::Reject(_)
    ));
    assert_eq!(guard.admit(&event("e2", 2, Some("e1"))), Admission::Accept);
    let mut other = event("o1", 1, None);
    other.share_id = "sh2".into();
    assert!(matches!(guard.check(&other), Admission::Reject(_)));
}
