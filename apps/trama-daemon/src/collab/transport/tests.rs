//! A share stream between two daemons through the real service: events
//! reach the mapped local thread over IPC, heads and cursor persist, and
//! manifests round-trip through a pluggable cipher.

use std::sync::Arc;

use ::http::Method;
use trama_collab_protocol::vnext::api::{LeaseRequest, ShareMemberRequest, ShareRole};
use trama_collab_protocol::vnext::cipher::{CipherError, NoCipher, PayloadCipher, PayloadKind};
use trama_collab_protocol::vnext::discovery::{DISCOVERY_PATH, ServerDiscovery};
use trama_collab_protocol::vnext::events::{ShareEventEnvelope, ShareEventKind};
use trama_db::{CollaborationServerRow, ShareRow, ShareStatus, ShareThreadRow};
use trama_domain::manifest::{SignatureAlg, SignedEnvelope};
use trama_domain::share::{ServerId, ShareId, SharePolicy, ShareScope, ThreadGid};
use trama_protocol::DaemonMessage;

use super::super::tests::{Node, Server, eventually};
use super::{ShareClient, fetch_manifests, publish_manifests};
use crate::testing::FakeAdapter;

/// Reversible test cipher (byte complement); stands in for MLS.
struct FlipCipher;

impl PayloadCipher for FlipCipher {
    fn seal(&self, _: &str, _: u64, _: PayloadKind, p: &[u8]) -> Result<Vec<u8>, CipherError> {
        Ok(p.iter().map(|b| !b).collect())
    }
    fn open(&self, _: &str, _: u64, _: PayloadKind, c: &[u8]) -> Result<Vec<u8>, CipherError> {
        Ok(c.iter().map(|b| !b).collect())
    }
}

fn event(id: &str, seq: u64, prev: Option<&str>, kind: ShareEventKind) -> ShareEventEnvelope {
    ShareEventEnvelope {
        event_id: id.into(),
        share_id: "sh1".into(),
        kind,
        thread_gid: (kind == ShareEventKind::HistoryAppended).then(|| "t_main".into()),
        author_device: "dev_alice".into(),
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

fn envelope(tag: &str) -> SignedEnvelope {
    SignedEnvelope {
        payload_type: format!("trama.{tag}.v1"),
        payload_bytes: tag.as_bytes().to_vec(),
        alg: SignatureAlg::Ed25519,
        key_id: "k1".into(),
        signature: vec![1; 64],
    }
}

#[tokio::test]
async fn share_stream_delivers_persists_and_manifests_round_trip() {
    let server = Server::start().await;
    let discovery: ServerDiscovery = server
        .endpoint
        .call(Method::GET, DISCOVERY_PATH, None, None::<&()>)
        .await
        .unwrap();
    let alice_token = server.login("alice").await;
    let alice = ShareClient::new(server.endpoint.clone(), alice_token);
    alice.register("sh1").await.unwrap();
    let grant = ShareMemberRequest {
        username: "bob".into(),
        role: ShareRole::Writer,
        read_code: true,
    };
    alice.add_member("sh1", &grant).await.unwrap();

    // Bob's daemon knows the share and maps t_main to a local thread.
    let bob = Node::start(FakeAdapter::scripted()).await;
    let mut ipc = bob.ipc().await;
    let local = ipc.create_thread(&bob.workspace_id, "Shared root").await;
    let db = bob.sessions.db().clone();
    db.upsert_collaboration_server(CollaborationServerRow {
        server_id: ServerId::from(discovery.server_id.as_str()),
        label: "test".into(),
        base_url: server.url.clone(),
        server_fingerprint: None,
        trust_policy: "test".into(),
        created_at: "2026-10-13T08:00:00Z".into(),
    })
    .await
    .unwrap();
    db.insert_share(ShareRow {
        share_id: ShareId::from("sh1"),
        server_id: Some(ServerId::from(discovery.server_id.as_str())),
        scope: ShareScope::Subtree,
        policy: SharePolicy::default(),
        root_gid: Some(ThreadGid::from("t_main")),
        current_snapshot_version: 0,
        status: ShareStatus::Ready,
        created_at: "2026-10-13T08:00:00Z".into(),
    })
    .await
    .unwrap();
    db.upsert_share_threads(vec![ShareThreadRow {
        share_id: ShareId::from("sh1"),
        thread_gid: ThreadGid::from("t_main"),
        local_thread_id: Some(local.to_string()),
        parent_gid: None,
        source_scope_root: true,
        kind: "main".into(),
    }])
    .await
    .unwrap();
    ipc.subscribe(&local).await;
    ipc.connect(&server, "bob", None).await;

    alice
        .publish(
            "sh1",
            vec![event("e1", 1, None, ShareEventKind::ShareCreated)],
        )
        .await
        .unwrap();
    alice
        .publish(
            "sh1",
            vec![event("e2", 2, Some("e1"), ShareEventKind::HistoryAppended)],
        )
        .await
        .unwrap();
    for id in ["e1", "e2"] {
        let got = ipc
            .expect("share event", |m| match m {
                DaemonMessage::ShareEvent { thread_id, event } if event.event_id == id => {
                    Some(thread_id.clone())
                }
                _ => None,
            })
            .await;
        assert_eq!(got, Some(local.clone()));
    }
    let share = ShareId::from("sh1");
    eventually("cursor persisted", || async {
        db.share_stream_cursor(&share).await.unwrap() == 2
    })
    .await;
    let heads = db.share_device_heads(&share).await.unwrap();
    assert_eq!(heads[0].author_seq, 2);

    // Manifests: refused without a cipher, round-trip with one.
    let alice_db = trama_db::TramaDb::open_in_memory().unwrap();
    let none: Arc<dyn PayloadCipher> = Arc::new(NoCipher);
    assert!(
        publish_manifests(
            &alice,
            &alice_db,
            none.as_ref(),
            "sh1",
            0,
            0,
            &envelope("h"),
            None
        )
        .await
        .is_err()
    );
    let version = publish_manifests(
        &alice,
        &alice_db,
        &FlipCipher,
        "sh1",
        0,
        0,
        &envelope("history"),
        Some(&envelope("content")),
    )
    .await
    .unwrap();
    assert_eq!(version, 1);
    let bob_token = server.login("bob").await;
    let bob_client = ShareClient::new(server.endpoint.clone(), bob_token);
    let fetched = fetch_manifests(&bob_client, &FlipCipher, "sh1", 0, 0, true)
        .await
        .unwrap();
    assert_eq!(fetched.version, 1);
    assert_eq!(fetched.history, envelope("history"));
    assert_eq!(fetched.content, Some(envelope("content")));

    // Writer leases are exclusive per thread.
    let lease = |device: &str| LeaseRequest {
        thread_gid: "t_main".into(),
        device_id: device.into(),
        ttl_secs: 60,
    };
    alice
        .acquire_lease("sh1", &lease("dev_alice"))
        .await
        .unwrap();
    assert!(
        bob_client
            .acquire_lease("sh1", &lease("dev_bob"))
            .await
            .is_err()
    );
    assert_eq!(bob_client.leases("sh1").await.unwrap()[0].holder, "alice");
    alice.release_lease("sh1", "t_main").await.unwrap();
}
