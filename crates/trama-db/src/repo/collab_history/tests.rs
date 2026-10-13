use super::*;
use trama_domain::thread::{Thread, ThreadKind};
use trama_domain::workspace::Workspace;

async fn linked_thread(db: &TramaDb) -> ThreadId {
    let workspace = Workspace::new("history");
    db.create_workspace(&workspace).await.unwrap();
    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id),
        parent_thread_id: None,
        kind: ThreadKind::Top,
        title: Some("t".into()),
        repo_attachments: vec![],
    };
    db.create_thread(&thread).await.unwrap();
    db.link_collab_thread(&thread.id, "http://127.0.0.1:1", "c1")
        .await
        .unwrap();
    thread.id
}

fn job(key: &str, thread: &ThreadId) -> OutboxJob {
    OutboxJob {
        dedup_key: key.into(),
        command_id: format!("cmd-{key}"),
        thread_id: thread.clone(),
        job: "message".into(),
        ref_id: None,
        payload_json: None,
    }
}

#[tokio::test]
async fn outbox_dedups_keeps_order_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trama.db");
    let db = TramaDb::open(&path).unwrap();
    let thread = linked_thread(&db).await;
    let added = db
        .enqueue_collab_outbox(vec![job("a", &thread), job("b", &thread)], vec![])
        .await
        .unwrap();
    assert_eq!(added, 2);
    // A rescan enqueues nothing twice.
    let added = db
        .enqueue_collab_outbox(vec![job("a", &thread), job("c", &thread)], vec![])
        .await
        .unwrap();
    assert_eq!(added, 1);
    drop(db);
    let db = TramaDb::open(&path).unwrap();
    let pending = db.collab_outbox_pending(10).await.unwrap();
    let keys: Vec<_> = pending.iter().map(|r| r.job.dedup_key.as_str()).collect();
    assert_eq!(keys, ["a", "b", "c"]);
    db.collab_outbox_retry_at(pending[0].id, Utc::now(), "down")
        .await
        .unwrap();
    db.collab_outbox_delivered(&[pending[0].id]).await.unwrap();
    db.collab_outbox_failed(pending[1].id, "rejected")
        .await
        .unwrap();
    let pending = db.collab_outbox_pending(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].job.dedup_key, "c");
    let counts = db.collab_outbox_counts().await.unwrap();
    assert_eq!(counts.get("delivered"), Some(&1));
    assert_eq!(counts.get("failed"), Some(&1));
}

#[tokio::test]
async fn applied_messages_keep_log_order_and_apply_once() {
    let db = TramaDb::open_in_memory().unwrap();
    let thread = linked_thread(&db).await;
    let mut first = Message::new_system(thread.clone(), "first");
    first.created_at = Utc::now();
    let mut older = Message::new_system(thread.clone(), "second, stamped earlier");
    older.created_at = first.created_at - Duration::seconds(60);
    let apply = |message: Message, origin: &str| CollabApply {
        messages: vec![(message, Some(origin.to_owned()))],
        ..CollabApply::default()
    };
    assert!(
        db.apply_collab_changes(&thread, 1, apply(first, "o1"))
            .await
            .unwrap()
            .is_some()
    );
    // Staged activity is stored with the message that ends the turn.
    let stage = CollabApply {
        stage_activity: Some((
            "o2".into(),
            0,
            serde_json::to_string(&Vec::<ActivityItem>::new()).unwrap(),
        )),
        ..CollabApply::default()
    };
    db.apply_collab_changes(&thread, 2, stage).await.unwrap();
    let applied = db
        .apply_collab_changes(&thread, 3, apply(older, "o2"))
        .await
        .unwrap()
        .unwrap();
    assert!(applied.messages[0].created_at > Utc::now() - Duration::seconds(5));
    // Replaying an applied seq writes nothing.
    let again = Message::new_system(thread.clone(), "dup");
    assert!(
        db.apply_collab_changes(&thread, 3, apply(again, "o3"))
            .await
            .unwrap()
            .is_none()
    );
    let texts: Vec<String> = db
        .list_messages_by_thread(&thread)
        .await
        .unwrap()
        .iter()
        .map(Message::text)
        .collect();
    assert_eq!(texts, ["first", "second, stamped earlier"]);
    let origins = db.collab_message_origins(&thread).await.unwrap();
    assert_eq!(origins.len(), 2);
    assert!(origins.values().all(|(_, remote)| *remote));
    let id = db.collab_origin_message(&thread, "o2").await.unwrap();
    assert!(id.is_some());
}
