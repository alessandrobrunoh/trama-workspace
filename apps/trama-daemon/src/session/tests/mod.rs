use super::*;
use crate::error::DaemonError;
use crate::testing::FakeAdapter;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use trama_domain::agent::AgentEvent;
use trama_domain::ids::ThreadId;
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_protocol::DaemonMessage;

pub(super) async fn seed_workspace(
    db: &TramaDb,
    with_repo: bool,
    root: &std::path::Path,
) -> WorkspaceId {
    let workspace = Workspace::new("Default");
    let id = workspace.id.clone();
    db.create_workspace(&workspace).await.unwrap();
    if with_repo {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        test_git(&source, &["init", "-b", "main"]);
        std::fs::write(source.join("same.txt"), "source\n").unwrap();
        test_git(&source, &["add", "same.txt"]);
        test_git(&source, &["commit", "-m", "initial"]);
        let repo = Repository::new(None, Some(source), Some("main".into()));
        db.create_repository(&repo).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: id.clone(),
            repository_id: repo.id,
            alias: Some("repo".into()),
            default_enabled: true,
        })
        .await
        .unwrap();
    }
    id
}

async fn manager(with_repo: bool) -> (SessionManager, WorkspaceId, FakeAdapter, tempfile::TempDir) {
    let db = TramaDb::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ws = seed_workspace(&db, with_repo, dir.path()).await;
    let fake = FakeAdapter::scripted();
    let mgr =
        SessionManager::with_checkout_root(db, Arc::new(fake.clone()), dir.path().join("managed"));
    (mgr, ws, fake, dir)
}

fn test_git(cwd: &std::path::Path, args: &[&str]) {
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

fn test_repository(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    test_git(path, &["init", "-b", "main"]);
    std::fs::write(path.join("same.txt"), "source\n").unwrap();
    test_git(path, &["add", "same.txt"]);
    test_git(path, &["commit", "-m", "initial"]);
}

#[tokio::test]
async fn two_threads_receive_isolated_checkouts_for_every_default_repo() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let db = TramaDb::open(&db_path).unwrap();
    let workspace = Workspace::new("isolated");
    db.create_workspace(&workspace).await.unwrap();
    let sources = [dir.path().join("source-a"), dir.path().join("source-b")];
    for (name, source) in ["a", "b"].into_iter().zip(&sources) {
        test_repository(source);
        let repository = Repository::new(None, Some(source.clone()), Some("main".into()));
        db.create_repository(&repository).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: repository.id,
            alias: Some(name.into()),
            default_enabled: true,
        })
        .await
        .unwrap();
    }

    let fake = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(1));
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    let first = manager
        .create_thread(workspace.id.clone(), "first".into())
        .await
        .unwrap();
    let second = manager
        .create_thread(workspace.id.clone(), "second".into())
        .await
        .unwrap();
    let first_thread = db.get_thread(&first.id).await.unwrap().unwrap();
    assert_eq!(first_thread.repo_attachments.len(), 2);
    assert!(first_thread.repo_attachments.iter().all(|attachment| {
        attachment.checkout_id.is_some() && attachment.base_revision.is_some()
    }));
    let snapshots = manager.thread_checkouts(&first.id).await.unwrap();
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().all(|snapshot| {
        snapshot.status == trama_protocol::CheckoutStatus::Ready
            && snapshot.checkout_path.as_ref().is_some_and(|path| {
                path.starts_with(&snapshot.source_path.join(".trama/worktrees"))
            })
    }));

    let (tx1, mut rx1) = mpsc::unbounded_channel();
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    manager.subscribe(&first.id, tx1).await.unwrap();
    manager.subscribe(&second.id, tx2).await.unwrap();
    manager
        .send_prompt(first.id.clone(), "edit".into())
        .await
        .unwrap();
    manager
        .send_prompt(second.id.clone(), "edit".into())
        .await
        .unwrap();
    wait_turn_complete(&mut rx1).await;
    wait_turn_complete(&mut rx2).await;

    let configs = fake
        .start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(configs.len(), 2);
    assert_eq!(configs[0].extra_dirs.len(), 1);
    assert_eq!(configs[1].extra_dirs.len(), 1);
    let sources: Vec<_> = sources
        .iter()
        .map(|source| dunce::canonicalize(&source).unwrap())
        .collect();
    let first_paths = [&configs[0].cwd, &configs[0].extra_dirs[0]];
    let second_paths = [&configs[1].cwd, &configs[1].extra_dirs[0]];
    assert!(first_paths.iter().all(|path| {
        sources
            .iter()
            .any(|source| path.starts_with(source.join(".trama/worktrees")))
    }));
    assert!(second_paths.iter().all(|path| {
        sources
            .iter()
            .any(|source| path.starts_with(source.join(".trama/worktrees")))
    }));
    assert!(first_paths.iter().all(|path| !sources.contains(path)));
    assert!(second_paths.iter().all(|path| !sources.contains(path)));
    std::fs::write(first_paths[0].join("same.txt"), "first edit\n").unwrap();
    std::fs::write(first_paths[1].join("same.txt"), "first edit repo two\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(second_paths[0].join("same.txt")).unwrap(),
        "source\n"
    );
    assert_eq!(
        std::fs::read_to_string(second_paths[1].join("same.txt")).unwrap(),
        "source\n"
    );
    assert_eq!(
        std::fs::read_to_string(&sources[0].join("same.txt")).unwrap(),
        "source\n"
    );
    assert_eq!(
        std::fs::read_to_string(&sources[1].join("same.txt")).unwrap(),
        "source\n"
    );
    assert_ne!(first_paths[0], second_paths[0]);

    drop(manager);
    let reopened = TramaDb::open(&db_path).unwrap();
    let persisted = reopened.get_thread(&first.id).await.unwrap().unwrap();
    assert_eq!(
        persisted.repo_attachments.len(),
        first_thread.repo_attachments.len()
    );
    for (persisted, original) in persisted
        .repo_attachments
        .iter()
        .zip(&first_thread.repo_attachments)
    {
        assert_eq!(persisted.checkout_id, original.checkout_id);
        assert_eq!(persisted.base_revision, original.base_revision);
    }
    let after_restart = SessionManager::with_checkout_root(
        reopened,
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed"),
    );
    let resumed_config = after_restart.session_config(&first.id).await.unwrap();
    let expected_path = after_restart
        .db
        .checkout_location(
            &first.id,
            &first_thread.repo_attachments[0].repository_id,
            first_thread.repo_attachments[0]
                .checkout_id
                .as_ref()
                .unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed_config.cwd, expected_path);
}

