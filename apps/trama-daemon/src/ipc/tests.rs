use super::*;
use crate::session::SessionManager;
use crate::testing::FakeAdapter;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};
use tokio::sync::{mpsc, watch};
use tokio::time::{Duration, timeout};
use trama_db::TramaDb;
use trama_domain::agent::AgentEvent;
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

async fn test_sessions() -> SessionManager {
    let db = TramaDb::open_in_memory().unwrap();
    let workspace = Workspace::new("Default");
    db.create_workspace(&workspace).await.unwrap();
    let source = std::env::temp_dir().join(format!(
        "trama-connection-test-{}",
        trama_domain::ids::RepositoryId::generate()
    ));
    std::fs::create_dir_all(&source).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-b", "main"]);
    std::fs::write(source.join("same.txt"), "source\n").unwrap();
    git(&["add", "same.txt"]);
    git(&["commit", "-m", "initial"]);
    let repo = Repository::new(None, Some(source), Some("main".into()));
    db.create_repository(&repo).await.unwrap();
    db.add_membership(&WorkspaceMembership {
        workspace_id: workspace.id,
        repository_id: repo.id,
        alias: Some("repo".into()),
        default_enabled: true,
    })
    .await
    .unwrap();
    SessionManager::with_checkout_root(
        db,
        Arc::new(FakeAdapter::scripted()),
        std::env::temp_dir().join(format!(
            "trama-connection-managed-{}",
            trama_domain::ids::ThreadId::generate()
        )),
    )
}

async fn write_client<W: AsyncWrite + Unpin>(writer: &mut W, msg: &ClientMessage) {
    let bytes = encode_json_line(msg).unwrap();
    writer.write_all(&bytes).await.unwrap();
    writer.flush().await.unwrap();
}

async fn read_daemon<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> DaemonMessage {
    let mut line = String::new();
    timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .expect("read timeout")
        .unwrap();
    decode_json_line(&line).unwrap()
}

#[tokio::test]
async fn rejected_prompt_ends_only_the_requesting_clients_turn() {
    let root = tempfile::tempdir().unwrap();
    let sessions = SessionManager::with_checkout_root(
        TramaDb::open_in_memory().unwrap(),
        Arc::new(FakeAdapter::scripted()),
        root.path(),
    );
    let thread_id = ThreadId::from("missing-thread");
    let line = serde_json::to_string(&ClientMessage::SendPrompt {
        thread_id: thread_id.clone(),
        text: "keep this draft".into(),
        mentions: Vec::new(),
        terminals: Vec::new(),
    })
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (shutdown, _) = watch::channel(false);
    let collab = CollabService::disabled(sessions.clone());
    let result = handle_line(
        &line,
        &sessions,
        &collab,
        &tx,
        &mut HashMap::new(),
        &shutdown,
    )
    .await;
    assert!(
        result.is_ok(),
        "a rejected prompt must keep the connection usable"
    );
    assert!(matches!(
        rx.try_recv().unwrap(),
        DaemonMessage::Error { .. }
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        DaemonMessage::AgentEventEnvelope {
            thread_id: id,
            event: AgentEvent::Failed { .. }
        } if id == thread_id
    ));
}

