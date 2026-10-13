//! End to end: checkout content restored on join (real Git checkouts) and
//! the durable share-op retry queue.

use std::path::{Path, PathBuf};

use trama_domain::ids::ThreadId;
use trama_domain::share::ShareId;
use trama_protocol::{ClientMessage, DaemonMessage, ShareInvite, ShareRole, ShareScope};

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
    let gid = &rows[0].thread_gid.0;
    let prefix = format!("share_checkout:{share_id}:{gid}:");
    let mut restored = None;
    // One repository: find its key by listing the share-checkouts dir.
    let base = bob.app_dir().join("share-checkouts");
    for entry in walk(&base) {
        if entry.join(".git").exists() {
            restored = Some(entry);
        }
    }
    let dst = restored.expect("checkout restored");
    let key_repo = dst.file_name().unwrap().to_string_lossy().into_owned();
    let repo_gid = key_repo.rsplit_once("-v").unwrap().0.to_owned();
    let recorded = bob
        .collab
        .db()
        .collab_setting_or_insert(&format!("{prefix}{repo_gid}"), "")
        .await
        .unwrap();
    assert_eq!(PathBuf::from(recorded), dst);

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
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && p.file_name().is_some_and(|n| n != ".git") {
            out.push(p.clone());
            if !p.join(".git").exists() {
                out.extend(walk(&p));
            }
        }
    }
    out
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