#[tokio::test]
async fn provisioning_and_validation_fail_closed_before_adapter_start() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("failure");
    db.create_workspace(&workspace).await.unwrap();
    let invalid_source = dir.path().join("not-a-git-repo");
    std::fs::create_dir_all(&invalid_source).unwrap();
    let invalid_repo = Repository::new(None, Some(invalid_source), None);
    let invalid_repo_id = invalid_repo.id.clone();
    db.create_repository(&invalid_repo).await.unwrap();
    db.add_membership(&WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: invalid_repo_id.clone(),
        alias: None,
        default_enabled: true,
    })
    .await
    .unwrap();
    let fake = FakeAdapter::scripted();
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    assert!(
        manager
            .create_thread(workspace.id.clone(), "cannot clone".into())
            .await
            .is_err()
    );
    assert!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    db.upsert_membership(&WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: invalid_repo_id,
        alias: None,
        default_enabled: false,
    })
    .await
    .unwrap();

    let valid_source = dir.path().join("valid-repo");
    test_repository(&valid_source);
    let valid_repo = Repository::new(None, Some(valid_source.clone()), Some("main".into()));
    db.create_repository(&valid_repo).await.unwrap();
    db.add_membership(&WorkspaceMembership {
        workspace_id: workspace.id.clone(),
        repository_id: valid_repo.id.clone(),
        alias: Some("valid".into()),
        default_enabled: true,
    })
    .await
    .unwrap();
    // Deliberately model a historical global checkout. Losing its source
    // must not delete the local work kept outside that source repository.
    let mut valid_thread = Thread::new_for_workspace(
        &workspace,
        &db.list_memberships(&workspace.id).await.unwrap(),
        Some("valid".into()),
    );
    let checkout_id = CheckoutId::generate();
    let provisioned = manager
        .checkout_manager
        .provision(
            &valid_source,
            &valid_thread.id,
            &valid_repo.id,
            &checkout_id,
        )
        .unwrap();
    valid_thread.repo_attachments[0].checkout_id = Some(checkout_id);
    valid_thread.repo_attachments[0].base_revision = Some(provisioned.base_revision);
    db.create_thread(&valid_thread).await.unwrap();
    let attachment = db
        .get_thread(&valid_thread.id)
        .await
        .unwrap()
        .unwrap()
        .repo_attachments
        .remove(0);
    let checkout_path = manager
        .checkout_manager
        .path(
            &valid_thread.id,
            &attachment.repository_id,
            attachment.checkout_id.as_ref().unwrap(),
        )
        .unwrap();
    std::fs::write(checkout_path.join("user-work.txt"), "keep me\n").unwrap();
    let moved_source = dir.path().join("moved-valid-repo");
    std::fs::rename(&valid_source, &moved_source).unwrap();
    std::fs::remove_dir_all(&moved_source).unwrap();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout_path)
        .args(["remote", "set-url", "local", valid_source.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(manager.session_config(&valid_thread.id).await.is_err());
    assert_eq!(
        std::fs::read_to_string(checkout_path.join("user-work.txt")).unwrap(),
        "keep me\n"
    );
    assert!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn adopted_threads_are_rejected_before_start_with_migration_guidance() {
    use trama_domain::agent::AgentSession;
    use trama_domain::ids::AgentSessionId;
    use trama_domain::thread::{AttachMode, ThreadKind, ThreadRepositoryAttachment};

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("adopted-repo");
    test_repository(&source);
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("legacy-adopted");
    db.create_workspace(&workspace).await.unwrap();
    let repository = Repository::new(None, Some(source.clone()), Some("main".into()));
    db.create_repository(&repository).await.unwrap();
    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: ThreadKind::Top,
        title: Some("old adopted thread".into()),
        repo_attachments: vec![ThreadRepositoryAttachment {
            repository_id: repository.id.clone(),
            base_revision: None,
            checkout_id: None,
            attach_mode: AttachMode::Adopted,
        }],
    };
    db.create_thread(&thread).await.unwrap();
    db.create_agent_session(
        &thread.id,
        &AgentSession {
            id: AgentSessionId::generate(),
            external_session_id: Some("old-external-session".into()),
            model: None,
            cwd: source.clone(),
        },
        "claude_code",
    )
    .await
    .unwrap();

    let fake = FakeAdapter::scripted();
    let manager =
        SessionManager::with_checkout_root(db, Arc::new(fake.clone()), dir.path().join("managed"));
    let error = manager.ensure_session(&thread.id).await.unwrap_err();
    assert!(error.to_string().contains("adopted"));
    assert!(error.to_string().contains("migrate_legacy_thread"));
    assert!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

fn agent_event(msg: &DaemonMessage) -> Option<&AgentEvent> {
    match msg {
        DaemonMessage::AgentEventEnvelope { event, .. } => Some(event),
        _ => None,
    }
}

async fn wait_turn_complete(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) {
    let deadline = Duration::from_secs(10);
    loop {
        let msg = timeout(deadline, rx.recv())
            .await
            .expect("event")
            .expect("channel open");
        if matches!(
            msg,
            DaemonMessage::UsageSnapshot { .. }
                | DaemonMessage::AgentEventEnvelope {
                    event: AgentEvent::Failed { .. },
                    ..
                }
        ) {
            return;
        }
    }
}

#[tokio::test]
async fn create_and_list_threads() {
    let (mgr, ws, _, _dir) = manager(true).await;
    assert_eq!(mgr.list_workspaces().await.unwrap().len(), 1);
    let created = mgr.create_thread(ws.clone(), "First".into()).await.unwrap();
    assert_eq!(created.title, "First");
    let listed = mgr.list_threads(&ws).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
    let missing = WorkspaceId::from("nope");
    assert!(mgr.list_threads(&missing).await.is_err());
}

#[tokio::test]
async fn send_prompt_emits_scripted_events() {
    let (mgr, ws, _, _dir) = manager(true).await;
    let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "hi".into())
        .await
        .unwrap();

    let mut saw_delta = false;
    let mut saw_usage = false;
    let mut saw_turn = false;
    let mut saw_snapshot = false;
    let deadline = Duration::from_secs(10);
    while !(saw_delta && saw_usage && saw_turn && saw_snapshot) {
        let msg = timeout(deadline, rx.recv())
            .await
            .expect("event")
            .expect("channel open");
        match msg {
            DaemonMessage::MessageAppended { .. } => {}
            DaemonMessage::AgentEventEnvelope { event, .. } => match event {
                AgentEvent::TextDelta { .. } => saw_delta = true,
                AgentEvent::UsageReported { .. } => saw_usage = true,
                AgentEvent::TurnCompleted { .. } => saw_turn = true,
                _ => {}
            },
            DaemonMessage::UsageSnapshot { .. } => saw_snapshot = true,
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn interrupt_aborts_mock_turn() {
    let (mgr, ws, _, _dir) = manager(true).await;
    let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "hi".into())
        .await
        .unwrap();

    let _first = timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(mgr.interrupt(&thread.id).await.unwrap());

    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut rest = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        rest.push(msg);
    }
    let completed = rest
        .iter()
        .any(|m| matches!(agent_event(m), Some(AgentEvent::TurnCompleted { .. })));
    assert!(
        !completed,
        "interrupt should drop the mock task before TurnCompleted"
    );
}

#[tokio::test]
async fn subscribe_unknown_thread_fails() {
    let (mgr, _, _, _dir) = manager(false).await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let err = mgr
        .subscribe(&ThreadId::from("missing"), tx)
        .await
        .unwrap_err();
    assert!(matches!(err, DaemonError::NotFound(_)));
}

#[tokio::test]
async fn send_prompt_persists_messages_usage_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let db = TramaDb::open(&db_path).unwrap();
    let ws = seed_workspace(&db, true, dir.path()).await;
    let fake = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(5));
    let checkout_root = dir.path().join("managed");
    let mgr = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        checkout_root.clone(),
    );
    let thread = mgr.create_thread(ws, "persist".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "hello world".into())
        .await
        .unwrap();
    wait_turn_complete(&mut rx).await;

    let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "hello world");
    assert_eq!(messages[1].text(), "Hello from the mock agent.");
    let totals = db.usage_totals_for_thread(&thread.id).await.unwrap();
    assert_eq!(totals.input_tokens, 16);
    assert_eq!(totals.output_tokens, 8);
    let stored = db
        .latest_agent_session_for_thread(&thread.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.external_session_id.as_deref(), Some("fake-ext-1"));

    drop(mgr);
    let fake2 = FakeAdapter::new(crate::testing::scripted_events(), Duration::from_millis(5));
    let reopened = TramaDb::open(&db_path).unwrap();
    let mgr2 = SessionManager::with_checkout_root(reopened, Arc::new(fake2.clone()), checkout_root);
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    mgr2.subscribe(&thread.id, tx2).await.unwrap();
    let replayed: Vec<_> = {
        let mut out = Vec::new();
        while let Ok(msg) = rx2.try_recv() {
            out.push(msg);
        }
        out
    };
    let replayed_texts: Vec<_> = replayed
        .iter()
        .filter_map(|m| match m {
            DaemonMessage::MessageAppended { message, .. } => Some(message.text()),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed_texts,
        vec![
            "hello world".to_string(),
            "Hello from the mock agent.".to_string()
        ]
    );
    assert!(replayed.iter().any(|message| matches!(
        message,
        DaemonMessage::UsageSnapshot { totals, .. } if totals.input_tokens == 16
    )));

    mgr2.send_prompt(thread.id.clone(), "second".into())
        .await
        .unwrap();
    wait_turn_complete(&mut rx2).await;
    let configs = fake2
        .start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(configs.len(), 1);
    assert_eq!(
        configs[0].resume_external_session.as_deref(),
        Some("fake-ext-1")
    );
    let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2].text(), "second");
}

