//! Project setup (`.agents/prepare`, `.agents/linked`) runs in new thread
//! checkouts and subthread copies; failures surface in `thread_checkouts`.

use super::*;
use crate::testing::{FakeAdapter, scripted_events};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use trama_protocol::{CheckoutStatus, SubthreadKind};

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Trama Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Trama Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A workspace with one repository (alias `app`) whose committed
/// `.agents/prepare` is `script`.
async fn fixture(script: &str) -> (SessionManager, WorkspaceId, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open(&dir.path().join("trama.db")).unwrap();
    let workspace = Workspace::new("setup");
    db.create_workspace(&workspace).await.unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir_all(source.join(".agents")).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    std::fs::write(source.join("same.txt"), "one\n").unwrap();
    let prepare = source.join(".agents/prepare");
    std::fs::write(&prepare, script).unwrap();
    std::fs::set_permissions(&prepare, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "initial"]);
    let repo = Repository::new(None, Some(source.clone()), Some("main".into()));
    db.create_repository(&repo).await.unwrap();
    db.add_membership(&WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: repo.id,
        alias: Some("app".into()),
        default_enabled: true,
    })
    .await
    .unwrap();
    let fake = FakeAdapter::new(scripted_events(), Duration::from_millis(1));
    let manager =
        SessionManager::with_checkout_root(db, Arc::new(fake), dir.path().join("managed"));
    (manager, workspace.id, source.canonicalize().unwrap(), dir)
}

#[tokio::test]
async fn prepare_runs_in_new_thread_checkouts_and_subthread_copies() {
    let (manager, workspace, source, _dir) = fixture(
        "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$TRAMA_CHECKOUT_PATH\" \"$TRAMA_THREAD_TITLE\" \"$TRAMA_REPOSITORY_ALIAS\" > prepared.txt\n",
    )
    .await;
    let thread = manager
        .create_thread(workspace, "feature".into())
        .await
        .unwrap();
    let rows = manager.thread_checkouts(&thread.id).await.unwrap();
    assert_eq!(rows[0].status, CheckoutStatus::Ready);
    assert_eq!(rows[0].detail, None);
    let checkout = rows[0].checkout_path.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(checkout.join("prepared.txt")).unwrap(),
        format!("{}|feature|app\n", checkout.display())
    );
    assert!(!source.join("prepared.txt").exists());

    let sub = manager
        .create_subthread(
            thread.id.clone(),
            SubthreadKind::Plain,
            Some("child".into()),
            None,
        )
        .await
        .unwrap();
    let rows = manager.thread_checkouts(&sub.id).await.unwrap();
    assert_eq!(rows[0].status, CheckoutStatus::Ready);
    let copy = rows[0].checkout_path.clone().unwrap();
    assert_ne!(copy, checkout);
    assert_eq!(
        std::fs::read_to_string(copy.join("prepared.txt")).unwrap(),
        format!("{}|child|app\n", copy.display())
    );
}

#[tokio::test]
async fn failed_prepare_keeps_the_checkout_and_reports_the_output_tail() {
    let (manager, workspace, source, _dir) =
        fixture("#!/bin/sh\necho 'npm ERR! lockfile mismatch' >&2\nexit 2\n").await;
    let thread = manager
        .create_thread(workspace, "feature".into())
        .await
        .unwrap();
    let rows = manager.thread_checkouts(&thread.id).await.unwrap();
    assert_eq!(rows[0].status, CheckoutStatus::Ready);
    let checkout = rows[0].checkout_path.clone().unwrap();
    assert!(checkout.join("same.txt").exists());
    assert_ne!(checkout, source);
    let detail = rows[0].detail.clone().unwrap();
    assert!(detail.starts_with("prepare failed: "), "{detail}");
    assert!(detail.contains("exited with status 2"), "{detail}");
    assert!(detail.contains("npm ERR! lockfile mismatch"), "{detail}");
}
