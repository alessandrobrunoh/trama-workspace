//! Shapes of the history a shared thread publishes (ADR 0013) and the
//! translation of local ids to ids every participant understands.
//!
//! - A message is known in the shared log by its *origin key*: its own id
//!   when this daemon published it, `cmd:<command id>` for a shared turn's
//!   prompt, or the publisher's key for a mirrored message
//!   (`collab_message_origins`).
//! - A thread is known by its collab thread id (`collab_links`).

use super::truncate;
use serde_json::{Value, json};
use trama_db::TramaDb;
use trama_domain::activity::ActivityItem;
use trama_domain::comment::CommentTarget;
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::message::{ContentBlock, Message, MessageAnnotation, Role};

/// Activity parts stay well under the service's 192 KiB payload limit.
const MAX_PART_BYTES: usize = 150 * 1024;
const MAX_TEXT_BLOCK_BYTES: usize = 96 * 1024;
const MAX_TOOL_BLOCK_BYTES: usize = 8 * 1024;
const MAX_CONTENT_BYTES: usize = 160 * 1024;

pub(crate) fn origin_for_prompt(command_id: &str) -> String {
    format!("cmd:{command_id}")
}

/// The key `message_id` is known by in the shared log.
pub(crate) async fn origin_of(db: &TramaDb, message_id: &MessageId) -> String {
    db.collab_message_origin(message_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| message_id.to_string())
}

/// Collab thread id of a local thread, if linked.
pub(crate) async fn collab_id(db: &TramaDb, thread_id: &ThreadId) -> Option<String> {
    db.collab_link(thread_id)
        .await
        .ok()
        .flatten()
        .map(|link| link.collab_thread_id)
}

/// Local thread linked to a collab thread id on `server_url`.
pub(crate) async fn local_thread(db: &TramaDb, server_url: &str, collab: &str) -> Option<ThreadId> {
    db.collab_link_by_remote(server_url, collab)
        .await
        .ok()
        .flatten()
        .map(|link| link.thread_id)
}

/// Thread references of an annotation rewritten to collab ids. A thread
/// that is not shared drops the annotation (the text stays).
pub(crate) async fn publish_annotation(
    db: &TramaDb,
    annotation: &MessageAnnotation,
) -> Option<MessageAnnotation> {
    let map = |id: &ThreadId| {
        let id = id.clone();
        async move { collab_id(db, &id).await.map(ThreadId::from) }
    };
    Some(match annotation {
        MessageAnnotation::AgentMessageSent {
            to_thread_id,
            to_title,
        } => MessageAnnotation::AgentMessageSent {
            to_thread_id: map(to_thread_id).await?,
            to_title: to_title.clone(),
        },
        MessageAnnotation::AgentMessageReceived {
            from_thread_id,
            from_title,
        } => MessageAnnotation::AgentMessageReceived {
            from_thread_id: map(from_thread_id).await?,
            from_title: from_title.clone(),
        },
        MessageAnnotation::SubagentSpawned {
            child_thread_id,
            title,
            profile,
        } => MessageAnnotation::SubagentSpawned {
            child_thread_id: map(child_thread_id).await?,
            title: title.clone(),
            profile: *profile,
        },
        MessageAnnotation::SubagentFinished {
            child_thread_id,
            title,
            status,
        } => MessageAnnotation::SubagentFinished {
            child_thread_id: map(child_thread_id).await?,
            title: title.clone(),
            status: *status,
        },
    })
}

/// The reverse of [`publish_annotation`]: collab ids to local threads.
pub(crate) async fn mirror_annotation(
    db: &TramaDb,
    server_url: &str,
    annotation: MessageAnnotation,
) -> Option<MessageAnnotation> {
    let map = |id: ThreadId| async move { local_thread(db, server_url, &id.0).await };
    Some(match annotation {
        MessageAnnotation::AgentMessageSent {
            to_thread_id,
            to_title,
        } => MessageAnnotation::AgentMessageSent {
            to_thread_id: map(to_thread_id).await?,
            to_title,
        },
        MessageAnnotation::AgentMessageReceived {
            from_thread_id,
            from_title,
        } => MessageAnnotation::AgentMessageReceived {
            from_thread_id: map(from_thread_id).await?,
            from_title,
        },
        MessageAnnotation::SubagentSpawned {
            child_thread_id,
            title,
            profile,
        } => MessageAnnotation::SubagentSpawned {
            child_thread_id: map(child_thread_id).await?,
            title,
            profile,
        },
        MessageAnnotation::SubagentFinished {
            child_thread_id,
            title,
            status,
        } => MessageAnnotation::SubagentFinished {
            child_thread_id: map(child_thread_id).await?,
            title,
            status,
        },
    })
}