#[tokio::test]
async fn send_prompt_rejects_when_busy() {
    let (mgr, ws, _, _dir) = manager(true).await;
    let thread = mgr.create_thread(ws, "busy".into()).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "one".into())
        .await
        .unwrap();
    let err = mgr.send_prompt(thread.id, "two".into()).await.unwrap_err();
    assert!(matches!(err, DaemonError::Busy));
}

#[tokio::test]
async fn send_prompt_reports_missing_claude() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let ws = seed_workspace(&db, true, dir.path()).await;
    let mgr = SessionManager::with_checkout_root(
        db,
        Arc::new(FakeAdapter::fail_start()),
        dir.path().join("managed"),
    );
    let thread = mgr.create_thread(ws, "t".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id, "hi".into()).await.unwrap();
    let deadline = Duration::from_secs(10);
    let mut saw_error = false;
    let mut saw_persisted_failure = false;
    for _ in 0..8 {
        let msg = timeout(deadline, rx.recv())
            .await
            .expect("event")
            .expect("channel open");
        if let DaemonMessage::MessageAppended { message, .. } = &msg {
            if message.role == Role::System && message.text().contains("did not run") {
                assert!(
                    message.text().contains("CLAUDE_BIN") || message.text().contains("Claude"),
                    "{}",
                    message.text()
                );
                saw_persisted_failure = true;
            }
        }
        if let DaemonMessage::Error { code, message } = msg {
            assert_eq!(code, "agent");
            assert!(
                message.contains("CLAUDE_BIN") || message.contains("Claude"),
                "{message}"
            );
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "expected agent error envelope");
    assert!(
        saw_persisted_failure,
        "turn failure must be stored as a system message"
    );
}

