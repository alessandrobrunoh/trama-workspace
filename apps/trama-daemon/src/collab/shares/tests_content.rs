//! End to end: checkout content restored on join (real Git checkouts) and
//! the durable share-op retry queue.

use std::path::{Path, PathBuf};

use trama_domain::ids::ThreadId;
use trama_domain::share::ShareId;
use trama_protocol::{
    ClientMessage, DaemonMessage, ShareInvite, ShareRestoreState, ShareRole, ShareScope,
    ShareStatusReport,
};

use super::super::tests::{Ipc, Node, Server, git};
use super::outbound::Payload;
use super::retry::OP_EVENT;
use crate::testing::FakeAdapter;
use trama_collab_protocol::vnext::events::ShareEventKind;

fn fail(m: &DaemonMessage) {
    if let DaemonMessage::Error { message, .. } = m {
        panic!("daemon error: {message}");
    }
}

fn out(cwd: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}");
    String::from_utf8(o.stdout).unwrap().trim().to_owned()
}

/// Alice shares `root` with Bob (writer, `read_code` as given) and Bob
/// joins. Returns the share id and Bob's root thread.
async fn share_and_join(
    a: &mut Ipc,
    b: &mut Ipc,
    bob: &Node,
    root: &ThreadId,
    read_code: bool,
) -> (String, ThreadId) {
    let created = a
        .request(
            ClientMessage::CreateShare {
                thread_id: root.clone(),
                scope: ShareScope::Subtree,
                members: vec![ShareInvite {
                    username: "bob".into(),
                    role: ShareRole::Writer,
                    read_code,
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
    let share_id = created.share_id;
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
    a.request(
        ClientMessage::ApproveShareJoin {
            share_id: share_id.clone(),
            device_id: device,
        },
        "join approved",
        |m| {
            fail(m);
            matches!(m, DaemonMessage::ShareJoinApproved { .. }).then_some(())
        },
    )
    .await;
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
    (share_id, joined.root_thread_id.expect("root mapped"))
}

async fn checkout_of(node: &Node, thread: &ThreadId) -> PathBuf {
    let db = node.sessions.db();
    let t = db.get_thread(thread).await.unwrap().unwrap();
    let att = t
        .repo_attachments
        .iter()
        .find(|a| a.checkout_id.is_some())
        .expect("thread has a checkout");
    db.checkout_location(
        thread,
        &att.repository_id,
        att.checkout_id.as_ref().unwrap(),
    )
    .await
    .unwrap()
    .expect("checkout location")
}

#[tokio::test]
async fn joiner_restores_the_owner_checkout_exactly() {
    let server = Server::start().await;
    let alice = Node::start(FakeAdapter::scripted()).await;
    let bob = Node::start(FakeAdapter::scripted()).await;
    let mut a = alice.ipc().await;
    let mut b = bob.ipc().await;
    a.connect(&server, "alice", None).await;
    b.connect(&server, "bob", None).await;
    let root = a.create_thread(&alice.workspace_id, "Code root").await;
    let src = checkout_of(&alice, &root).await;
    // A private commit, a staged change, an unstaged edit, an untracked
    // file and CRLF/binary bytes that must survive unchanged.
    std::fs::write(src.join("feature.rs"), b"fn main() {}\r\n\x00tail\n").unwrap();
    git(&src, &["add", "feature.rs"]);
    git(&src, &["commit", "-m", "private work"]);
    std::fs::write(src.join("staged.txt"), "staged\n").unwrap();
    git(&src, &["add", "staged.txt"]);
    std::fs::write(src.join("same.txt"), "edited, not staged\n").unwrap();
    std::fs::write(src.join("notes.txt"), "untracked\n").unwrap();

    let (share_id, bob_root) = share_and_join(&mut a, &mut b, &bob, &root, true).await;
    let rows = bob
        .collab
        .db()
        .share_threads(&ShareId::from(share_id.as_str()))
        .await
        .unwrap();
    assert_eq!(
        rows[0].local_thread_id.as_deref(),
        Some(bob_root.to_string().as_str())
    );
    // The restored checkout is the thread's real (managed) checkout.
    let dst = checkout_of(&bob, &bob_root).await;
    // Stored paths are canonical (`/private/var` on macOS).
    let base = dunce::canonicalize(bob.app_dir().join("share-checkouts")).unwrap();
    assert!(dunce::canonicalize(&dst).unwrap().starts_with(&base));
    assert_eq!(
        out(&dst, &["rev-parse", "HEAD"]),
        out(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        out(&dst, &["symbolic-ref", "HEAD"]),
        out(&src, &["symbolic-ref", "HEAD"])
    );
    assert_eq!(out(&dst, &["write-tree"]), out(&src, &["write-tree"]));
    for file in ["feature.rs", "staged.txt", "same.txt", "notes.txt"] {
        assert_eq!(
            std::fs::read(dst.join(file)).unwrap(),
            std::fs::read(src.join(file)).unwrap(),
            "{file}"
        );
    }

    let report = share_status(
        &mut b,
        ClientMessage::GetShareStatus {
            share_id: share_id.clone(),
        },
    )
    .await;
    assert_eq!(report.restore, ShareRestoreState::Restored);
    assert_eq!(report.checkouts.len(), 1);
    assert_eq!(PathBuf::from(&report.checkouts[0].path), dst);
    assert_eq!(report.checkouts[0].thread_id.as_ref(), Some(&bob_root));
    // A retry keeps the attached checkout.
    let again = share_status(
        &mut b,
        ClientMessage::RetryShareRestore {
            share_id: share_id.clone(),
        },
    )
    .await;
    assert_eq!(again.restore, ShareRestoreState::Restored);
    assert_eq!(again.checkouts, report.checkouts);
    assert_eq!(checkout_of(&bob, &bob_root).await, dst);

    // A newer snapshot is offered, not applied.
    std::fs::write(src.join("update.txt"), "v2\n").unwrap();
    git(&src, &["add", "update.txt"]);
    git(&src, &["commit", "-m", "update"]);
    let (_, alice_client) = alice.collab.share_client().unwrap();
    let v2 = alice
        .collab
        .publish_history_snapshot(&alice_client, &share_id, false)
        .await
        .unwrap();
    let offered = share_status(
        &mut b,
        ClientMessage::GetShareStatus {
            share_id: share_id.clone(),
        },
    )
    .await;
    assert_eq!(offered.update_available, Some(v2));
    assert!(!dst.join("update.txt").exists());
    // Local changes block the update; nothing is touched.
    std::fs::write(dst.join("mine.txt"), "mine\n").unwrap();
    let refused = b
        .request(
            ClientMessage::ApplyShareUpdate {
                share_id: share_id.clone(),
            },
            "update refused",
            |m| match m {
                DaemonMessage::Error { message, .. } => Some(message.clone()),
                DaemonMessage::ShareStatusReport(_) => panic!("update applied over local changes"),
                _ => None,
            },
        )
        .await;
    assert!(refused.contains("local changes"), "{refused}");
    assert!(!dst.join("update.txt").exists());
    std::fs::remove_file(dst.join("mine.txt")).unwrap();
    let updated = share_status(
        &mut b,
        ClientMessage::ApplyShareUpdate {
            share_id: share_id.clone(),
        },
    )
    .await;
    assert_eq!(updated.update_available, None);
    assert_eq!(updated.checkouts, report.checkouts);
    assert_eq!(
        out(&dst, &["rev-parse", "HEAD"]),
        out(&src, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        std::fs::read(dst.join("same.txt")).unwrap(),
        std::fs::read(src.join("same.txt")).unwrap()
    );
    assert!(dst.join("update.txt").exists());

    // A deleted checkout folder is recreated in place, with one repository.
    std::fs::remove_dir_all(&dst).unwrap();
    share_status(
        &mut b,
        ClientMessage::RetryShareRestore {
            share_id: share_id.clone(),
        },
    )
    .await;
    assert_eq!(checkout_of(&bob, &bob_root).await, dst);
    assert!(dst.join("update.txt").exists());
    let sources = bob
        .collab
        .db()
        .list_all_repositories()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| {
            r.local_path
                .as_ref()
                .and_then(|p| dunce::canonicalize(p).ok())
                .is_some_and(|p| p.starts_with(&base))
        })
        .count();
    assert_eq!(sources, 1);

    // A secret in the owner's checkout: a membership-change republish keeps
    // history without code and records a warning; a normal one refuses.
    std::fs::write(
        src.join("conf.txt"),
        "aws_access_key_id = AKIAIOSFODNN7EXAMPLQ\n",
    )
    .unwrap();
    let (_, client) = alice.collab.share_client().unwrap();
    alice
        .collab
        .publish_history_snapshot(&client, &share_id, true)
        .await
        .unwrap();
    assert!(
        alice
            .collab
            .publish_history_snapshot(&client, &share_id, false)
            .await
            .is_err()
    );
    let owner = share_status(&mut a, ClientMessage::GetShareStatus { share_id }).await;
    assert_eq!(owner.restore, ShareRestoreState::NotRequested);
    assert_eq!(owner.warnings[0].code, "content_skipped");
}

async fn share_status(ipc: &mut Ipc, msg: ClientMessage) -> ShareStatusReport {
    ipc.request(msg, "share status", |m| {
        fail(m);
        match m {
            DaemonMessage::ShareStatusReport(r) => Some(r.clone()),
            _ => None,
        }
    })
    .await
}

#[tokio::test]
async fn share_ops_retry_in_order_and_block_other_events() {
    let server = Server::start().await;
    let alice = Node::start(FakeAdapter::scripted()).await;
    let bob = Node::start(FakeAdapter::scripted()).await;
    let mut a = alice.ipc().await;
    let mut b = bob.ipc().await;
    a.connect(&server, "alice", None).await;
    b.connect(&server, "bob", None).await;
    let root = a.create_thread(&alice.workspace_id, "Retry root").await;
    let (share_id, _) = share_and_join(&mut a, &mut b, &bob, &root, false).await;
    let share = ShareId::from(share_id.as_str());
    let version = || async {
        alice
            .collab
            .db()
            .share(&share)
            .await
            .unwrap()
            .unwrap()
            .current_snapshot_version
    };
    let before = version().await;

    // An undeliverable membership commit ahead of a snapshot republish.
    let db = alice.collab.db();
    let stuck = db
        .enqueue_share_op(&share_id, OP_EVENT, "not json")
        .await
        .unwrap();
    alice.collab.enqueue_snapshot(&share_id).await.unwrap();
    assert!(!alice.collab.flush_share_ops(Some(&share_id)).await);
    let pending = db.share_pending_ops().await.unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].attempts, 1);
    assert_eq!(pending[1].attempts, 0, "later ops wait for the first");
    assert_eq!(version().await, before);
    // Ordinary events of the share are held back meanwhile.
    let (_, client) = alice.collab.share_client().unwrap();
    assert!(
        alice
            .collab
            .emit_share_event(
                &client,
                &share_id,
                ShareEventKind::ShareCreated,
                None,
                Payload::Plain(b"{}".to_vec()),
            )
            .await
            .is_err()
    );

    // Once the head op is through (here: acknowledged), the queue drains.
    db.finish_share_op(stuck).await.unwrap();
    assert!(alice.collab.flush_share_ops(Some(&share_id)).await);
    assert!(db.share_pending_ops().await.unwrap().is_empty());
    assert_eq!(version().await, before + 1);
}
