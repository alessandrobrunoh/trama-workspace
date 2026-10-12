//! Per-connection JSON-lines handler.

use crate::error::Result;
use crate::session::{SessionManager, SubscriptionId};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use trama_domain::ids::ThreadId;
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn handle_connection<S>(
    stream: S,
    sessions: SessionManager,
    shutdown: watch::Sender<bool>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    let Some(first) = lines.next_line().await? else {
        return Ok(());
    };
    if first.trim().is_empty() {
        write_msg(&mut writer, &welcome(false)).await?;
        return Ok(());
    }

    let accepted = match decode_json_line::<ClientMessage>(&first) {
        Ok(ClientMessage::Hello {
            protocol_version,
            client_name: _,
        }) => protocol_version == PROTOCOL_VERSION,
        _ => false,
    };
    write_msg(&mut writer, &welcome(accepted)).await?;
    if !accepted {
        return Ok(());
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<DaemonMessage>();
    let mut subscriptions: HashMap<ThreadId, SubscriptionId> = HashMap::new();

    loop {
        tokio::select! {
            incoming = lines.next_line() => {
                match incoming {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match handle_line(
                            &line,
                            &sessions,
                            &out_tx,
                            &mut subscriptions,
                            &shutdown,
                        ).await {
                            Ok(ConnectionControl::Continue) => {}
                            Ok(ConnectionControl::Close) => break,
                            Err(err) => {
                                let _ = out_tx.send(DaemonMessage::Error {
                                    code: err.code().into(),
                                    message: err.to_string(),
                                });
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            outbound = out_rx.recv() => {
                match outbound {
                    Some(msg) => write_msg(&mut writer, &msg).await?,
                    None => break,
                }
            }
        }
    }

    let leftover: Vec<_> = subscriptions.into_iter().collect();
    sessions.unsubscribe_all(&leftover);
    Ok(())
}

enum ConnectionControl {
    Continue,
    Close,
}

fn welcome(accepted: bool) -> DaemonMessage {
    DaemonMessage::Welcome {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: DAEMON_VERSION.to_string(),
        accepted,
    }
}

async fn write_msg<W: AsyncWrite + Unpin>(writer: &mut W, msg: &DaemonMessage) -> Result<()> {
    let bytes = encode_json_line(msg)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn handle_line(
    line: &str,
    sessions: &SessionManager,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    subscriptions: &mut HashMap<ThreadId, SubscriptionId>,
    shutdown: &watch::Sender<bool>,
) -> Result<ConnectionControl> {
    let msg: ClientMessage = match decode_json_line(line) {
        Ok(msg) => msg,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                code: "invalid_json".into(),
                message: err.to_string(),
            });
            return Ok(ConnectionControl::Continue);
        }
    };

    match msg {
        ClientMessage::Hello { .. } => {
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "hello already accepted".into(),
            });
        }
        ClientMessage::ListWorkspaces => {
            let _ = out_tx.send(DaemonMessage::Workspaces(sessions.list_workspaces().await?));
        }
        ClientMessage::ListThreads { workspace_id } => {
            let threads = sessions.list_threads(&workspace_id).await?;
            let _ = out_tx.send(DaemonMessage::Threads(threads));
        }
        ClientMessage::CreateThread {
            workspace_id,
            title,
        } => {
            let snapshot = sessions.create_thread(workspace_id, title).await?;
            let _ = out_tx.send(DaemonMessage::ThreadCreated(snapshot));
        }
        ClientMessage::SubscribeThread { thread_id } => {
            if subscriptions.contains_key(&thread_id) {
                return Ok(ConnectionControl::Continue);
            }
            let sub_id = sessions.subscribe(&thread_id, out_tx.clone()).await?;
            subscriptions.insert(thread_id, sub_id);
        }
        ClientMessage::UnsubscribeThread { thread_id } => {
            if let Some(sub_id) = subscriptions.remove(&thread_id) {
                sessions.unsubscribe(&thread_id, sub_id)?;
            }
        }
        ClientMessage::SendPrompt { thread_id, text } => {
            if let Err(error) = sessions.send_prompt(thread_id.clone(), text).await {
                let message = error.to_string();
                let _ = out_tx.send(DaemonMessage::Error {
                    code: error.code().into(),
                    message: message.clone(),
                });
                // This attempt was rejected, not another subscriber's running
                // turn. Reply only to the requesting client, scoped by thread.
                let _ = out_tx.send(DaemonMessage::AgentEventEnvelope {
                    thread_id,
                    event: trama_domain::agent::AgentEvent::Failed { message },
                });
            }
        }
        ClientMessage::Interrupt { thread_id } => {
            sessions.interrupt(&thread_id).await?;
        }
        ClientMessage::RespondApproval {
            thread_id,
            request_id,
            decision,
        } => {
            sessions
                .respond_approval(&thread_id, &request_id, decision)
                .await?;
        }
        ClientMessage::CreateWorkspace { name } => {
            let snapshot = sessions.create_workspace(name).await?;
            let _ = out_tx.send(DaemonMessage::WorkspaceCreated(snapshot));
        }
        ClientMessage::AddRepository {
            workspace_id,
            path,
            alias,
        } => {
            let repository = sessions
                .add_repository(workspace_id.clone(), path, alias)
                .await?;
            let _ = out_tx.send(DaemonMessage::RepositoryAdded {
                workspace_id,
                repository,
            });
        }
        ClientMessage::ListRepositories { workspace_id } => {
            let repositories = sessions.list_repositories(&workspace_id).await?;
            let _ = out_tx.send(DaemonMessage::Repositories {
                workspace_id,
                repositories,
            });
        }
        ClientMessage::MigrateLegacyThread { thread_id } => {
            let snapshot = sessions.migrate_legacy_thread(thread_id).await?;
            let _ = out_tx.send(DaemonMessage::ThreadMigrated(snapshot));
        }
        ClientMessage::GetThreadCheckouts { thread_id } => {
            let checkouts = sessions.thread_checkouts(&thread_id).await?;
            let _ = out_tx.send(DaemonMessage::ThreadCheckouts {
                thread_id,
                checkouts,
            });
        }
        ClientMessage::Shutdown => {
            let _ = shutdown.send(true);
            return Ok(ConnectionControl::Close);
        }
    }
    Ok(ConnectionControl::Continue)
}

#[cfg(test)]
mod tests {
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
        timeout(Duration::from_secs(3), reader.read_line(&mut line))
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
        })
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (shutdown, _) = watch::channel(false);
        let result = handle_line(&line, &sessions, &tx, &mut HashMap::new(), &shutdown).await;
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
    async fn hello_send_prompt_interrupt_over_duplex() {
        let (client, server) = duplex(64 * 1024);
        let sessions = test_sessions().await;
        let (shutdown, _rx) = watch::channel(false);
        tokio::spawn(async move {
            let _ = handle_connection(server, sessions, shutdown).await;
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
            },
        )
        .await;

        let mut events = Vec::new();
        let deadline = Duration::from_secs(3);
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
            let _ = handle_connection(server, sessions, shutdown).await;
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
            let _ = handle_connection(server, sessions, shutdown).await;
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
            let _ = handle_connection(server, sessions, shutdown).await;
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
            },
        )
        .await;
        let deadline = Duration::from_secs(3);
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
            configs[0]
                .cwd
                .starts_with(source.canonicalize().unwrap().join(".trama/worktrees"))
        );
    }
}
