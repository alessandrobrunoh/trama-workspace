//! Disk usage, archiving and removing thread checkouts with the fake agent.

use super::agents_tests::{checkout, fast_fake, fixture, git, manager, run_prompt};
use super::*;
use crate::testing::{FakeAdapter, scripted_events};
use tokio::time::Duration;
use trama_protocol::CheckoutStatus;

/// The fake agent writes `same.txt` = `<word>` for a prompt `write <word>`.
fn writing_fake() -> FakeAdapter {
    FakeAdapter::new(scripted_events(), Duration::from_millis(1)).with_on_send(|cwd, prompt| {
        if let Some(word) = prompt.strip_prefix("write ") {
            std::fs::write(cwd.join("same.txt"), format!("{word}\n")).unwrap();
        }
    })
}

#[tokio::test]
async fn disk_usage_lists_checkouts_by_private_size() {
    let (fixture, db) = fixture().await;
    let fake = fast_fake();
    let mgr = manager(&fixture, db, &fake);
    let source = fixture.dir.path().join("source");
    std::fs::write(source.join(".gitignore"), "/target/\n").unwrap();
    git(&source, &["add", ".gitignore"]);
    git(&source, &["commit", "-q", "-m", "ignore target"]);
    std::fs::create_dir_all(source.join("target")).unwrap();
    std::fs::write(source.join("target/out.bin"), vec![1u8; 512 * 1024]).unwrap();
    let small = mgr
        .create_thread(fixture.workspace.clone(), "small".into())
        .await
        .unwrap();
    let big = mgr
        .create_thread(fixture.workspace.clone(), "big".into())
        .await
        .unwrap();
    let big_checkout = checkout(&mgr, &big.id).await;
    // Build output only this thread has.
    std::fs::write(big_checkout.join("target/new.bin"), vec![2u8; 1 << 20]).unwrap();

    let items = mgr.disk_usage(Some(&fixture.workspace)).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].thread_id, big.id);
    assert_eq!(items[0].thread_title, "big");
    assert_eq!(items[0].repository_title, "app");
    assert_eq!(items[1].thread_id, small.id);
    assert!(items[0].private_bytes >= 1 << 20, "{:?}", items[0]);
    assert!(items[1].private_bytes < 256 * 1024, "{:?}", items[1]);
    // Both see the cloned build output in their apparent size.
    assert!(items[1].apparent_bytes >= 512 * 1024);
    assert!(items.iter().all(|item| item.last_active.is_some()));
    assert!(mgr.disk_usage(None).await.unwrap().len() >= 2);
}

#[tokio::test]
async fn archive_refuses_unsaved_work_then_removes_and_restores_on_unarchive() {
    let (fixture, db) = fixture().await;
    let fake = writing_fake();
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "work".into())
        .await
        .unwrap();
    run_prompt(&mgr, &thread.id, "write archived").await;
    let path = checkout(&mgr, &thread.id).await;
    std::fs::write(path.join("notes.txt"), "untracked\n").unwrap();

    // Unsaved work: refused without `force`, nothing changes.
    let refused = mgr
        .archive_thread(&thread.id, true, false)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(refused.len(), 1);
    assert!(refused[0].has_unsaved_work());
    assert_eq!(refused[0].changed_file_count, 2, "{:?}", refused[0]);
    assert!(path.exists());
    assert!(!mgr.db.is_thread_archived(&thread.id).await.unwrap());

    let (snapshot, removed) = mgr
        .archive_thread(&thread.id, true, true)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.archived);
    assert_eq!(removed.len(), 1);
    assert!(!path.exists());
    let listed = mgr.list_threads(&fixture.workspace).await.unwrap();
    assert!(
        listed
            .iter()
            .any(|listed| listed.id == thread.id && listed.archived)
    );
    let rows = mgr.thread_checkouts(&thread.id).await.unwrap();
    assert_eq!(rows[0].status, CheckoutStatus::Removed);
    assert!(rows[0].detail.as_deref().unwrap().contains("unarchive"));
    // History is kept; read-only views and turns fail until unarchived.
    assert!(
        !mgr.db
            .list_messages_by_thread(&thread.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        mgr.get_changes(&thread.id, Default::default())
            .await
            .is_err()
    );
    assert!(
        mgr.send_prompt(thread.id.clone(), "write again".into())
            .await
            .is_err()
    );
    assert!(mgr.disk_usage(None).await.unwrap().is_empty());

    let unarchived = mgr.unarchive_thread(&thread.id).await.unwrap();
    assert!(!unarchived.archived);
    assert!(path.exists());
    assert_eq!(
        std::fs::read_to_string(path.join("same.txt")).unwrap(),
        "archived\n"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("notes.txt")).unwrap(),
        "untracked\n"
    );
    let rows = mgr.thread_checkouts(&thread.id).await.unwrap();
    assert_eq!(rows[0].status, CheckoutStatus::Ready, "{:?}", rows[0]);
    run_prompt(&mgr, &thread.id, "write later").await;
    assert_eq!(
        std::fs::read_to_string(path.join("same.txt")).unwrap(),
        "later\n"
    );
}

#[tokio::test]
async fn deleted_checkouts_come_back_on_the_next_turn() {
    let (fixture, db) = fixture().await;
    let fake = writing_fake();
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "clean".into())
        .await
        .unwrap();
    let path = checkout(&mgr, &thread.id).await;
    let RemovalOutcome::Removed(removed) = mgr
        .delete_thread_checkouts(&thread.id, false)
        .await
        .unwrap()
    else {
        panic!("a clean checkout is removed without force");
    };
    assert_eq!(removed.len(), 1);
    assert!(!removed[0].has_unsaved_work());
    assert!(!path.exists());
    assert!(!mgr.db.is_thread_archived(&thread.id).await.unwrap());
    assert_eq!(
        mgr.thread_checkouts(&thread.id).await.unwrap()[0].status,
        CheckoutStatus::Removed
    );
    // A turn recreates it.
    run_prompt(&mgr, &thread.id, "write back").await;
    assert_eq!(
        std::fs::read_to_string(path.join("same.txt")).unwrap(),
        "back\n"
    );
    assert_eq!(
        mgr.thread_checkouts(&thread.id).await.unwrap()[0].status,
        CheckoutStatus::Ready
    );
}

#[tokio::test]
async fn removal_is_refused_while_a_turn_runs() {
    let (fixture, db) = fixture().await;
    let fake = FakeAdapter::new(scripted_events(), Duration::from_millis(400));
    let mgr = manager(&fixture, db, &fake);
    let thread = mgr
        .create_thread(fixture.workspace.clone(), "busy".into())
        .await
        .unwrap();
    let path = checkout(&mgr, &thread.id).await;
    mgr.send_prompt(thread.id.clone(), "hello".into())
        .await
        .unwrap();
    assert!(matches!(
        mgr.archive_thread(&thread.id, true, true).await,
        Err(DaemonError::Busy)
    ));
    assert!(matches!(
        mgr.delete_thread_checkouts(&thread.id, true).await,
        Err(DaemonError::Busy)
    ));
    assert!(path.exists());
}
