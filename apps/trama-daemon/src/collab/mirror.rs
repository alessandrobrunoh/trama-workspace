//! Client socket of one linked thread: mirror other participants' events
//! into the local thread, relay drafts and presence.

use super::history;
use super::http::CollabError;
use super::{CollabService, Connection, REMOTE_ACTOR_PREFIX, truncate};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use trama_collab_protocol::{ClientFrame, EventEnvelope, EventKind, ServerFrame};
use trama_db::{CollabApply, ReviewVerdictRecord};
use trama_domain::agent::AgentEvent;
use trama_domain::comment::{Comment, CommentTarget};
use trama_domain::event::{Aggregate, DomainEvent, EventRecord};
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_domain::message::{Actor, ContentBlock, Message, MessageAnnotation, Role};
use trama_domain::subagent::{SubagentProfile, SubagentStatus};
use trama_domain::subthread::ReviewVerdict;
use trama_domain::thread::ThreadKind;
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
                // The service is reachable again: retry the outbox now.
                service.reset_outbox_backoff().await;
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

fn remote_actor(author: &str) -> Actor {
    Actor::Human {
        id: format!("{REMOTE_ACTOR_PREFIX}{author}"),
    }
}

fn remote_message(thread_id: &ThreadId, author: &str, role: Role, text: String) -> Message {
    Message {
        id: MessageId::generate(),
        thread_id: thread_id.clone(),
        parent_id: None,
        actor: remote_actor(author),
        role,
        content: vec![ContentBlock::Text { text }],
        created_at: Utc::now(),
        claude_uuid: None,
        annotation: None,
    }
}

