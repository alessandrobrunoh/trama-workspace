use super::*;

#[test]
fn discover_collab_server_round_trips_with_optional_url() {
    let msg: ClientMessage =
        serde_json::from_str(r#"{"type":"discover_collab_server","payload":{}}"#).unwrap();
    assert!(matches!(
        msg,
        ClientMessage::DiscoverCollabServer { server_url: None }
    ));
    let info = DaemonMessage::CollabServerInfo(CollabServerInfo {
        server_url: "http://127.0.0.1:1".into(),
        discovery: None,
        profile: None,
        error: None,
    });
    let json = serde_json::to_value(&info).unwrap();
    assert_eq!(json["type"], "collab_server_info");
    let back: DaemonMessage = serde_json::from_value(json).unwrap();
    assert!(matches!(back, DaemonMessage::CollabServerInfo(_)));
}

#[test]
fn share_event_frame_uses_snake_case_kind() {
    let msg = DaemonMessage::ShareEvent {
        thread_id: None,
        event: ShareEventEnvelope {
            event_id: "e1".into(),
            share_id: "s1".into(),
            kind: ShareEventKind::ShareCreated,
            thread_gid: None,
            author_device: "d1".into(),
            author_seq: 1,
            prev_author_event: None,
            parents: vec![],
            server_seq: Some(1),
            key_epoch: 0,
            cipher_payload: "AA==".into(),
            signature: "AA==".into(),
            created_at: 0,
            received_at: None,
        },
    };
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["type"], "share_event");
    assert_eq!(json["payload"]["event"]["kind"], "share_created");
}

#[test]
fn share_flow_messages_round_trip_with_defaults() {
    let msg: ClientMessage = serde_json::from_str(
        r#"{"type":"acquire_share_lease","payload":{"share_id":"s","thread_id":"t"}}"#,
    )
    .unwrap();
    assert!(matches!(
        msg,
        ClientMessage::AcquireShareLease {
            ttl_secs: DEFAULT_LEASE_TTL_SECS,
            ..
        }
    ));
    let msg: ClientMessage = serde_json::from_str(
        r#"{"type":"create_share","payload":{"thread_id":"t","scope":"subtree"}}"#,
    )
    .unwrap();
    assert!(matches!(msg, ClientMessage::CreateShare { ref members, .. } if members.is_empty()));
    let report = DaemonMessage::SharePreflightReport(SharePreflightReport {
        thread_id: ThreadId::from("t"),
        scope: ShareScope::Subtree,
        ok: false,
        threads: vec![],
        repositories: vec![],
        scanned_files: 0,
        blockers: vec![ShareBlocker::GitConflicts {
            thread_id: ThreadId::from("t"),
            repository: "repo".into(),
            paths: vec!["a.txt".into()],
        }],
    });
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["payload"]["blockers"][0]["kind"], "git_conflicts");
    let back: DaemonMessage = serde_json::from_value(json).unwrap();
    assert!(matches!(back, DaemonMessage::SharePreflightReport(r) if !r.ok));
}
