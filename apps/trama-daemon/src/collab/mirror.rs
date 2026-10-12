//! Client socket of one linked thread: mirror other participants' events
//! into the local thread, relay drafts and presence.

use super::http::CollabError;
use super::{CollabService, Connection, REMOTE_ACTOR_PREFIX, truncate};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use trama_collab_protocol::{ClientFrame, EventEnvelope, EventKind, ServerFrame};
use trama_domain::agent::AgentEvent;
use trama_domain::event::{Aggregate, DomainEvent, EventRecord};
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::{Actor, ContentBlock, Message, Role};
use trama_protocol::DaemonMessage;

const PING_EVERY: Duration = Duration::from_secs(25);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const MAX_MIRRORED_TEXT_BYTES: usize = 64 * 1024;

enum Ended {
    Retry,
    Stop,
}

pub(super) async fn run(
    service: CollabService,
    generation: u64,
    thread_id: ThreadId,
    collab_thread_id: String,
    mut frames: mpsc::UnboundedReceiver<ClientFrame>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        let Some(connection) = service.connection(Some(generation)) else {
            break;
        };
        let Some(token) = connection.session_token.clone() else {
            break;
        };
        let cursor = match service.db().collab_link(&thread_id).await {
            Ok(Some(link)) if link.collab_thread_id == collab_thread_id => link.cursor,
            Ok(_) => break,
            Err(err) => {
                crate::log::warn(format!("shared thread cursor unavailable: {err}"));
                break;
            }
        };
        let path = format!("/threads/{collab_thread_id}/ws?after={cursor}");
        let ended = match connection.endpoint.websocket(&path, &token).await {
            Ok(ws) => {
                backoff = Duration::from_millis(250);
                serve(&service, &connection, &thread_id, cursor, ws, &mut frames).await
            }
            Err(CollabError::NotFound) => {
                revoked(&service, &thread_id);
                Ended::Stop
            }
            Err(CollabError::Unauthorized) => {
                service.session_rejected(generation);
                Ended::Stop
            }
            Err(CollabError::Rejected(..)) => {
                // A cursor beyond the log head: the service lost history this
                // thread already mirrored. Do not guess; surface it.
                service.note_error(
                    generation,
                    format!(
                        "shared thread {collab_thread_id} no longer matches the local copy; \
                         unlink it and join again"
                    ),
                );
                Ended::Stop
            }
            Err(err) => {
                service.note_error(generation, err.to_string());
                Ended::Retry
            }
        };
        match ended {
            Ended::Stop => break,
            Ended::Retry => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
    service.mirror_ended(&thread_id, generation);
}

fn revoked(service: &CollabService, thread_id: &ThreadId) {
    crate::log::info(format!(
        "access to shared thread of {thread_id} was revoked"
    ));
    service.sessions().notify(
        thread_id,
        DaemonMessage::CollabAccessRevoked {
            thread_id: thread_id.clone(),
        },
    );
}

async fn serve(
    service: &CollabService,
    connection: &Connection,
    thread_id: &ThreadId,
    mut cursor: i64,
    ws: super::http::Ws,
    frames: &mut mpsc::UnboundedReceiver<ClientFrame>,
) -> Ended {
    let (mut sink, mut stream) = ws.split();
    let mut caught_up = false;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            outgoing = frames.recv() => {
                let Some(mut frame) = outgoing else { return Ended::Stop };
                // Drafts are last-write-wins: send only the newest queued one.
                if matches!(frame, ClientFrame::Draft { .. }) {
                    while let Ok(next) = frames.try_recv() {
                        if matches!(next, ClientFrame::Draft { .. }) {
                            frame = next;
                            continue;
                        }
                        if send(&mut sink, &frame).await.is_err() {
                            return Ended::Retry;
                        }
                        frame = next;
                        break;
                    }
                }
                if send(&mut sink, &frame).await.is_err() {
                    return Ended::Retry;
                }
            }
            _ = ping.tick() => {
                if send(&mut sink, &ClientFrame::Ping).await.is_err() {
                    return Ended::Retry;
                }
            }
            incoming = stream.next() => {
                let message = match incoming {
                    Some(Ok(message)) => message,
                    _ => return Ended::Retry,
                };
                let frame = match &message {
                    WsMessage::Text(text) => serde_json::from_str::<ServerFrame>(text.as_str()).ok(),
                    WsMessage::Close(_) => return Ended::Retry,
                    _ => None,
                };
                match frame {
                    Some(ServerFrame::Event { event }) => {
                        if event.seq <= cursor {
                            continue;
                        }
                        if let Err(err) = apply(service, connection, thread_id, &event, caught_up).await {
                            crate::log::warn(format!("could not mirror shared event: {err}"));
                            return Ended::Retry;
                        }
                        cursor = event.seq;
                    }
                    Some(ServerFrame::CaughtUp { .. }) => caught_up = true,
                    Some(ServerFrame::Draft { username, text, .. }) => {
                        if !connection.is_me(Some(&username)) {
                            service.sessions().notify(thread_id, DaemonMessage::CollabDraft {
                                thread_id: thread_id.clone(),
                                username,
                                text,
                            });
                        }
                    }
                    Some(ServerFrame::Presence { username, state }) => {
                        if !connection.is_me(Some(&username)) {
                            service.sessions().notify(thread_id, DaemonMessage::CollabPresence {
                                thread_id: thread_id.clone(),
                                username,
                                state,
                            });
                        }
                    }
                    Some(ServerFrame::Revoked) => {
                        revoked(service, thread_id);
                        return Ended::Stop;
                    }
                    Some(ServerFrame::Error { error }) => {
                        crate::log::warn(format!("shared thread socket: {error}"));
                    }
                    Some(ServerFrame::Hello { .. }) | Some(ServerFrame::Pong) | None => {}
                }
            }
        }
    }
}

