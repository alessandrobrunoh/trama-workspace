use super::*;
use crate::TramaDb;
use trama_domain::share::{ShareId, SharePolicy, ShareScope};

#[tokio::test]
async fn heads_and_cursor_only_move_forward() {
    let db = TramaDb::open_in_memory().unwrap();
    db.insert_share(ShareRow {
        share_id: ShareId::from("s1"),
        server_id: None,
        scope: ShareScope::ThreadOnly,
        policy: SharePolicy::default(),
        root_gid: None,
        current_snapshot_version: 0,
        status: ShareStatus::Ready,
        created_at: "2026-10-13T08:00:00Z".into(),
    })
    .await
    .unwrap();
    let id = ShareId::from("s1");
    assert_eq!(db.share_stream_cursor(&id).await.unwrap(), 0);
    let head = |seq: u64, ev: &str| ShareDeviceHead {
        device_id: "d1".into(),
        author_seq: seq,
        event_id: ev.into(),
    };
    db.record_share_event(&id, head(2, "e2"), Some(5))
        .await
        .unwrap();
    db.record_share_event(&id, head(1, "e1"), Some(3))
        .await
        .unwrap();
    assert_eq!(
        db.share_device_heads(&id).await.unwrap(),
        vec![head(2, "e2")]
    );
    assert_eq!(db.share_stream_cursor(&id).await.unwrap(), 5);
}
