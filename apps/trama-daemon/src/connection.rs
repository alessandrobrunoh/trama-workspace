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
                        ) {
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

fn handle_line(
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
            let _ = out_tx.send(DaemonMessage::Workspaces(sessions.list_workspaces()));
        }
        ClientMessage::ListThreads { workspace_id } => {
            let threads = sessions.list_threads(&workspace_id)?;
            let _ = out_tx.send(DaemonMessage::Threads(threads));
        }
        ClientMessage::CreateThread {
            workspace_id,
            title,
        } => {
            let snapshot = sessions.create_thread(workspace_id, title)?;
            let _ = out_tx.send(DaemonMessage::ThreadCreated(snapshot));
        }
        ClientMessage::SubscribeThread { thread_id } => {
            if subscriptions.contains_key(&thread_id) {
                return Ok(ConnectionControl::Continue);
            }
            let sub_id = sessions.subscribe(&thread_id, out_tx.clone())?;
            subscriptions.insert(thread_id, sub_id);
        }
        ClientMessage::UnsubscribeThread { thread_id } => {
            if let Some(sub_id) = subscriptions.remove(&thread_id) {
                sessions.unsubscribe(&thread_id, sub_id)?;
            }
        }
        ClientMessage::SendPrompt { thread_id, text } => {
            sessions.send_prompt(thread_id, text)?;
        }
        ClientMessage::Interrupt { thread_id } => {
            sessions.interrupt(&thread_id)?;
        }
        ClientMessage::RespondApproval {
            thread_id,
            request_id,
            decision,
        } => {
            sessions.respond_approval(&thread_id, &request_id, decision)?;
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
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};
    use tokio::sync::{mpsc, watch};
    use tokio::time::{Duration, timeout};
    use trama_domain::agent::AgentEvent;
    use trama_protocol::{
        ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
    };

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
    async fn hello_send_prompt_interrupt_over_duplex() {
        let (client, server) = duplex(64 * 1024);
        let sessions = SessionManager::new();
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
        // Drain anything already buffered; TurnCompleted should not appear after interrupt.
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
        let sessions = SessionManager::new();
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
        let sessions = SessionManager::new();
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
}