async fn send<S>(sink: &mut S, frame: &ClientFrame) -> Result<(), ()>
where
    S: futures_util::Sink<WsMessage> + Unpin,
{
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    sink.send(WsMessage::Text(text.into()))
        .await
        .map_err(|_| ())
}

fn text_of(payload: &Value, key: &str) -> String {
    truncate(
        payload.get(key).and_then(Value::as_str).unwrap_or_default(),
        MAX_MIRRORED_TEXT_BYTES,
    )
}

fn remote_message(thread_id: &ThreadId, author: &str, role: Role, text: String) -> Message {
    Message {
        id: MessageId::generate(),
        thread_id: thread_id.clone(),
        parent_id: None,
        actor: Actor::Human {
            id: format!("{REMOTE_ACTOR_PREFIX}{author}"),
        },
        role,
        content: vec![ContentBlock::Text { text }],
        created_at: Utc::now(),
    }
}

/// Transcript text for a non-message event, or `None` when it is only
/// relayed in structured form.
fn describe(event: &EventEnvelope, author: &str) -> Option<String> {
    let payload = &event.payload;
    match event.kind {
        EventKind::CommentCreated => {
            let mut text = format!("{author} commented: {}", text_of(payload, "body"));
            if let Some(excerpt) = payload
                .get("anchor")
                .and_then(|a| a.get("excerpt"))
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty())
            {
                text.push_str("\n> ");
                text.push_str(&truncate(excerpt, 1024).replace('\n', "\n> "));
            }
            Some(text)
        }
        EventKind::CommentReplied => Some(format!(
            "{author} replied to a comment: {}",
            text_of(payload, "body")
        )),
        EventKind::CommentResolved => Some(format!("{author} resolved a comment.")),
        EventKind::ReviewVerdict => {
            let verdict = match payload.get("verdict").and_then(Value::as_str) {
                Some("approved") => "approved the changes",
                Some("changes_requested") => "requested changes",
                _ => "reviewed the changes",
            };
            let body = text_of(payload, "body");
            Some(if body.is_empty() {
                format!("{author} {verdict}.")
            } else {
                format!("{author} {verdict}: {body}")
            })
        }
        EventKind::ChangesSummary => {
            let mut lines = Vec::new();
            for repo in payload
                .get("repos")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let files: Vec<&str> = repo
                    .get("files")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.get("path").and_then(Value::as_str))
                    .take(20)
                    .collect();
                let count = repo
                    .get("file_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(files.len() as u64);
                lines.push(format!(
                    "{} ({count} file{}): {}",
                    repo.get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("repository"),
                    if count == 1 { "" } else { "s" },
                    files.join(", ")
                ));
            }
            Some(format!(
                "{author}'s turn changed files on their machine:\n{}",
                lines.join("\n")
            ))
        }
        EventKind::CommandAmbiguous => Some(format!(
            "{author}'s turn has an unknown outcome: their runner was lost after claiming it. \
             It is not retried automatically."
        )),
        _ => None,
    }
}

async fn apply(
    service: &CollabService,
    connection: &Connection,
    thread_id: &ThreadId,
    event: &EventEnvelope,
    live: bool,
) -> crate::error::Result<()> {
    let own = connection.is_own_command(event.command_id.as_deref());
    let author = event.author.clone().unwrap_or_else(|| "someone".into());
    let mut messages = Vec::new();
    let mut structured = !own;
    if own {
        // This daemon already has its own turns, comments and verdicts
        // locally. Only an ambiguous outcome needs recording here.
        if event.kind == EventKind::CommandAmbiguous {
            if let Some(command_id) = &event.command_id {
                service.mark_ambiguous(command_id, thread_id).await;
            }
        }
    } else {
        match event.kind {
            EventKind::MessageAppended => messages.push(remote_message(
                thread_id,
                &author,
                Role::User,
                text_of(&event.payload, "text"),
            )),
            EventKind::AgentEvent => {
                structured = false;
                match event.payload.get("type").and_then(Value::as_str) {
                    Some("message") => messages.push(remote_message(
                        thread_id,
                        &author,
                        Role::Assistant,
                        text_of(&event.payload, "text"),
                    )),
                    Some("notice") => messages.push(remote_message(
                        thread_id,
                        &author,
                        Role::System,
                        text_of(&event.payload, "text"),
                    )),
                    Some("event") if live => {
                        if let Some(agent_event) = event
                            .payload
                            .get("event")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<AgentEvent>(v).ok())
                        {
                            service.sessions().notify(
                                thread_id,
                                DaemonMessage::CollabAgentEvent {
                                    thread_id: thread_id.clone(),
                                    author: author.clone(),
                                    command_id: event.command_id.clone().unwrap_or_default(),
                                    event: agent_event,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                if let Some(text) = describe(event, &author) {
                    messages.push(remote_message(thread_id, &author, Role::System, text));
                }
            }
        }
    }
    let records = messages
        .iter()
        .map(|message| {
            EventRecord::new(
                Aggregate::Thread {
                    id: thread_id.clone(),
                },
                message.actor.clone(),
                DomainEvent::MessageAdded {
                    message: message.clone(),
                },
                vec![],
            )
        })
        .collect::<trama_domain::Result<Vec<_>>>()?;
    let applied = service
        .db()
        .apply_collab_event(thread_id, event.seq, &messages, &records)
        .await?;
    if !applied {
        return Ok(());
    }
    for message in messages {
        service.sessions().notify(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            },
        );
    }
    if structured {
        service.sessions().notify(
            thread_id,
            DaemonMessage::CollabEvent {
                thread_id: thread_id.clone(),
                event: event.clone(),
            },
        );
    }
    Ok(())
}
