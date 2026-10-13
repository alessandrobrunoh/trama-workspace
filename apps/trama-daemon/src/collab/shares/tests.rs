//! Share/Join end to end between two daemons through the real service,
//! plus the secret scan over files and Git objects.

use std::path::Path;

use trama_collab_protocol::vnext::bodies::{HistoryAppendedBody, HistoryMessage, HistoryRole};
use trama_collab_protocol::vnext::events::ShareEventKind;
use trama_domain::secrets::{SecretRule, SecretScanReport};
use trama_domain::share::ShareId;
use trama_protocol::{ClientMessage, DaemonMessage, ShareInvite, ShareRole, ShareScope};

use super::super::tests::{Node, Server, eventually, git};
use super::outbound::Payload;
use crate::testing::FakeAdapter;

fn fail(m: &DaemonMessage) {
    if let DaemonMessage::Error { message, .. } = m {
        panic!("daemon error: {message}");
    }
}

#[tokio::test]
async fn owner_shares_device_joins_and_history_flows_encrypted() {
    let server = Server::start().await;
    let alice = Node::start(FakeAdapter::scripted()).await;
    let bob = Node::start(FakeAdapter::scripted()).await;
    let mut a = alice.ipc().await;
    let mut b = bob.ipc().await;
    a.connect(&server, "alice", None).await;
    b.connect(&server, "bob", None).await;
    let root = a.create_thread(&alice.workspace_id, "Shared root").await;

    let report = a
        .request(
            ClientMessage::SharePreflight {
                thread_id: root.clone(),
                scope: ShareScope::Subtree,
            },
            "preflight",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::SharePreflightReport(r) => Some(r.clone()),
                    _ => None,
                }
            },
        )
        .await;
    assert!(report.ok, "{:?}", report.blockers);
    assert_eq!(report.threads.len(), 1);

    let created = a
        .request(
            ClientMessage::CreateShare {
                thread_id: root.clone(),
                scope: ShareScope::Subtree,
                members: vec![ShareInvite {
                    username: "bob".into(),
                    role: ShareRole::Writer,
                    read_code: false,
                }],
            },
            "share created",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::ShareCreated(s) => Some(s.clone()),
                    DaemonMessage::SharePreflightReport(r) => panic!("blocked: {r:?}"),
                    _ => None,
                }
            },
        )
        .await;
    assert_eq!(created.role, ShareRole::Owner);
    assert_eq!(created.snapshot_version, 1);
    assert_eq!(created.key_epoch, Some(0));
    assert_eq!(created.root_thread_id.as_ref(), Some(&root));
    let share_id = created.share_id.clone();

    let device = b
        .request(
            ClientMessage::RequestShareJoin {
                share_id: share_id.clone(),
            },
            "join requested",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::ShareJoinRequested { device_id, .. } => Some(device_id.clone()),
                    _ => None,
                }
            },
        )
        .await;
    let requests = a
        .request(
            ClientMessage::ListShareJoinRequests {
                share_id: share_id.clone(),
            },
            "join requests",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::ShareJoinRequests { requests, .. } => Some(requests.clone()),
                    _ => None,
                }
            },
        )
        .await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].username, "bob");
    let epoch = a
        .request(
            ClientMessage::ApproveShareJoin {
                share_id: share_id.clone(),
                device_id: device.clone(),
            },
            "join approved",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::ShareJoinApproved { key_epoch, .. } => Some(*key_epoch),
                    _ => None,
                }
            },
        )
        .await;
    assert_eq!(epoch, 1);
    let joined = b
        .request(
            ClientMessage::CompleteShareJoin {
                share_id: share_id.clone(),
                workspace_id: bob.workspace_id.clone(),
            },
            "joined",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::ShareJoined(s) => Some(s.clone()),
                    _ => None,
                }
            },
        )
        .await;
    assert_eq!(joined.role, ShareRole::Writer);
    assert_eq!(joined.key_epoch, Some(1));
    assert_eq!(joined.snapshot_version, 2);
    let bob_root = joined.root_thread_id.clone().expect("root mapped");
    let title = bob
        .sessions
        .db()
        .get_thread(&bob_root)
        .await
        .unwrap()
        .unwrap()
        .title;
    assert_eq!(title.as_deref(), Some("Shared root"));

    // Alice appends history; Bob's daemon decrypts it into his thread.
    let gid = alice
        .collab
        .db()
        .share_threads(&ShareId::from(share_id.as_str()))
        .await
        .unwrap()[0]
        .thread_gid
        .0
        .clone();
    let body = HistoryAppendedBody {
        messages: vec![HistoryMessage {
            origin: "m1".into(),
            role: HistoryRole::Assistant,
            text: "hello from alice".into(),
            created_at: "2026-10-13T08:00:00Z".into(),
        }],
    };
    let (_, client) = alice.collab.share_client().unwrap();
    let sent = alice
        .collab
        .emit_share_event(
            &client,
            &share_id,
            ShareEventKind::HistoryAppended,
            Some(gid),
            Payload::Plain(serde_json::to_vec(&body).unwrap()),
        )
        .await
        .unwrap();
    assert!(!sent.cipher_payload.contains("hello"));
    eventually("history applied", || async {
        bob.messages(&bob_root).await.iter().any(|m| {
            format!("{:?}", m.content).contains("hello from alice")
                && matches!(&m.actor, trama_domain::message::Actor::Human { id } if id.starts_with("collab:"))
        })
    })
    .await;

    let lease = b
        .request(
            ClientMessage::AcquireShareLease {
                share_id: share_id.clone(),
                thread_id: bob_root.clone(),
                ttl_secs: 60,
            },
            "lease",
            |m| {
                fail(m);
                match m {
                    DaemonMessage::ShareLease(l) => Some(l.clone()),
                    _ => None,
                }
            },
        )
        .await;
    assert_eq!(lease.holder, "bob");
    assert_eq!(lease.thread_id.as_ref(), Some(&bob_root));
    let shares = a
        .request(ClientMessage::ListShares, "shares", |m| {
            fail(m);
            match m {
                DaemonMessage::Shares(s) => Some(s.clone()),
                _ => None,
            }
        })
        .await;
    assert_eq!(shares.len(), 1);
    assert_eq!(shares[0].key_epoch, Some(1));
}