#[tokio::test]
async fn subscribe_replays_history() {
    let (mgr, ws, _, _dir) = manager(true).await;
    let thread = mgr.create_thread(ws, "hist".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "hi".into())
        .await
        .unwrap();
    wait_turn_complete(&mut rx).await;

    let (tx2, mut rx2) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx2).await.unwrap();
    let first = timeout(Duration::from_secs(2), rx2.recv())
        .await
        .unwrap()
        .unwrap();
    let second = timeout(Duration::from_secs(2), rx2.recv())
        .await
        .unwrap()
        .unwrap();
    match (first, second) {
        (
            DaemonMessage::MessageAppended { message: a, .. },
            DaemonMessage::MessageAppended { message: b, .. },
        ) => {
            assert_eq!(a.text(), "hi");
            assert_eq!(b.text(), "Hello from the mock agent.");
        }
        other => panic!("expected two appended messages, got {other:?}"),
    }
}

async fn insert_legacy(
    db: &TramaDb,
    root: &std::path::Path,
    names: &[&str],
    mode: AttachMode,
) -> (WorkspaceId, Thread, Vec<std::path::PathBuf>) {
    let workspace = Workspace::new("legacy");
    db.create_workspace(&workspace).await.unwrap();
    let mut sources = Vec::new();
    let mut attachments = Vec::new();
    for name in names {
        let source = root.join(name);
        test_repository(&source);
        let repository = Repository::new(None, Some(source.clone()), Some("main".into()));
        db.create_repository(&repository).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: repository.id.clone(),
            alias: Some((*name).into()),
            default_enabled: true,
        })
        .await
        .unwrap();
        attachments.push(ThreadRepositoryAttachment {
            repository_id: repository.id,
            base_revision: None,
            checkout_id: None,
            attach_mode: mode,
        });
        sources.push(source);
    }
    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: trama_domain::thread::ThreadKind::Top,
        title: Some("legacy".into()),
        repo_attachments: attachments,
    };
    db.create_thread(&thread).await.unwrap();
    db.append_message(&Message::new_user(
        thread.id.clone(),
        "user",
        "please fix the bug",
    ))
    .await
    .unwrap();
    db.append_message(&Message::new_assistant(
        thread.id.clone(),
        trama_domain::ids::AgentSessionId::generate(),
        vec![ContentBlock::Text {
            text: "I patched it".into(),
        }],
    ))
    .await
    .unwrap();
    db.create_agent_session(
        &thread.id,
        &trama_domain::agent::AgentSession {
            id: trama_domain::ids::AgentSessionId::generate(),
            external_session_id: Some("old-claude-session".into()),
            model: Some("old-model".into()),
            cwd: sources[0].clone(),
        },
        "claude_code",
    )
    .await
    .unwrap();
    (workspace.id, thread, sources)
}

fn checkout_ids(thread: &Thread) -> Vec<Option<CheckoutId>> {
    thread
        .repo_attachments
        .iter()
        .map(|attachment| attachment.checkout_id.clone())
        .collect()
}

