//! Delivery of the durable publish outbox (migration 0011, ADR 0013).
//!
//! Rows go out in `id` order. Consecutive history, changes and status rows
//! of one thread travel in one `publish` batch (one mutation for the
//! service's rate limit). A transport failure, rate limit or expired session
//! leaves the row pending and retries it with backoff, stopping the queue so
//! order is kept; a rejection (404, 403, 400, 409, 413) fails only that row.
//! Every request carries the row's stored command id, so a retry after a
//! lost reply is answered with the original receipt instead of a duplicate.

use super::family::job;
use super::history::{self, activity_parts, origin_of};
use super::http::CollabError;
use super::{
    CollabService, Connection, MAX_ANCHOR_BYTES, MAX_COMMENT_BYTES, sanitize_text, truncate,
};
use ::http::Method;
use chrono::Utc;
use serde_json::{Value, json};
use std::time::Duration;
use trama_collab_protocol::{
    ChildCreated, CommandReceipt, CommentRequest, CreateChildRequest, MAX_PUBLISH_EVENTS,
    PublishEvent, PublishKind, PublishRequest, PublishResponse, ResolveRequest, Verdict,
    VerdictRequest,
};
use trama_db::OutboxRow;
use trama_domain::ids::{CommentId, MessageId, ThreadId};

/// How long `SendPrompt` waits for its thread's backlog before submitting.
pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_secs(15);
const BATCH_ROWS: usize = 32;
const MAX_BATCH_BYTES: usize = 900 * 1024;
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const IDLE_WAIT: Duration = Duration::from_secs(5);

enum Outcome {
    Delivered,
    /// Try again later (service unreachable, rate limited, signed out).
    Retry(String),
    /// Never deliverable; skip it.
    Failed(String),
}

fn classify(service: &CollabService, connection: &Connection, error: CollabError) -> Outcome {
    match error {
        CollabError::Transport(_) | CollabError::Limited(_) => Outcome::Retry(error.to_string()),
        CollabError::Unauthorized => {
            service.session_rejected(connection.generation);
            Outcome::Retry(error.to_string())
        }
        CollabError::NotFound | CollabError::Forbidden | CollabError::Rejected(..) => {
            Outcome::Failed(error.to_string())
        }
    }
}

fn backoff(attempts: i64) -> Duration {
    let factor = 1u32 << attempts.clamp(0, 8) as u32;
    (MIN_BACKOFF * factor).min(MAX_BACKOFF)
}

fn is_batched(kind: &str) -> bool {
    matches!(kind, job::MESSAGE | job::CHANGES | job::STATUS)
}

