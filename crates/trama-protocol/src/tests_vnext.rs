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