#[tokio::test]
async fn legacy_migration_retains_transcript_and_does_not_resume() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let db = TramaDb::open(&db_path).unwrap();
    let (_workspace, thread, sources) =
        insert_legacy(&db, dir.path(), &["app", "web"], AttachMode::Isolated).await;
    let fake = FakeAdapter::scripted();
    let managed = dir.path().join("managed");
    let manager =
        SessionManager::with_checkout_root(db.clone(), Arc::new(fake.clone()), managed.clone());

    let rejected = manager
        .send_prompt(thread.id.clone(), "should not run".into())
        .await
        .unwrap_err();
    assert!(rejected.to_string().contains("migrate_legacy_thread"));
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        2
    );
    assert!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );

    let migrated = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap();
    assert_eq!(migrated.id, thread.id);
    let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
    let ids = checkout_ids(&stored);
    assert!(ids.iter().all(|id| id.is_some()));
    assert!(stored.repo_attachments.iter().all(|attachment| {
        attachment.attach_mode == AttachMode::Isolated && attachment.base_revision.is_some()
    }));
    let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages[0].text(), "please fix the bug");
    assert_eq!(messages[1].text(), "I patched it");
    assert_eq!(messages[2].role, Role::System);
    assert!(messages[2].text().contains("transcript is retained"));
    assert!(messages[2].text().contains("fresh"));
    for source in &sources {
        assert_eq!(
            std::fs::read_to_string(source.join("same.txt")).unwrap(),
            "source\n"
        );
    }

    let again = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap();
    assert_eq!(again.id, thread.id);
    assert_eq!(
        checkout_ids(&db.get_thread(&thread.id).await.unwrap().unwrap()),
        ids
    );
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        3
    );

    drop(manager);
    drop(db);
    let reopened = TramaDb::open(&db_path).unwrap();
    let persisted = reopened.get_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(checkout_ids(&persisted), ids);
    assert_eq!(
        reopened
            .list_messages_by_thread(&thread.id)
            .await
            .unwrap()
            .len(),
        3
    );
    let fake2 = FakeAdapter::scripted();
    let manager2 = SessionManager::with_checkout_root(
        reopened.clone(),
        Arc::new(fake2.clone()),
        managed.clone(),
    );
    manager2
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .list_messages_by_thread(&thread.id)
            .await
            .unwrap()
            .len(),
        3
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager2.subscribe(&thread.id, tx).await.unwrap();
    manager2
        .send_prompt(thread.id.clone(), "continue".into())
        .await
        .unwrap();
    wait_turn_complete(&mut rx).await;
    let configs = fake2
        .start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(configs.len(), 1);
    assert!(configs[0].resume_external_session.is_none());
    assert!(sources.iter().any(|source| {
        configs[0].cwd.starts_with(
            dunce::canonicalize(&source)
                .unwrap()
                .join(".trama/worktrees"),
        )
    }));
    assert_eq!(configs[0].extra_dirs.len(), 1);
    assert!(sources.iter().any(|source| {
        configs[0].extra_dirs[0].starts_with(
            dunce::canonicalize(&source)
                .unwrap()
                .join(".trama/worktrees"),
        )
    }));
    assert!(
        sources
            .iter()
            .all(|source| { &configs[0].cwd != source && &configs[0].extra_dirs[0] != source })
    );
}

#[tokio::test]
async fn migration_failure_leaves_attachments_and_does_not_start_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let (_workspace, thread, sources) =
        insert_legacy(&db, dir.path(), &["one", "two"], AttachMode::Isolated).await;
    let fake = FakeAdapter::scripted();
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    std::fs::write(sources[1].join("dirty.txt"), "wip\n").unwrap();
    let dirty = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert!(dirty.to_string().contains("commit or stash"), "{dirty}");
    assert!(
        db.get_thread(&thread.id)
            .await
            .unwrap()
            .unwrap()
            .repo_attachments
            .iter()
            .all(|attachment| attachment.checkout_id.is_none())
    );
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(sources[1].join("dirty.txt")).unwrap(),
        "wip\n"
    );

    std::fs::remove_file(sources[1].join("dirty.txt")).unwrap();
    let blocked = sources[1].join(".trama");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(
        blocked.join("foreign.txt"),
        "foreign content: do not replace\n",
    )
    .unwrap();
    std::fs::write(sources[1].join(".git/info/exclude"), "/.trama/\n").unwrap();
    let failed = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert!(failed.to_string().contains("unchanged"), "{failed}");
    assert!(
        db.get_thread(&thread.id)
            .await
            .unwrap()
            .unwrap()
            .repo_attachments
            .iter()
            .all(|attachment| {
                attachment.checkout_id.is_none() && attachment.base_revision.is_none()
            })
    );
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        2
    );
    assert!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(sources[0].join("same.txt")).unwrap(),
        "source\n"
    );

    std::fs::remove_dir_all(&sources[0]).unwrap();
    std::fs::remove_dir_all(blocked).unwrap();
    let missing = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert!(
        missing.to_string().contains("missing") || missing.to_string().contains("does not exist"),
        "{missing}"
    );
    assert!(
        db.get_thread(&thread.id)
            .await
            .unwrap()
            .unwrap()
            .repo_attachments
            .iter()
            .all(|attachment| attachment.checkout_id.is_none())
    );
}

#[tokio::test]
async fn busy_or_live_thread_is_not_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let (_workspace, thread, _sources) =
        insert_legacy(&db, dir.path(), &["app"], AttachMode::Isolated).await;
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed"),
    );
    {
        let mut inner = manager.lock();
        inner
            .threads
            .entry(thread.id.clone())
            .or_default()
            .reservation = Some(Reservation::Turn);
    }
    let err = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "busy");
    {
        let mut inner = manager.lock();
        let runtime = inner.threads.get_mut(&thread.id).unwrap();
        runtime.reservation = None;
        runtime.live_session = Some(trama_domain::agent::AgentSession {
            id: trama_domain::ids::AgentSessionId::generate(),
            external_session_id: Some("live".into()),
            model: None,
            cwd: dir.path().join("app"),
        });
    }
    let err = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "busy");
    assert!(
        db.get_thread(&thread.id)
            .await
            .unwrap()
            .unwrap()
            .repo_attachments[0]
            .checkout_id
            .is_none()
    );
}