impl CollabService {
    /// Deliver pending rows while this generation's session lasts.
    pub(super) async fn run_outbox(self, generation: u64) {
        loop {
            let Some(connection) = self.connection(Some(generation)) else {
                return;
            };
            if connection.session_token.is_none() {
                return;
            }
            let rows = match self.db().collab_outbox_pending(BATCH_ROWS).await {
                Ok(rows) => rows,
                Err(err) => {
                    crate::log::warn(format!("collab outbox unreadable: {err}"));
                    self.wait_outbox(IDLE_WAIT).await;
                    continue;
                }
            };
            let Some(head) = rows.first() else {
                self.wait_outbox(IDLE_WAIT).await;
                continue;
            };
            let due = head.next_attempt_at - Utc::now();
            if due > chrono::Duration::zero() {
                self.wait_outbox(due.to_std().unwrap_or(MIN_BACKOFF)).await;
                continue;
            }
            let batch: Vec<&OutboxRow> = if is_batched(&head.job.job) {
                rows.iter()
                    .take_while(|row| {
                        is_batched(&row.job.job) && row.job.thread_id == head.job.thread_id
                    })
                    .collect()
            } else {
                vec![head]
            };
            let results = self.deliver(&connection, &batch).await;
            let mut stop = false;
            for (row, outcome) in results {
                match outcome {
                    Outcome::Delivered => {
                        let _ = self.db().collab_outbox_delivered(&[row.id]).await;
                    }
                    Outcome::Failed(reason) => {
                        crate::log::warn(format!(
                            "shared-thread item not published ({}): {reason}",
                            row.job.job
                        ));
                        let _ = self.db().collab_outbox_failed(row.id, &reason).await;
                    }
                    Outcome::Retry(reason) => {
                        let at = Utc::now()
                            + chrono::Duration::from_std(backoff(row.attempts))
                                .unwrap_or_else(|_| chrono::Duration::seconds(1));
                        let _ = self.db().collab_outbox_retry_at(row.id, at, &reason).await;
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                // Order matters: nothing after a row that must be retried.
                continue;
            }
        }
    }

    async fn wait_outbox(&self, at_most: Duration) {
        let _ = tokio::time::timeout(at_most, self.inner.outbox_notify.notified()).await;
    }

    /// Wake the outbox worker (new rows, or a reconnect).
    pub(crate) fn kick_outbox(&self) {
        self.inner.outbox_notify.notify_one();
    }

    /// Deliver `rows`; returns an outcome per handled row, in order (a
    /// prefix: rows after a retry are not attempted).
    async fn deliver<'a>(
        &self,
        connection: &Connection,
        rows: &[&'a OutboxRow],
    ) -> Vec<(&'a OutboxRow, Outcome)> {
        let mut out = Vec::new();
        if rows.len() > 1 || is_batched(&rows[0].job.job) {
            // Build each row's events, then send them in as few batches as
            // the size bounds allow.
            let Some(collab) = history::collab_id(self.db(), &rows[0].job.thread_id).await else {
                for row in rows {
                    out.push((*row, Outcome::Failed("the thread is not shared".into())));
                }
                return out;
            };
            let mut pending: Vec<(&OutboxRow, Vec<PublishEvent>)> = Vec::new();
            let mut size = 0usize;
            let mut count = 0usize;
            for row in rows {
                let events = match self.publish_events(row).await {
                    Ok(events) => events,
                    Err(reason) => {
                        if pending.is_empty() {
                            out.push((*row, Outcome::Failed(reason)));
                            continue;
                        }
                        break;
                    }
                };
                let bytes: usize = events
                    .iter()
                    .map(|e| serde_json::to_vec(&e.payload).map(|v| v.len()).unwrap_or(0) + 128)
                    .sum();
                if !pending.is_empty()
                    && (size + bytes > MAX_BATCH_BYTES || count + events.len() > MAX_PUBLISH_EVENTS)
                {
                    break;
                }
                size += bytes;
                count += events.len();
                pending.push((*row, events));
            }
            if pending.is_empty() {
                return out;
            }
            let sent = self.send_batch(connection, &collab, &pending).await;
            match sent {
                Outcome::Failed(reason) if pending.len() > 1 => {
                    // One bad row must not sink its neighbours: send each
                    // alone.
                    let _ = reason;
                    for (row, events) in &pending {
                        let outcome = self
                            .send_batch(connection, &collab, &[(*row, events.clone())])
                            .await;
                        let retry = matches!(outcome, Outcome::Retry(_));
                        out.push((*row, outcome));
                        if retry {
                            break;
                        }
                    }
                }
                outcome => {
                    let retry = matches!(outcome, Outcome::Retry(_));
                    let first = pending[0].0;
                    if retry {
                        out.push((first, outcome));
                    } else {
                        let failed = match &outcome {
                            Outcome::Failed(reason) => Some(reason.clone()),
                            _ => None,
                        };
                        for (row, _) in &pending {
                            out.push((
                                *row,
                                match &failed {
                                    Some(reason) => Outcome::Failed(reason.clone()),
                                    None => Outcome::Delivered,
                                },
                            ));
                        }
                    }
                }
            }
            return out;
        }
        let row = rows[0];
        let outcome = self.deliver_single(connection, row).await;
        out.push((row, outcome));
        out
    }

    async fn send_batch(
        &self,
        connection: &Connection,
        collab: &str,
        pending: &[(&OutboxRow, Vec<PublishEvent>)],
    ) -> Outcome {
        let events: Vec<PublishEvent> = pending
            .iter()
            .flat_map(|(_, events)| events.iter().cloned())
            .collect();
        if events.is_empty() {
            return Outcome::Delivered;
        }
        let Ok(session) = connection.session() else {
            return Outcome::Retry("signed out".into());
        };
        match connection
            .endpoint
            .call::<PublishResponse>(
                Method::POST,
                &format!("/threads/{collab}/publish"),
                Some(session),
                Some(&PublishRequest { events }),
            )
            .await
        {
            Ok(_) => Outcome::Delivered,
            Err(error) => classify(self, connection, error),
        }
    }

    /// The events one batched row publishes.
    async fn publish_events(
        &self,
        row: &OutboxRow,
    ) -> std::result::Result<Vec<PublishEvent>, String> {
        let command_id = row.job.command_id.clone();
        match row.job.job.as_str() {
            job::MESSAGE => {
                let id = MessageId::from(row.job.ref_id.clone().unwrap_or_default());
                let message = self
                    .db()
                    .get_message(&id)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("the message no longer exists")?;
                let origin = origin_of(self.db(), &message.id).await;
                let items = self
                    .db()
                    .turn_activity_for_message(&message.id)
                    .await
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                let parts = activity_parts(&items);
                let mut events = Vec::with_capacity(parts.len() + 1);
                for (index, part) in parts.iter().enumerate() {
                    events.push(PublishEvent {
                        command_id: format!("{command_id}.a{index}"),
                        kind: PublishKind::HistoryActivity,
                        payload: json!({
                            "origin": origin,
                            "part": index,
                            "parts": parts.len(),
                            "items": part,
                        }),
                    });
                }
                events.push(PublishEvent {
                    command_id,
                    kind: PublishKind::HistoryMessage,
                    payload: history::message_payload(self.db(), &message, &origin, parts.len())
                        .await,
                });
                Ok(events)
            }
            job::CHANGES | job::STATUS => {
                let payload: Value = row
                    .job
                    .payload_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .ok_or("the captured payload is missing")?;
                let kind = if row.job.job == job::CHANGES {
                    PublishKind::ChangesSnapshot
                } else {
                    PublishKind::SubagentStatus
                };
                Ok(vec![PublishEvent {
                    command_id,
                    kind,
                    payload,
                }])
            }
            other => Err(format!("{other} is not a batched job")),
        }
    }

    async fn deliver_single(&self, connection: &Connection, row: &OutboxRow) -> Outcome {
        let Ok(session) = connection.session().map(str::to_owned) else {
            return Outcome::Retry("signed out".into());
        };
        let command_id = row.job.command_id.clone();
        let thread_id = &row.job.thread_id;
        match row.job.job.as_str() {
            job::CHILD => {
                let Some(parent) = row.job.ref_id.as_deref().map(ThreadId::from) else {
                    return Outcome::Failed("no parent".into());
                };
                let Some(parent_collab) = history::collab_id(self.db(), &parent).await else {
                    return Outcome::Failed("the parent thread is not shared".into());
                };
                let Ok(Some(thread)) = self.db().get_thread(thread_id).await else {
                    return Outcome::Failed("the thread no longer exists".into());
                };
                let title = thread
                    .title
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| "Subthread".into());
                let title = sanitize_text(&truncate(&title, 180), 200, "title")
                    .map(|t| t.replace(['\n', '\t'], " "))
                    .unwrap_or_else(|_| "Subthread".into());
                let kind = serde_json::to_value(thread.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "sub".into());
                let subagent =
                    self.db()
                        .get_subagent(thread_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|record| {
                            json!({
                                "profile": record.spawned_by.profile,
                                "status": record.status,
                                "detail": record.detail.as_deref().map(|d| truncate(d, 512)),
                            })
                        });
                let fork_origin = match self.db().get_subthread_fork(thread_id).await {
                    Ok(Some(fork)) => match fork.fork_message_id {
                        Some(message) => Some(origin_of(self.db(), &message).await),
                        None => None,
                    },
                    _ => None,
                };
                let request = CreateChildRequest {
                    command_id,
                    title,
                    kind,
                    subagent,
                    fork_origin,
                };
                match connection
                    .endpoint
                    .call::<ChildCreated>(
                        Method::POST,
                        &format!("/threads/{parent_collab}/children"),
                        Some(&session),
                        Some(&request),
                    )
                    .await
                {
                    Ok(created) => {
                        match self
                            .db()
                            .link_collab_thread(
                                thread_id,
                                connection.endpoint.url(),
                                &created.thread.thread_id,
                            )
                            .await
                        {
                            Ok(link) => {
                                // Its own log starts here; everything in it so
                                // far is this daemon's.
                                self.ensure_mirror(&link, connection.generation);
                                Outcome::Delivered
                            }
                            Err(err) => Outcome::Failed(err.to_string()),
                        }
                    }
                    Err(error) => classify(self, connection, error),
                }
            }
            job::COMMENT => self.deliver_comment(connection, &session, row).await,
            job::RESOLVE | job::REOPEN | job::DELETE_COMMENT => {
                let Some(comment) = row.job.ref_id.as_deref().map(CommentId::from) else {
                    return Outcome::Failed("no comment".into());
                };
                let Ok(Some((collab, seq))) = self.db().collab_comment_seq(&comment).await else {
                    return Outcome::Failed("the comment was never published".into());
                };
                let action = match row.job.job.as_str() {
                    job::RESOLVE => "resolve",
                    job::REOPEN => "reopen",
                    _ => "delete",
                };
                match connection
                    .endpoint
                    .call::<CommandReceipt>(
                        Method::POST,
                        &format!("/threads/{collab}/comments/{seq}/{action}"),
                        Some(&session),
                        Some(&ResolveRequest { command_id }),
                    )
                    .await
                {
                    Ok(_) => Outcome::Delivered,
                    Err(error) => classify(self, connection, error),
                }
            }
            job::VERDICT => {
                let Some(collab) = history::collab_id(self.db(), thread_id).await else {
                    return Outcome::Failed("the thread is not shared".into());
                };
                let payload: Value = row
                    .job
                    .payload_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or(Value::Null);
                let verdict = match payload["verdict"].as_str() {
                    Some("approved") => Verdict::Approved,
                    Some("changes_requested") => Verdict::ChangesRequested,
                    _ => Verdict::Commented,
                };
                let body = payload["body"]
                    .as_str()
                    .and_then(|b| sanitize_text(b, MAX_COMMENT_BYTES, "verdict").ok());
                match connection
                    .endpoint
                    .call::<CommandReceipt>(
                        Method::POST,
                        &format!("/threads/{collab}/verdicts"),
                        Some(&session),
                        Some(&VerdictRequest {
                            command_id,
                            verdict,
                            body,
                        }),
                    )
                    .await
                {
                    Ok(_) => Outcome::Delivered,
                    Err(error) => classify(self, connection, error),
                }
            }
            other => Outcome::Failed(format!("unknown outbox job {other}")),
        }
    }

    async fn deliver_comment(
        &self,
        connection: &Connection,
        session: &str,
        row: &OutboxRow,
    ) -> Outcome {
        let Some(id) = row.job.ref_id.as_deref().map(CommentId::from) else {
            return Outcome::Failed("no comment".into());
        };
        let Ok(Some(record)) = self.db().get_comment(&id).await else {
            // Deleted before it was published: nothing to share.
            return Outcome::Failed("the comment no longer exists".into());
        };
        let Some(collab) = history::collab_id(self.db(), &record.comment.thread_id).await else {
            return Outcome::Failed("the thread is not shared".into());
        };
        let parent_seq = match &record.comment.parent_id {
            Some(parent) => match self.db().collab_comment_seq(parent).await {
                Ok(Some((cid, seq))) if cid == collab => Some(seq),
                _ => return Outcome::Failed("its comment was never published".into()),
            },
            None => None,
        };
        let Ok(body) = sanitize_text(
            &truncate(&record.comment.body, MAX_COMMENT_BYTES - 8),
            MAX_COMMENT_BYTES,
            "the comment",
        ) else {
            return Outcome::Failed("the comment cannot be shared".into());
        };
        let excerpt = record.comment.excerpt.as_deref().map(|e| truncate(e, 1024));
        let target = history::publish_target(self.db(), &record.comment.target).await;
        let mut anchor = json!({ "target": target, "excerpt": excerpt });
        if anchor.to_string().len() > MAX_ANCHOR_BYTES {
            anchor = json!({ "excerpt": excerpt });
        }
        match connection
            .endpoint
            .call::<CommandReceipt>(
                Method::POST,
                &format!("/threads/{collab}/comments"),
                Some(session),
                Some(&CommentRequest {
                    command_id: row.job.command_id.clone(),
                    body,
                    anchor: Some(anchor),
                    parent_seq,
                }),
            )
            .await
        {
            Ok(receipt) => {
                match self
                    .db()
                    .set_collab_comment_seq(&id, &collab, receipt.seq)
                    .await
                {
                    Ok(()) => Outcome::Delivered,
                    Err(err) => Outcome::Retry(err.to_string()),
                }
            }
            Err(error) => classify(self, connection, error),
        }
    }
}