/// Transcript text for a non-message event, or `None` when it is only
/// relayed in structured form.
fn describe(event: &EventEnvelope, author: &str) -> Option<String> {
    let payload = &event.payload;
    match event.kind {
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

/// What to tell subscribers after an event was applied.
#[derive(Default)]
struct Notices {
    comments_created: Vec<CommentId>,
    comments_updated: Vec<CommentId>,
    comments_deleted: Vec<CommentId>,
    remote_changes: bool,
    status: Option<(ThreadId, SubagentStatus, Option<String>)>,
    verdict: Option<(ThreadId, ReviewVerdict)>,
}

fn parse_verdict(payload: &Value) -> Option<ReviewVerdict> {
    match payload.get("verdict").and_then(Value::as_str) {
        Some("approved") => Some(ReviewVerdict::Approve),
        Some("changes_requested") => Some(ReviewVerdict::RequestChanges {
            summary: text_of(payload, "body"),
        }),
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
    let db = service.db();
    let mut apply = CollabApply::default();
    let mut notices = Notices::default();
    let mut structured = !own;
    // Origin of messages created from a non-message event.
    let event_origin = format!("evt:{}:{}", event.thread_id, event.seq);
    if own {
        // This daemon already has its own turns, history, comments and
        // verdicts locally. Only an ambiguous outcome needs recording here.
        if event.kind == EventKind::CommandAmbiguous {
            if let Some(command_id) = &event.command_id {
                service.mark_ambiguous(command_id, thread_id).await;
            }
        }
    } else {
        match event.kind {
            EventKind::MessageAppended => {
                let origin = match &event.command_id {
                    Some(command_id) => history::origin_for_prompt(command_id),
                    None => event_origin.clone(),
                };
                if db
                    .collab_origin_message(thread_id, &origin)
                    .await?
                    .is_none()
                {
                    apply.messages.push((
                        remote_message(
                            thread_id,
                            &author,
                            Role::User,
                            text_of(&event.payload, "text"),
                        ),
                        Some(origin),
                    ));
                }
            }
            EventKind::HistoryMessage => {
                structured = false;
                let payload = &event.payload;
                let origin = payload
                    .get("origin")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| event_origin.clone());
                if db
                    .collab_origin_message(thread_id, &origin)
                    .await?
                    .is_none()
                {
                    let content: Vec<ContentBlock> = payload
                        .get("content")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();
                    let annotation = match payload
                        .get("annotation")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<MessageAnnotation>(v).ok())
                    {
                        Some(annotation) => {
                            history::mirror_annotation(db, connection.endpoint.url(), annotation)
                                .await
                        }
                        None => None,
                    };
                    let created_at = payload
                        .get("created_at")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<DateTime<Utc>>(v).ok())
                        .unwrap_or_else(Utc::now);
                    let message = Message {
                        id: MessageId::generate(),
                        thread_id: thread_id.clone(),
                        parent_id: None,
                        actor: remote_actor(&author),
                        role: history::parse_role(payload.get("role").and_then(Value::as_str)),
                        content,
                        created_at,
                        claude_uuid: None,
                        annotation,
                    };
                    if payload
                        .get("activity_parts")
                        .and_then(Value::as_u64)
                        .is_some_and(|n| n > 0)
                    {
                        apply.take_activity = Some((origin.clone(), message.id.clone()));
                    }
                    apply.messages.push((message, Some(origin)));
                }
            }
            EventKind::HistoryActivity => {
                structured = false;
                let payload = &event.payload;
                if let (Some(origin), Some(part), Some(items)) = (
                    payload.get("origin").and_then(Value::as_str),
                    payload.get("part").and_then(Value::as_i64),
                    payload.get("items"),
                ) {
                    apply.stage_activity = Some((origin.to_owned(), part, items.to_string()));
                }
            }
            EventKind::AgentEvent => {
                structured = false;
                match event.payload.get("type").and_then(Value::as_str) {
                    // Older peers report their answers this way.
                    Some("message") => apply.messages.push((
                        remote_message(
                            thread_id,
                            &author,
                            Role::Assistant,
                            text_of(&event.payload, "text"),
                        ),
                        Some(event_origin.clone()),
                    )),
                    Some("notice") => apply.messages.push((
                        remote_message(
                            thread_id,
                            &author,
                            Role::System,
                            text_of(&event.payload, "text"),
                        ),
                        Some(event_origin.clone()),
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
            EventKind::CommentCreated | EventKind::CommentReplied => {
                let payload = &event.payload;
                let body = text_of(payload, "body");
                let (target, excerpt, parent_id) = if event.kind == EventKind::CommentReplied {
                    let parent_seq = payload.get("parent_seq").and_then(Value::as_i64);
                    let parent = match parent_seq {
                        Some(seq) => db.collab_comment_by_seq(&event.thread_id, seq).await?,
                        None => None,
                    };
                    let parent = match parent {
                        Some(parent) => db.get_comment(&parent).await?.map(|r| r.comment),
                        None => None,
                    };
                    match parent {
                        Some(parent) => (parent.target, parent.excerpt, Some(parent.id)),
                        None => (
                            CommentTarget::MessageRange {
                                message_id: MessageId::from(event_origin.as_str()),
                                start_byte: 0,
                                end_byte: 0,
                            },
                            None,
                            None,
                        ),
                    }
                } else {
                    let anchor = payload.get("anchor").cloned().unwrap_or(Value::Null);
                    let excerpt = anchor
                        .get("excerpt")
                        .and_then(Value::as_str)
                        .map(|e| truncate(e, 4096));
                    let target = match anchor
                        .get("target")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<CommentTarget>(v).ok())
                    {
                        Some(target) => history::mirror_target(db, thread_id, target).await,
                        None => CommentTarget::MessageRange {
                            message_id: MessageId::from(event_origin.as_str()),
                            start_byte: 0,
                            end_byte: 0,
                        },
                    };
                    (target, excerpt, None)
                };
                let comment = Comment {
                    id: CommentId::generate(),
                    thread_id: thread_id.clone(),
                    author: remote_actor(&author),
                    target,
                    body,
                    created_at: Utc::now(),
                    resolved_at: None,
                    parent_id,
                    excerpt,
                };
                apply.events.push(EventRecord::new(
                    Aggregate::Thread {
                        id: thread_id.clone(),
                    },
                    comment.author.clone(),
                    DomainEvent::CommentCreated {
                        comment: comment.clone(),
                    },
                    vec![],
                )?);
                notices.comments_created.push(comment.id.clone());
                apply
                    .comments
                    .push((comment, event.thread_id.clone(), event.seq));
            }
            EventKind::CommentResolved | EventKind::CommentReopened | EventKind::CommentDeleted => {
                let seq = event.payload.get("comment_seq").and_then(Value::as_i64);
                let comment = match seq {
                    Some(seq) => db.collab_comment_by_seq(&event.thread_id, seq).await?,
                    None => None,
                };
                if let Some(comment) = comment {
                    let (domain, at) = match event.kind {
                        EventKind::CommentResolved => {
                            let at = Utc::now();
                            (
                                DomainEvent::CommentResolved {
                                    thread_id: thread_id.clone(),
                                    comment_id: comment.clone(),
                                    resolved_at: at,
                                },
                                Some(Some(at)),
                            )
                        }
                        EventKind::CommentReopened => (
                            DomainEvent::CommentReopened {
                                thread_id: thread_id.clone(),
                                comment_id: comment.clone(),
                            },
                            Some(None),
                        ),
                        _ => {
                            let mut ids: Vec<CommentId> = db
                                .list_comments(thread_id)
                                .await?
                                .into_iter()
                                .filter(|r| r.comment.parent_id.as_ref() == Some(&comment))
                                .map(|r| r.comment.id)
                                .collect();
                            ids.push(comment.clone());
                            notices.comments_deleted = ids.clone();
                            (
                                DomainEvent::CommentsDeleted {
                                    thread_id: thread_id.clone(),
                                    comment_ids: ids,
                                },
                                None,
                            )
                        }
                    };
                    match at {
                        Some(resolved_at) => {
                            apply.resolved.push((comment.clone(), resolved_at));
                            notices.comments_updated.push(comment);
                        }
                        None => apply.deleted.push(comment),
                    }
                    apply.events.push(EventRecord::new(
                        Aggregate::Thread {
                            id: thread_id.clone(),
                        },
                        remote_actor(&author),
                        domain,
                        vec![],
                    )?);
                }
            }
            EventKind::ReviewVerdict => {
                let thread = db.get_thread(thread_id).await?;
                match (thread, parse_verdict(&event.payload)) {
                    (Some(thread), Some(verdict))
                        if thread.kind == ThreadKind::Review
                            && thread.parent_thread_id.is_some() =>
                    {
                        // The parent's visible row arrives as history.
                        let parent = thread
                            .parent_thread_id
                            .clone()
                            .unwrap_or_else(|| thread_id.clone());
                        apply.review_verdict = Some(ReviewVerdictRecord {
                            id: format!("collab:{}:{}", event.thread_id, event.seq),
                            review_thread_id: thread_id.clone(),
                            parent_thread_id: parent.clone(),
                            verdict: verdict.clone(),
                            snapshot: Vec::new(),
                            created_at: Utc::now(),
                        });
                        notices.verdict = Some((parent, verdict));
                    }
                    _ => {
                        if let Some(text) = describe(event, &author) {
                            apply.messages.push((
                                remote_message(thread_id, &author, Role::System, text),
                                Some(event_origin.clone()),
                            ));
                        }
                    }
                }
            }
            EventKind::ChangesSnapshot => {
                apply.remote_changes = Some((author.clone(), event.payload.to_string()));
                notices.remote_changes = true;
            }
            EventKind::SubagentStatus => {
                if let Some(status) = event
                    .payload
                    .get("status")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<SubagentStatus>(v).ok())
                {
                    let detail = event
                        .payload
                        .get("detail")
                        .and_then(Value::as_str)
                        .map(|d| truncate(d, 1024));
                    apply.subagent_status = Some((thread_id.clone(), status, detail.clone()));
                    notices.status = Some((thread_id.clone(), status, detail));
                }
            }
            EventKind::ChildThreadAdded => {
                if let Some((child, status)) =
                    mirror_child(service, connection, thread_id, event).await?
                {
                    apply.subagent_status = status.map(|(s, d)| (child.clone(), s, d));
                }
            }
            _ => {
                if let Some(text) = describe(event, &author) {
                    apply.messages.push((
                        remote_message(thread_id, &author, Role::System, text),
                        Some(event_origin.clone()),
                    ));
                }
            }
        }
    }
    let Some(applied) = db.apply_collab_changes(thread_id, event.seq, apply).await? else {
        return Ok(());
    };
    let sessions = service.sessions();
    if let Some((message_id, items)) = applied.activity {
        sessions.notify(
            thread_id,
            DaemonMessage::TurnActivity {
                thread_id: thread_id.clone(),
                message_id: Some(message_id),
                items,
            },
        );
    }
    for message in applied.messages {
        sessions.notify(
            thread_id,
            DaemonMessage::MessageAppended {
                thread_id: thread_id.clone(),
                message,
            },
        );
    }
    if !notices.comments_created.is_empty() {
        sessions
            .notify_comments(thread_id, &notices.comments_created, true)
            .await;
    }
    if !notices.comments_updated.is_empty() {
        sessions
            .notify_comments(thread_id, &notices.comments_updated, false)
            .await;
    }
    if !notices.comments_deleted.is_empty() {
        sessions.notify(
            thread_id,
            DaemonMessage::CommentDeleted {
                thread_id: thread_id.clone(),
                comment_ids: notices.comments_deleted,
            },
        );
    }
    if notices.remote_changes {
        if let Ok(snapshots) = service.remote_changes(thread_id).await {
            sessions.notify(
                thread_id,
                DaemonMessage::RemoteChanges {
                    thread_id: thread_id.clone(),
                    snapshots,
                },
            );
        }
    }
    if let Some((child, status, detail)) = notices.status {
        if let Ok(Some(thread)) = db.get_thread(&child).await {
            if let Some(parent) = thread.parent_thread_id {
                let message = DaemonMessage::SubagentStatusChanged {
                    thread_id: child.clone(),
                    parent_thread_id: parent.clone(),
                    status,
                    detail,
                };
                sessions.notify(&parent, message.clone());
                sessions.notify(&child, message);
            }
        }
    }
    if let Some((parent, verdict)) = notices.verdict {
        let posted = DaemonMessage::ReviewVerdictPosted {
            review_thread_id: thread_id.clone(),
            parent_thread_id: parent.clone(),
            verdict,
        };
        sessions.notify(&parent, posted.clone());
        sessions.notify(thread_id, posted);
    }
    if structured {
        sessions.notify(
            thread_id,
            DaemonMessage::CollabEvent {
                thread_id: thread_id.clone(),
                event: event.clone(),
            },
        );
    }
    Ok(())
}

/// Another participant created a child under this thread: create the local
/// child (once), link it and start mirroring it. Returns the child and the
/// subagent state it was announced with.
#[allow(clippy::type_complexity)]
async fn mirror_child(
    service: &CollabService,
    connection: &Connection,
    thread_id: &ThreadId,
    event: &EventEnvelope,
) -> crate::error::Result<Option<(ThreadId, Option<(SubagentStatus, Option<String>)>)>> {
    let payload = &event.payload;
    let Some(collab_child) = payload.get("child_thread_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if super::http::validate_path_segment(collab_child, "shared thread id").is_err() {
        return Ok(None);
    }
    let db = service.db();
    let url = connection.endpoint.url();
    let _serialized = service.inner.sync_lock.lock().await;
    if db.collab_link_by_remote(url, collab_child).await?.is_some() {
        return Ok(None);
    }
    let kind = payload
        .get("kind")
        .cloned()
        .and_then(|v| serde_json::from_value::<ThreadKind>(v).ok())
        .unwrap_or(ThreadKind::Sub);
    let subagent = payload.get("subagent").filter(|v| v.is_object());
    let profile = subagent
        .and_then(|v| v.get("profile").cloned())
        .and_then(|v| serde_json::from_value::<SubagentProfile>(v).ok());
    let status = subagent.and_then(|v| {
        let status = v
            .get("status")
            .cloned()
            .and_then(|s| serde_json::from_value::<SubagentStatus>(s).ok())?;
        let detail = v
            .get("detail")
            .and_then(Value::as_str)
            .map(|d| truncate(d, 1024));
        Some((status, detail))
    });
    let fork_message = match payload.get("fork_origin").and_then(Value::as_str) {
        Some(origin) => db.collab_origin_message(thread_id, origin).await?,
        None => None,
    };
    let title = truncate(&text_of(payload, "title"), 110);
    let title = if title.trim().is_empty() {
        "Shared subthread".to_owned()
    } else {
        title
    };
    let root = service
        .family_root(thread_id)
        .await
        .unwrap_or_else(|| thread_id.clone());
    let child = service
        .sessions()
        .create_mirrored_child(thread_id, kind, title, profile, fork_message)
        .await?;
    db.link_remote_child(&child.id, &root, url, collab_child)
        .await?;
    if let Some(link) = db.collab_link(&child.id).await? {
        service.ensure_mirror(&link, connection.generation);
    }
    Ok(Some((child.id, status)))
}