#[tokio::test]
async fn adopted_legacy_thread_migrates_to_isolated_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let (_workspace, thread, sources) =
        insert_legacy(&db, dir.path(), &["adopted"], AttachMode::Adopted).await;
    let fake = FakeAdapter::scripted();
    let managed = dir.path().join("managed");
    let manager =
        SessionManager::with_checkout_root(db.clone(), Arc::new(fake.clone()), managed.clone());
    manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap();
    let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(stored.repo_attachments[0].attach_mode, AttachMode::Isolated);
    assert!(stored.repo_attachments[0].checkout_id.is_some());
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager.subscribe(&thread.id, tx).await.unwrap();
    manager.send_prompt(thread.id, "go".into()).await.unwrap();
    wait_turn_complete(&mut rx).await;
    let configs = fake
        .start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(configs.len(), 1);
    assert!(configs[0].resume_external_session.is_none());
    assert!(
        configs[0].cwd.starts_with(
            dunce::canonicalize(&sources[0])
                .unwrap()
                .join(".trama/worktrees")
        )
    );
    assert_ne!(configs[0].cwd, sources[0]);
    assert_eq!(
        std::fs::read_to_string(sources[0].join("same.txt")).unwrap(),
        "source\n"
    );
}

#[tokio::test]
async fn create_workspace_and_add_repository_round_trip_without_config() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open(&dir.path().join("trama.db")).unwrap();
    crate::config::seed_from_config(&db, &crate::config::FileConfig::default())
        .await
        .unwrap();
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed"),
    );
    assert!(manager.create_workspace("  ".into()).await.is_err());
    let created = manager.create_workspace("Demo".into()).await.unwrap();
    assert_eq!(created.repo_count, 0);
    assert!(manager.create_workspace("Demo".into()).await.is_err());
    let empty = manager
        .create_thread(created.id.clone(), "no repos".into())
        .await
        .unwrap_err();
    assert!(empty.to_string().contains("Add a local Git repository"));
    assert!(manager.list_threads(&created.id).await.unwrap().is_empty());

    let source = dir.path().join("app");
    test_repository(&source);
    let child = source.join("src");
    std::fs::create_dir_all(&child).unwrap();
    assert!(
        manager
            .add_repository(created.id.clone(), child, "child".into())
            .await
            .is_err()
    );
    assert!(
        manager
            .add_repository(
                created.id.clone(),
                dir.path().to_path_buf(),
                "parent".into()
            )
            .await
            .is_err()
    );
    assert!(
        manager
            .add_repository(
                created.id.clone(),
                dir.path().join("missing"),
                "missing".into(),
            )
            .await
            .is_err()
    );
    let added = manager
        .add_repository(created.id.clone(), source.clone(), "app".into())
        .await
        .unwrap();
    assert!(added.default_enabled);
    assert_eq!(added.title, "app");
    let again = manager
        .add_repository(created.id.clone(), source, "app".into())
        .await
        .unwrap();
    assert_eq!(again.id, added.id);
    let listed = manager.list_repositories(&created.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, added.id);
    let workspaces = manager.list_workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].repo_count, 1);

    crate::config::seed_from_config(&db, &crate::config::FileConfig::default())
        .await
        .unwrap();
    assert_eq!(db.list_workspaces().await.unwrap().len(), 1);
    assert_eq!(db.list_memberships(&created.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn concurrent_migrations_are_serialized_one_notice_no_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let (_workspace, thread, _sources) =
        insert_legacy(&db, dir.path(), &["app"], AttachMode::Isolated).await;
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed"),
    );

    let (first, second) = tokio::join!(
        manager.migrate_legacy_thread(thread.id.clone()),
        manager.migrate_legacy_thread(thread.id.clone())
    );
    let results = [first, second];
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one migration may run: {:?}",
        results
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(err) if err.code() == "busy"))
            .count(),
        1,
        "the losing migration must be rejected as busy"
    );

    // One transcript notice, one set of published checkouts.
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        3
    );
    let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
    let ids = checkout_ids(&stored);
    assert!(ids.iter().all(|id| id.is_some()));

    // A later migration validates and keeps the same targets.
    manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap();
    assert_eq!(
        checkout_ids(&db.get_thread(&thread.id).await.unwrap().unwrap()),
        ids
    );
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        3
    );
}

#[tokio::test]
async fn send_prompt_is_rejected_for_the_whole_migration_window() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let (_workspace, thread, _sources) =
        insert_legacy(&db, dir.path(), &["app"], AttachMode::Isolated).await;
    let fake = FakeAdapter::scripted();
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    // Simulate a migration that has reserved the thread and is inside its
    // clone/persist window.
    {
        let mut inner = manager.lock();
        inner
            .threads
            .entry(thread.id.clone())
            .or_default()
            .reservation = Some(Reservation::Migration);
    }
    let err = manager
        .send_prompt(thread.id.clone(), "must not run".into())
        .await
        .unwrap_err();
    assert!(matches!(err, DaemonError::Busy), "{err}");
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        2
    );
    assert!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn migration_is_rejected_while_a_turn_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let ws = seed_workspace(&db, true, dir.path()).await;
    let fake = FakeAdapter::new(
        crate::testing::scripted_events(),
        Duration::from_millis(150),
    );
    let mgr =
        SessionManager::with_checkout_root(db, Arc::new(fake.clone()), dir.path().join("managed"));
    let thread = mgr.create_thread(ws, "in flight".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    // send_prompt returns once the turn is reserved and spawned; the
    // scripted delay keeps it running well past the migration attempt.
    mgr.send_prompt(thread.id.clone(), "long turn".into())
        .await
        .unwrap();
    let err = mgr
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "busy");
    // The in-flight turn still completes on its original checkouts.
    wait_turn_complete(&mut rx).await;
    assert_eq!(
        fake.start_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
}