#[tokio::test]
async fn comment_commands_reply_once_whether_or_not_subscribed() {
    use trama_domain::comment::{CodeAnchor, CommentTarget};
    let sessions = test_sessions().await;
    let workspace_id = sessions.default_workspace_id().await.unwrap();
    let thread = sessions
        .create_thread(workspace_id, "comments".into())
        .await
        .unwrap();
    let checkouts = sessions.thread_checkouts(&thread.id).await.unwrap();
    let target = CommentTarget::Code {
        anchor: CodeAnchor {
            repository_id: checkouts[0].repository_id.clone(),
            path: "same.txt".into(),
            revision: 0,
            start_line: 1,
            end_line: 1,
            text_hash: None,
            context_hash: None,
        },
    };
    let create = serde_json::to_string(&ClientMessage::CreateComment {
        thread_id: thread.id.clone(),
        target,
        body: "why?".into(),
        parent_comment_id: None,
    })
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (shutdown, _) = watch::channel(false);
    let mut subscriptions = HashMap::new();
    let collab = CollabService::disabled(sessions.clone());

    // Not subscribed: the requester gets the reply directly.
    handle_line(
        &create,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    let first = match rx.try_recv().unwrap() {
        DaemonMessage::CommentCreated(snapshot) => snapshot,
        other => panic!("expected comment_created, got {other:?}"),
    };
    assert_eq!(first.excerpt.as_deref(), Some("source"));
    assert!(rx.try_recv().is_err());

    // Subscribed: exactly one copy, through the broadcast.
    let subscribe = serde_json::to_string(&ClientMessage::SubscribeThread {
        thread_id: thread.id.clone(),
    })
    .unwrap();
    handle_line(
        &subscribe,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    while rx.try_recv().is_ok() {}
    handle_line(
        &create,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    let second = match rx.try_recv().unwrap() {
        DaemonMessage::CommentCreated(snapshot) => snapshot,
        other => panic!("expected comment_created, got {other:?}"),
    };
    assert!(rx.try_recv().is_err());

    let delete = serde_json::to_string(&ClientMessage::DeleteComment {
        comment_id: second.id.clone(),
    })
    .unwrap();
    handle_line(
        &delete,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    assert!(matches!(
        rx.try_recv().unwrap(),
        DaemonMessage::CommentDeleted { comment_ids, .. } if comment_ids == vec![second.id.clone()]
    ));
    assert!(rx.try_recv().is_err());

    let list = serde_json::to_string(&ClientMessage::ListComments {
        thread_id: thread.id.clone(),
    })
    .unwrap();
    handle_line(
        &list,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    match rx.try_recv().unwrap() {
        DaemonMessage::Comments { comments, .. } => {
            assert_eq!(comments.len(), 1);
            assert_eq!(comments[0].id, first.id);
        }
        other => panic!("expected comments, got {other:?}"),
    }

    // Resolved comments are not delivered, so nothing is left to send.
    let resolve = serde_json::to_string(&ClientMessage::ResolveComment {
        comment_id: first.id.clone(),
    })
    .unwrap();
    handle_line(
        &resolve,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    assert!(matches!(
        rx.try_recv().unwrap(),
        DaemonMessage::CommentUpdated(snapshot) if snapshot.id == first.id
    ));
    let send = serde_json::to_string(&ClientMessage::SendComments {
        thread_id: thread.id.clone(),
    })
    .unwrap();
    handle_line(
        &send,
        &sessions,
        &collab,
        &tx,
        &mut subscriptions,
        &shutdown,
    )
    .await
    .unwrap();
    assert!(matches!(
        rx.try_recv().unwrap(),
        DaemonMessage::Error { code, .. } if code == "invalid"
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        DaemonMessage::AgentEventEnvelope {
            event: AgentEvent::Failed { .. },
            ..
        }
    ));
}

#[tokio::test]
async fn composer_requests_over_duplex() {
    let (client, server) = duplex(64 * 1024);
    let sessions = test_sessions().await;
    let (shutdown, _rx) = watch::channel(false);
    tokio::spawn(async move {
        let collab = CollabService::disabled(sessions.clone());
        let _ = handle_connection(server, sessions, collab, shutdown).await;
    });
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);
    write_client(
        &mut client_write,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test".into(),
        },
    )
    .await;
    assert!(matches!(
        read_daemon(&mut reader).await,
        DaemonMessage::Welcome { accepted: true, .. }
    ));
    write_client(&mut client_write, &ClientMessage::ListWorkspaces).await;
    let DaemonMessage::Workspaces(workspaces) = read_daemon(&mut reader).await else {
        panic!("workspaces");
    };
    write_client(
        &mut client_write,
        &ClientMessage::CreateThread {
            workspace_id: workspaces[0].id.clone(),
            title: "composer".into(),
        },
    )
    .await;
    let DaemonMessage::ThreadCreated(thread) = read_daemon(&mut reader).await else {
        panic!("thread");
    };

    write_client(&mut client_write, &ClientMessage::ListModels).await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Models { items } => {
            assert!(items.iter().any(|m| m.id == "claude-sonnet-5-5"))
        }
        other => panic!("{other:?}"),
    }
    write_client(
        &mut client_write,
        &ClientMessage::ListMentionables {
            thread_id: thread.id.clone(),
            query: "same".into(),
            limit: None,
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Mentionables { items, query, .. } => {
            assert_eq!(query, "same");
            assert_eq!(items[0].label, "same.txt");
        }
        other => panic!("{other:?}"),
    }
    // Not subscribed: replies come directly.
    write_client(
        &mut client_write,
        &ClientMessage::SetThreadModel {
            thread_id: thread.id.clone(),
            model: Some("claude-opus-5-5".into()),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::ThreadModelChanged { model, .. } => {
            assert_eq!(model.as_deref(), Some("claude-opus-5-5"))
        }
        other => panic!("{other:?}"),
    }
    write_client(
        &mut client_write,
        &ClientMessage::OpenTerminal {
            thread_id: thread.id.clone(),
            repository_id: None,
            command: Some("sleep 30".into()),
            cols: 80,
            rows: 24,
        },
    )
    .await;
    let terminal = match read_daemon(&mut reader).await {
        DaemonMessage::TerminalOpened { terminal, .. } => terminal,
        other => panic!("{other:?}"),
    };
    assert_eq!(terminal.exit_status, None);
    write_client(
        &mut client_write,
        &ClientMessage::CloseTerminal {
            terminal_id: terminal.terminal_id.clone(),
        },
    )
    .await;
    write_client(
        &mut client_write,
        &ClientMessage::ListTerminals {
            thread_id: thread.id.clone(),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Terminals { terminals, .. } => assert!(terminals.is_empty()),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn hello_send_prompt_interrupt_over_duplex() {
    let (client, server) = duplex(64 * 1024);
    let sessions = test_sessions().await;
    let (shutdown, _rx) = watch::channel(false);
    tokio::spawn(async move {
        let collab = CollabService::disabled(sessions.clone());
        let _ = handle_connection(server, sessions, collab, shutdown).await;
    });

    let (client_read, mut client_write) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);

    write_client(
        &mut client_write,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test".into(),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Welcome { accepted, .. } => assert!(accepted),
        other => panic!("expected welcome, got {other:?}"),
    }

    write_client(&mut client_write, &ClientMessage::ListWorkspaces).await;
    let workspaces = match read_daemon(&mut reader).await {
        DaemonMessage::Workspaces(list) => list,
        other => panic!("expected workspaces, got {other:?}"),
    };
    assert_eq!(workspaces.len(), 1);
    let workspace_id = workspaces[0].id.clone();

    write_client(
        &mut client_write,
        &ClientMessage::CreateThread {
            workspace_id,
            title: "duplex".into(),
        },
    )
    .await;
    let thread = match read_daemon(&mut reader).await {
        DaemonMessage::ThreadCreated(snapshot) => snapshot,
        other => panic!("expected thread created, got {other:?}"),
    };

    write_client(
        &mut client_write,
        &ClientMessage::SubscribeThread {
            thread_id: thread.id.clone(),
        },
    )
    .await;

    write_client(
        &mut client_write,
        &ClientMessage::SendPrompt {
            thread_id: thread.id.clone(),
            text: "hello".into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        },
    )
    .await;

    let mut events = Vec::new();
    let deadline = Duration::from_secs(10);
    loop {
        let msg = timeout(deadline, async { read_daemon(&mut reader).await })
            .await
            .expect("event timeout");
        let is_delta = matches!(
            &msg,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TextDelta { .. },
                ..
            }
        );
        events.push(msg);
        if is_delta {
            break;
        }
    }
    assert!(
        events.iter().any(|m| matches!(
            m,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TextDelta { .. },
                ..
            } | DaemonMessage::MessageAppended { .. }
        )),
        "expected at least one agent text delta or appended message"
    );

    write_client(
        &mut client_write,
        &ClientMessage::Interrupt {
            thread_id: thread.id.clone(),
        },
    )
    .await;

    tokio::time::sleep(Duration::from_millis(350)).await;
    let (drain_tx, mut drain_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let mut line = String::new();
            match timeout(Duration::from_millis(80), reader.read_line(&mut line)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(_)) => {
                    if let Ok(msg) = decode_json_line::<DaemonMessage>(&line) {
                        let _ = drain_tx.send(msg);
                    }
                }
                Ok(Err(_)) => break,
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut late = Vec::new();
    while let Ok(msg) = drain_rx.try_recv() {
        late.push(msg);
    }
    let completed = late.iter().any(|m| {
        matches!(
            m,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TurnCompleted { .. },
                ..
            }
        )
    });
    assert!(!completed, "interrupt should cancel remaining mock events");
}

#[tokio::test]
async fn mismatched_hello_is_rejected() {
    let (client, server) = duplex(4096);
    let sessions = test_sessions().await;
    let (shutdown, _rx) = watch::channel(false);
    tokio::spawn(async move {
        let collab = CollabService::disabled(sessions.clone());
        let _ = handle_connection(server, sessions, collab, shutdown).await;
    });
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);
    write_client(
        &mut client_write,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            client_name: "old".into(),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Welcome { accepted, .. } => assert!(!accepted),
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn first_message_must_be_hello() {
    let (client, server) = duplex(4096);
    let sessions = test_sessions().await;
    let (shutdown, _rx) = watch::channel(false);
    tokio::spawn(async move {
        let collab = CollabService::disabled(sessions.clone());
        let _ = handle_connection(server, sessions, collab, shutdown).await;
    });
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);
    write_client(&mut client_write, &ClientMessage::ListWorkspaces).await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Welcome { accepted, .. } => assert!(!accepted),
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_config_onboarding_persists_across_reopen_over_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trama.db");
    let db = TramaDb::open(&db_path).unwrap();
    let config = crate::config::load_or_init_config(&dir.path().join("config.json")).unwrap();
    assert!(config.workspaces.is_empty());
    crate::config::seed_from_config(&db, &config).await.unwrap();

    let source = dir.path().join("app");
    std::fs::create_dir_all(&source).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-b", "main"]);
    std::fs::write(source.join("same.txt"), "source\n").unwrap();
    git(&["add", "same.txt"]);
    git(&["commit", "-m", "initial"]);

    let fake = FakeAdapter::scripted();
    let sessions = SessionManager::with_checkout_root(
        db.clone(),
        Arc::new(fake.clone()),
        dir.path().join("managed"),
    );
    let (client, server) = duplex(64 * 1024);
    let (shutdown, _rx) = watch::channel(false);
    let server = tokio::spawn(async move {
        let collab = CollabService::disabled(sessions.clone());
        let _ = handle_connection(server, sessions, collab, shutdown).await;
    });
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut reader = BufReader::new(client_read);

    write_client(
        &mut client_write,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "onboarding-test".into(),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Welcome { accepted, .. } => assert!(accepted),
        other => panic!("expected welcome, got {other:?}"),
    }

    write_client(
        &mut client_write,
        &ClientMessage::CreateWorkspace {
            name: "Demo".into(),
        },
    )
    .await;
    let workspace = match read_daemon(&mut reader).await {
        DaemonMessage::WorkspaceCreated(snapshot) => snapshot,
        other => panic!("expected workspace, got {other:?}"),
    };
    assert_eq!(workspace.title, "Demo");
    assert_eq!(workspace.repo_count, 0);

    write_client(
        &mut client_write,
        &ClientMessage::CreateThread {
            workspace_id: workspace.id.clone(),
            title: "too soon".into(),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::Error { message, .. } => {
            assert!(message.contains("Add a local Git repository"), "{message}");
        }
        other => panic!("expected error, got {other:?}"),
    }

    write_client(
        &mut client_write,
        &ClientMessage::AddRepository {
            workspace_id: workspace.id.clone(),
            path: source.clone(),
            alias: "app".into(),
        },
    )
    .await;
    let added = match read_daemon(&mut reader).await {
        DaemonMessage::RepositoryAdded { repository, .. } => repository,
        other => panic!("expected repository, got {other:?}"),
    };
    assert_eq!(added.title, "app");
    assert!(added.default_enabled);

    write_client(
        &mut client_write,
        &ClientMessage::AddRepository {
            workspace_id: workspace.id.clone(),
            path: source.clone(),
            alias: "app".into(),
        },
    )
    .await;
    match read_daemon(&mut reader).await {
        DaemonMessage::RepositoryAdded { repository, .. } => {
            assert_eq!(repository.id, added.id);
        }
        other => panic!("expected idempotent repository, got {other:?}"),
    }

    write_client(
        &mut client_write,
        &ClientMessage::CreateThread {
            workspace_id: workspace.id.clone(),
            title: "first".into(),
        },
    )
    .await;
    let thread = match read_daemon(&mut reader).await {
        DaemonMessage::ThreadCreated(snapshot) => snapshot,
        other => panic!("expected thread, got {other:?}"),
    };

    write_client(
        &mut client_write,
        &ClientMessage::SubscribeThread {
            thread_id: thread.id.clone(),
        },
    )
    .await;
    write_client(
        &mut client_write,
        &ClientMessage::SendPrompt {
            thread_id: thread.id.clone(),
            text: "hello".into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        },
    )
    .await;
    let deadline = Duration::from_secs(10);
    let mut saw_agent = false;
    for _ in 0..20 {
        let msg = timeout(deadline, async { read_daemon(&mut reader).await })
            .await
            .expect("event timeout");
        if matches!(
            msg,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::TextDelta { .. },
                ..
            }
        ) {
            saw_agent = true;
            break;
        }
    }
    assert!(saw_agent, "fake adapter should stream over IPC");

    drop(client_write);
    drop(reader);
    server.await.unwrap();
    crate::config::seed_from_config(&db, &crate::config::FileConfig::default())
        .await
        .unwrap();
    drop(db);
    let reopened = TramaDb::open(&db_path).unwrap();
    let workspaces = reopened.list_workspaces().await.unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].name, "Demo");
    assert_eq!(
        reopened
            .list_memberships(&workspaces[0].id)
            .await
            .unwrap()
            .len(),
        1
    );
    let threads = reopened.list_threads(&workspaces[0].id).await.unwrap();
    assert_eq!(threads.len(), 1);
    assert!(threads[0].repo_attachments[0].checkout_id.is_some());
    assert!(threads[0].repo_attachments[0].base_revision.is_some());
    assert!(
        !reopened
            .list_messages_by_thread(&threads[0].id)
            .await
            .unwrap()
            .is_empty()
    );
    let configs = fake
        .start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(configs.len(), 1);
    assert!(configs[0].resume_external_session.is_none());
    assert!(
        configs[0].cwd.starts_with(
            dunce::canonicalize(&source)
                .unwrap()
                .join(".trama/worktrees")
        )
    );
}