/// Content blocks within the payload bounds: long texts are cut, large tool
/// blocks dropped (their activity carries them).
pub(crate) fn bounded_content(content: &[ContentBlock]) -> Vec<ContentBlock> {
    let mut out: Vec<ContentBlock> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(ContentBlock::Text {
                text: truncate(text, MAX_TEXT_BLOCK_BYTES),
            }),
            ContentBlock::Thinking { text } => Some(ContentBlock::Thinking {
                text: truncate(text, MAX_TEXT_BLOCK_BYTES / 4),
            }),
            other => serde_json::to_vec(other)
                .ok()
                .filter(|bytes| bytes.len() <= MAX_TOOL_BLOCK_BYTES)
                .map(|_| other.clone()),
        })
        .collect();
    let size = serde_json::to_vec(&out).map(|v| v.len()).unwrap_or(0);
    if size > MAX_CONTENT_BYTES {
        let text: String = out
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        out = vec![ContentBlock::Text {
            text: truncate(&text, MAX_CONTENT_BYTES / 2),
        }];
    }
    out
}

/// Activity items split into parts below the payload limit. An item that
/// cannot fit alone is dropped.
pub(crate) fn activity_parts(items: &[ActivityItem]) -> Vec<Vec<ActivityItem>> {
    let mut parts: Vec<Vec<ActivityItem>> = Vec::new();
    let mut current: Vec<ActivityItem> = Vec::new();
    let mut size = 0usize;
    for item in items {
        let len = serde_json::to_vec(item)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if len > MAX_PART_BYTES {
            continue;
        }
        if size + len > MAX_PART_BYTES && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
            size = 0;
        }
        size += len + 1;
        current.push(item.clone());
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

pub(crate) fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
    }
}

pub(crate) fn parse_role(label: Option<&str>) -> Role {
    match label {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        _ => Role::System,
    }
}

/// `history_message` payload of a local message.
pub(crate) async fn message_payload(
    db: &TramaDb,
    message: &Message,
    origin: &str,
    activity_parts: usize,
) -> Value {
    let annotation = match &message.annotation {
        Some(annotation) => publish_annotation(db, annotation).await,
        None => None,
    };
    let actor = match &message.actor {
        trama_domain::message::Actor::Human { .. } => "human",
        trama_domain::message::Actor::Agent { .. } => "agent",
        trama_domain::message::Actor::System => "system",
    };
    json!({
        "origin": origin,
        "role": role_label(message.role),
        "actor": actor,
        "content": bounded_content(&message.content),
        "annotation": annotation,
        "created_at": message.created_at,
        "activity_parts": activity_parts,
    })
}

/// A comment target with its message id replaced by the origin key.
pub(crate) async fn publish_target(db: &TramaDb, target: &CommentTarget) -> CommentTarget {
    match target {
        CommentTarget::MessageRange {
            message_id,
            start_byte,
            end_byte,
        } => CommentTarget::MessageRange {
            message_id: MessageId::from(origin_of(db, message_id).await),
            start_byte: *start_byte,
            end_byte: *end_byte,
        },
        other => other.clone(),
    }
}

/// The reverse of [`publish_target`] for `thread_id`. An unknown message
/// keeps the origin key (the comment shows its excerpt, unanchored).
pub(crate) async fn mirror_target(
    db: &TramaDb,
    thread_id: &ThreadId,
    target: CommentTarget,
) -> CommentTarget {
    match target {
        CommentTarget::MessageRange {
            message_id,
            start_byte,
            end_byte,
        } => {
            let local = db
                .collab_origin_message(thread_id, &message_id.0)
                .await
                .ok()
                .flatten()
                .unwrap_or(message_id);
            CommentTarget::MessageRange {
                message_id: local,
                start_byte,
                end_byte,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn activity_is_split_below_the_payload_limit_in_order() {
        let item = |n: usize| ActivityItem::ToolCall {
            tool_use_id: format!("t{n}"),
            name: "Bash".into(),
            input: json!({"command": "x".repeat(30 * 1024)}),
            output: Some(json!("y".repeat(30 * 1024))),
            is_error: false,
            finished: true,
            started_at: Utc::now(),
            finished_at: None,
            truncated: false,
        };
        let items: Vec<_> = (0..10).map(item).collect();
        let parts = activity_parts(&items);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(serde_json::to_vec(part).unwrap().len() <= MAX_PART_BYTES + 64);
        }
        let flat: Vec<_> = parts.into_iter().flatten().collect();
        assert_eq!(flat, items);
    }

    #[test]
    fn content_is_bounded() {
        let big = vec![ContentBlock::Text {
            text: "a".repeat(400 * 1024),
        }];
        let bounded = bounded_content(&big);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= MAX_CONTENT_BYTES);
    }
}