fn repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
}

#[test]
fn scan_covers_working_files_index_and_unpublished_commits() {
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path();
    repo(wt);
    std::fs::write(wt.join("ok.txt"), "nothing here\n").unwrap();
    git(wt, &["add", "ok.txt"]);
    git(wt, &["commit", "-m", "clean"]);
    let mut report = SecretScanReport::default();
    super::scan::scan_checkout(wt, &mut report).unwrap();
    assert!(report.findings.is_empty(), "{:?}", report.findings);

    // A key committed then deleted still travels in the bundle.
    let key = "aws_access_key_id = AKIAIOSFODNN7EXAMPLQ\n";
    std::fs::write(wt.join("conf.txt"), key).unwrap();
    git(wt, &["add", "conf.txt"]);
    git(wt, &["commit", "-m", "oops"]);
    git(wt, &["rm", "-q", "conf.txt"]);
    git(wt, &["commit", "-m", "remove"]);
    let mut report = SecretScanReport::default();
    super::scan::scan_checkout(wt, &mut report).unwrap();
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.path == "conf.txt" && f.rule == SecretRule::AwsAccessKeyId),
        "{:?}",
        report.findings
    );

    // Untracked files and staged blobs are scanned too.
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path();
    repo(wt);
    std::fs::write(wt.join(".env"), "X=1\n").unwrap();
    std::fs::write(wt.join("staged.txt"), key).unwrap();
    git(wt, &["add", "staged.txt"]);
    std::fs::write(wt.join("staged.txt"), "clean now\n").unwrap();
    let mut report = SecretScanReport::default();
    let scanned = super::scan::scan_checkout(wt, &mut report).unwrap();
    assert!(scanned >= 3);
    let rules: Vec<_> = report
        .findings
        .iter()
        .map(|f| (f.path.as_str(), f.rule))
        .collect();
    assert!(
        rules.contains(&(".env", SecretRule::DotEnvFile)),
        "{rules:?}"
    );
    assert!(
        rules.contains(&("staged.txt", SecretRule::AwsAccessKeyId)),
        "{rules:?}"
    );
}