#[tokio::test]
async fn terminal_failed_event_persists_failure_with_partial_output_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let db = TramaDb::open(&db_path).unwrap();
    let ws = seed_workspace(&db, true, dir.path()).await;
    let failing = vec![
        AgentEvent::SessionStarted {
            external_session_id: Some("fake-ext-f1".into()),
            model: Some("fake".into()),
        },
        AgentEvent::TextDelta {
            text: "partial output before the crash".into(),
        },
        AgentEvent::Failed {
            message: "model overloaded".into(),
        },
    ];
    let fake = FakeAdapter::new(failing, Duration::from_millis(5));
    let managed = dir.path().join("managed");
    let mgr =
        SessionManager::with_checkout_root(db.clone(), Arc::new(fake.clone()), managed.clone());
    let thread = mgr.create_thread(ws, "fails".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "hello".into())
        .await
        .unwrap();

    let deadline = Duration::from_secs(10);
    let mut notice = None;
    while notice.is_none() {
        let msg = timeout(deadline, rx.recv())
            .await
            .expect("event")
            .expect("channel open");
        if let DaemonMessage::MessageAppended { message, .. } = &msg {
            if message.role == Role::System {
                notice = Some(message.text());
            }
        }
    }
    let notice = notice.unwrap();
    assert!(notice.contains("turn failed"), "{notice}");
    assert!(notice.contains("model overloaded"), "{notice}");
    assert!(
        notice.contains("partial output before the crash"),
        "{notice}"
    );

    let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text(), "hello");
    assert_eq!(messages[1].text(), notice);

    // The thread is free again after the failure.
    {
        let inner = mgr.lock();
        assert_eq!(
            inner.threads.get(&thread.id).unwrap().reservation,
            None,
            "failure must release the turn reservation"
        );
    }

    // Replay after a restart still shows the error.
    drop(mgr);
    drop(db);
    let reopened = TramaDb::open(&db_path).unwrap();
    let mgr2 = SessionManager::with_checkout_root(
        reopened.clone(),
        Arc::new(FakeAdapter::scripted()),
        managed,
    );
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    mgr2.subscribe(&thread.id, tx2).await.unwrap();
    let mut replayed_failure = false;
    while let Ok(msg) = rx2.try_recv() {
        if let DaemonMessage::MessageAppended { message, .. } = msg {
            if message.text().contains("model overloaded") {
                replayed_failure = true;
            }
        }
    }
    assert!(replayed_failure, "failure notice must replay after restart");
}

#[tokio::test]
async fn failed_event_after_completed_turn_appends_nothing() {
    let (mgr, ws, _, _dir) = manager(true).await;
    let thread = mgr.create_thread(ws, "late failure".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "ok turn".into())
        .await
        .unwrap();
    wait_turn_complete(&mut rx).await;
    // The turn completed and released the thread; a duplicate terminal
    // failure for the same turn must not persist anything.
    mgr.on_agent_event(
        &thread.id,
        AgentEvent::Failed {
            message: "late failure after success".into(),
        },
    )
    .await;
    let messages = mgr.db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages.iter().all(|message| message.role != Role::System),
        "no failure notice may follow a completed turn"
    );
}

#[tokio::test]
async fn unexpected_pump_closure_persists_interruption_and_frees_thread() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let ws = seed_workspace(&db, true, dir.path()).await;
    // Deltas only: the stream never reaches a terminal event.
    let hanging = vec![
        AgentEvent::SessionStarted {
            external_session_id: None,
            model: Some("fake".into()),
        },
        AgentEvent::TextDelta {
            text: "work in progress ".into(),
        },
    ];
    let fake = FakeAdapter::new(hanging, Duration::from_millis(5));
    let mgr = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    let thread = mgr.create_thread(ws, "hangs".into()).await.unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    mgr.subscribe(&thread.id, tx).await.unwrap();
    mgr.send_prompt(thread.id.clone(), "hello".into())
        .await
        .unwrap();

    let deadline = Duration::from_secs(10);
    loop {
        let msg = timeout(deadline, rx.recv())
            .await
            .expect("event")
            .expect("channel open");
        if matches!(
            &msg,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TextDelta { .. },
                ..
            }
        ) {
            break;
        }
    }
    // The adapter process exits without a terminal event.
    fake.close_all_event_streams();

    let mut notice = None;
    while notice.is_none() {
        let msg = timeout(deadline, rx.recv())
            .await
            .expect("event")
            .expect("channel open");
        if let DaemonMessage::MessageAppended { message, .. } = &msg {
            if message.role == Role::System {
                notice = Some(message.text());
            }
        }
    }
    let notice = notice.unwrap();
    assert!(notice.contains("ended unexpectedly"), "{notice}");
    assert!(notice.contains("work in progress "), "{notice}");

    let messages = db.list_messages_by_thread(&thread.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].text(), notice);
    // Busy was cleared: a new prompt is accepted rather than Busy.
    mgr.send_prompt(thread.id.clone(), "retry".into())
        .await
        .unwrap();
}

