use super::*;
use crate::TramaDb;
use trama_domain::share::{
    DeviceId, RepoGid, ServerId, ShareId, SharePolicy, ShareScope, ThreadGid,
};

const T: &str = "2026-10-13T08:00:00Z";

fn server(fp: Option<&str>) -> CollaborationServerRow {
    CollaborationServerRow {
        server_id: ServerId::from("srv_1"),
        label: "Company".into(),
        base_url: "https://collab.company.test".into(),
        server_fingerprint: fp.map(str::to_string),
        trust_policy: "enterprise_pinned".into(),
        created_at: T.into(),
    }
}

fn share() -> ShareRow {
    ShareRow {
        share_id: ShareId::from("share_1"),
        server_id: Some(ServerId::from("srv_1")),
        scope: ShareScope::Subtree,
        policy: SharePolicy::default(),
        root_gid: Some(ThreadGid::from("thread_main")),
        current_snapshot_version: 0,
        status: ShareStatus::Preparing,
        created_at: T.into(),
    }
}

async fn seeded() -> TramaDb {
    let db = TramaDb::open_in_memory().unwrap();
    db.upsert_collaboration_server(server(Some("fp1")))
        .await
        .unwrap();
    db.insert_share(share()).await.unwrap();
    db.upsert_share_threads(vec![ShareThreadRow {
        share_id: ShareId::from("share_1"),
        thread_gid: ThreadGid::from("thread_main"),
        local_thread_id: Some("local_1".into()),
        parent_gid: None,
        source_scope_root: true,
        kind: "plain".into(),
    }])
    .await
    .unwrap();
    db
}

fn state(generation: u64, previous: Option<u64>) -> ShareCheckoutStateRow {
    ShareCheckoutStateRow {
        share_id: ShareId::from("share_1"),
        thread_gid: ThreadGid::from("thread_main"),
        repo_gid: RepoGid::from("repo_backend"),
        generation,
        previous_generation: previous,
        head_oid: "h".into(),
        index_tree_oid: "i".into(),
        working_tree_oid: "w".into(),
        manifest_content_id: "c".into(),
        status: "ready".into(),
        created_at: T.into(),
    }
}

#[tokio::test]
async fn server_fingerprint_is_pinned() {
    let db = seeded().await;
    assert!(
        db.upsert_collaboration_server(server(Some("fp2")))
            .await
            .is_err()
    );
    assert!(db.upsert_collaboration_server(server(None)).await.is_err());
    let mut renamed = server(Some("fp1"));
    renamed.label = "Renamed".into();
    db.upsert_collaboration_server(renamed).await.unwrap();
    let row = db
        .collaboration_server(&ServerId::from("srv_1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.label, "Renamed");
    assert_eq!(db.collaboration_servers().await.unwrap().len(), 1);
}

#[tokio::test]
async fn share_roundtrip_status_and_snapshot_cas() {
    let db = seeded().await;
    let id = ShareId::from("share_1");
    assert_eq!(db.share(&id).await.unwrap().unwrap(), share());
    assert!(db.insert_share(share()).await.is_err());
    assert!(db.advance_share_snapshot(&id, 0, 1).await.unwrap());
    assert!(!db.advance_share_snapshot(&id, 0, 2).await.unwrap());
    assert!(db.advance_share_snapshot(&id, 1, 1).await.is_err());
    db.set_share_status(&id, ShareStatus::Closed).await.unwrap();
    assert!(db.set_share_status(&id, ShareStatus::Ready).await.is_err());
    let row = db.share(&id).await.unwrap().unwrap();
    assert_eq!(
        (row.current_snapshot_version, row.status),
        (1, ShareStatus::Closed)
    );
    assert!(
        db.set_share_status(&ShareId::from("nope"), ShareStatus::Ready)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unknown_server_is_rejected_by_fk() {
    let db = TramaDb::open_in_memory().unwrap();
    assert!(db.insert_share(share()).await.is_err());
}

#[tokio::test]
async fn threads_map_local_ids_and_cascade_states() {
    let db = seeded().await;
    let id = ShareId::from("share_1");
    let local = trama_domain::ids::ThreadId::from("local_1");
    assert_eq!(
        db.share_thread_gid(&id, &local).await.unwrap(),
        Some(ThreadGid::from("thread_main"))
    );
    db.append_share_checkout_state(state(0, None))
        .await
        .unwrap();
    assert!(
        db.remove_share_thread(&id, &ThreadGid::from("thread_main"))
            .await
            .unwrap()
    );
    assert!(db.share_checkout_states(&id).await.unwrap().is_empty());
}

#[tokio::test]
async fn checkout_generations_are_cas_appended() {
    let db = seeded().await;
    let id = ShareId::from("share_1");
    assert!(
        db.append_share_checkout_state(state(1, Some(0)))
            .await
            .is_err()
    );
    db.append_share_checkout_state(state(0, None))
        .await
        .unwrap();
    assert!(
        db.append_share_checkout_state(state(1, None))
            .await
            .is_err()
    );
    db.append_share_checkout_state(state(1, Some(0)))
        .await
        .unwrap();
    assert!(
        db.append_share_checkout_state(state(2, Some(0)))
            .await
            .is_err()
    );
    let latest = db
        .latest_share_checkout_state(
            &id,
            &ThreadGid::from("thread_main"),
            &RepoGid::from("repo_backend"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest, state(1, Some(0)));
    assert_eq!(db.share_checkout_states(&id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn inventory_and_acks() {
    let db = seeded().await;
    let id = ShareId::from("share_1");
    let mut obj = ShareObjectRow {
        share_id: id.clone(),
        content_id: "content_1".into(),
        epoch: 3,
        byte_len: 1024,
        state: ObjectState::Pending,
    };
    db.upsert_share_object(obj.clone()).await.unwrap();
    obj.state = ObjectState::Uploaded;
    db.upsert_share_object(obj.clone()).await.unwrap();
    let mut grown = obj.clone();
    grown.byte_len = 1;
    assert!(db.upsert_share_object(grown).await.is_err());
    assert_eq!(
        db.share_objects(&id, None).await.unwrap(),
        vec![obj.clone()]
    );
    assert!(
        db.share_objects(&id, Some(ObjectState::Pending))
            .await
            .unwrap()
            .is_empty()
    );

    let mut ack = SharePeerAck {
        share_id: id.clone(),
        device_id: DeviceId::from("dev_bob"),
        event_cursor: "c1".into(),
        content_watermark: None,
    };
    db.put_share_peer_ack(ack.clone()).await.unwrap();
    ack.event_cursor = "c2".into();
    db.put_share_peer_ack(ack.clone()).await.unwrap();
    assert_eq!(db.share_peer_acks(&id).await.unwrap(), vec![ack]);
}