#[tokio::test]
async fn mixed_migration_keeps_managed_edits_and_invalidates_resume() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("mixed");
    db.create_workspace(&workspace).await.unwrap();
    // Explicit ids so the persisted attachment order (repository id
    // ascending) is deterministic: alpha is the cwd.
    let alpha_id = trama_domain::ids::RepositoryId::from("repo-alpha");
    let beta_id = trama_domain::ids::RepositoryId::from("repo-beta");
    let sources = [dir.path().join("alpha"), dir.path().join("beta")];
    for source in &sources {
        test_repository(source);
    }
    let thread_id = ThreadId::generate();
    let checkout_alpha = CheckoutId::generate();
    let provisioner = trama_infra::git::checkout::CheckoutManager::new(dir.path().join("managed"));
    let managed_checkout = provisioner
        .provision(&sources[0], &thread_id, &alpha_id, &checkout_alpha)
        .unwrap();
    let thread = Thread {
        id: thread_id,
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: trama_domain::thread::ThreadKind::Top,
        title: Some("mixed".into()),
        repo_attachments: vec![
            ThreadRepositoryAttachment {
                repository_id: alpha_id.clone(),
                base_revision: Some(managed_checkout.base_revision.clone()),
                checkout_id: Some(checkout_alpha.clone()),
                attach_mode: AttachMode::Isolated,
            },
            ThreadRepositoryAttachment {
                repository_id: beta_id.clone(),
                base_revision: None,
                checkout_id: None,
                attach_mode: AttachMode::Adopted,
            },
        ],
    };
    for (repository_id, source) in [(&alpha_id, &sources[0]), (&beta_id, &sources[1])] {
        let repository = Repository {
            id: repository_id.clone(),
            canonical_remote: None,
            local_path: Some(source.clone()),
            default_branch: Some("main".into()),
        };
        db.create_repository(&repository).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: repository_id.clone(),
            alias: None,
            default_enabled: true,
        })
        .await
        .unwrap();
    }
    db.create_thread(&thread).await.unwrap();
    // The managed checkout already carries user work that must survive.
    std::fs::write(
        managed_checkout.path.join("same.txt"),
        "edited in managed checkout\n",
    )
    .unwrap();
    // A pre-migration external session whose cwd matches the unchanged
    // managed checkout: without invalidation it would be resumed.
    db.create_agent_session(
        &thread.id,
        &trama_domain::agent::AgentSession {
            id: trama_domain::ids::AgentSessionId::generate(),
            external_session_id: Some("pre-migration-session".into()),
            model: Some("old-model".into()),
            cwd: managed_checkout.path.clone(),
        },
        "claude_code",
    )
    .await
    .unwrap();

    let fake = FakeAdapter::scripted();
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap();

    let stored = db.get_thread(&thread.id).await.unwrap().unwrap();
    assert_eq!(stored.repo_attachments.len(), 2);
    assert!(stored.repo_attachments.iter().all(|attachment| {
        attachment.attach_mode == AttachMode::Isolated && attachment.checkout_id.is_some()
    }));
    assert_eq!(stored.repo_attachments[0].repository_id, alpha_id);
    assert_eq!(
        stored.repo_attachments[0].checkout_id,
        Some(checkout_alpha.clone()),
        "the managed target must not be replaced"
    );
    // Managed edit retained in place.
    assert_eq!(
        std::fs::read_to_string(managed_checkout.path.join("same.txt")).unwrap(),
        "edited in managed checkout\n"
    );
    let config = manager.session_config(&thread.id).await.unwrap();
    assert_eq!(config.cwd, managed_checkout.path);
    assert!(
        config.resume_external_session.is_none(),
        "migration must invalidate the pre-migration external session"
    );
    assert_eq!(config.extra_dirs.len(), 1);
    assert_ne!(config.extra_dirs[0], sources[1]);
    assert_eq!(
        std::fs::read_to_string(sources[0].join("same.txt")).unwrap(),
        "source\n"
    );
    assert_eq!(
        std::fs::read_to_string(sources[1].join("same.txt")).unwrap(),
        "source\n"
    );
}

#[tokio::test]
async fn migration_without_attachments_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("empty");
    db.create_workspace(&workspace).await.unwrap();
    let thread = Thread {
        id: ThreadId::generate(),
        workspace_id: Some(workspace.id.clone()),
        parent_thread_id: None,
        kind: trama_domain::thread::ThreadKind::Top,
        title: Some("no repos".into()),
        repo_attachments: vec![],
    };
    db.create_thread(&thread).await.unwrap();
    let manager = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed"),
    );
    let err = manager
        .migrate_legacy_thread(thread.id.clone())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no repository attachments"),
        "{err}"
    );
    assert_eq!(
        db.list_messages_by_thread(&thread.id).await.unwrap().len(),
        0
    );
}

#[tokio::test]
async fn concurrent_workspace_creation_yields_one_row_per_name() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let db_a = TramaDb::open(&db_path).unwrap();
    let db_b = TramaDb::open(&db_path).unwrap();
    let manager_a = SessionManager::with_checkout_root(
        db_a.clone(),
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed-a"),
    );
    let manager_b = SessionManager::with_checkout_root(
        db_b.clone(),
        Arc::new(FakeAdapter::scripted()),
        dir.path().join("managed-b"),
    );
    let (a, b, c, d) = tokio::join!(
        manager_a.create_workspace("Race".into()),
        manager_b.create_workspace("Race".into()),
        manager_a.create_workspace("Race".into()),
        manager_b.create_workspace("Race".into())
    );
    let outcomes = [&a, &b, &c, &d];
    assert_eq!(
        outcomes.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one creator may win: {a:?} {b:?} {c:?} {d:?}"
    );
    let names: Vec<_> = db_a
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|workspace| workspace.name)
        .collect();
    assert_eq!(names.iter().filter(|name| *name == "Race").count(), 1);
    assert_eq!(db_b.list_workspaces().await.unwrap().len(), 1);
}
