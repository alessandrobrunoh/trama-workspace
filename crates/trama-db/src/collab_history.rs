//! Shared-thread history and family sync: the durable publish outbox, the
//! family of a shared root, message origin keys, staged activity parts and
//! other participants' changes snapshots. See migration 0011 and ADR 0013.

use crate::codec::{from_json, parse_rfc3339, rfc3339, sql_err, to_json};
use crate::projections::{insert_event, insert_message};
use crate::store::TramaDb;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use std::collections::HashMap;
use trama_domain::Result;
use trama_domain::activity::ActivityItem;
use trama_domain::comment::Comment;
use trama_domain::event::{Aggregate, DomainEvent, EventRecord};
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_domain::message::{Actor, Message};
use trama_domain::subagent::SubagentStatus;

use crate::ReviewVerdictRecord;

/// One thing to publish. `dedup_key` identifies what is published (so a
/// rescan never enqueues it twice); `command_id` is sent to the service and
/// reused by every retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxJob {
    pub dedup_key: String,
    pub command_id: String,
    pub thread_id: ThreadId,
    pub job: String,
    pub ref_id: Option<String>,
    pub payload_json: Option<String>,
}

/// A pending outbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    pub job: OutboxJob,
    pub attempts: i64,
    pub next_attempt_at: DateTime<Utc>,
}

/// A thread of a shared family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyRow {
    pub thread_id: ThreadId,
    pub root_thread_id: ThreadId,
    /// `local` or `remote`.
    pub origin: String,
}

/// A changes snapshot another participant published for a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteChangesRow {
    pub author: String,
    pub payload_json: String,
    pub captured_at: DateTime<Utc>,
}

/// Everything one mirrored collab event writes, applied with the cursor
/// advance in one transaction (exactly once per event).
#[derive(Debug, Default, Clone)]
pub struct CollabApply {
    /// Messages to insert with their origin key (registered as remote).
    /// Their `created_at` is moved after the thread's last message so the
    /// transcript keeps the shared log's order. A `MessageAdded` event is
    /// written for each.
    pub messages: Vec<(Message, Option<String>)>,
    /// Extra events.
    pub events: Vec<EventRecord>,
    /// Store the staged activity parts of `origin` with the message
    /// `message_id` (one of `messages`), then drop them.
    pub take_activity: Option<(String, MessageId)>,
    /// Stage one activity part: (origin, part, items JSON).
    pub stage_activity: Option<(String, i64, String)>,
    /// Comments to insert, each mapped to (collab thread id, seq).
    pub comments: Vec<(Comment, String, i64)>,
    pub resolved: Vec<(CommentId, Option<DateTime<Utc>>)>,
    pub deleted: Vec<CommentId>,
    pub subagent_status: Option<(ThreadId, SubagentStatus, Option<String>)>,
    /// (author, payload JSON) replacing that author's snapshot.
    pub remote_changes: Option<(String, String)>,
    pub review_verdict: Option<ReviewVerdictRecord>,
}

/// What an applied event wrote that subscribers should see.
#[derive(Debug, Clone, Default)]
pub struct CollabApplied {
    /// The inserted messages, with their final `created_at`.
    pub messages: Vec<Message>,
    /// Activity stored with a message: (message id, items).
    pub activity: Option<(MessageId, Vec<ActivityItem>)>,
}

const OUTBOX_COLUMNS: &str =
    "id, dedup_key, command_id, thread_id, job, ref_id, payload_json, attempts, next_attempt_at";

fn outbox_row(row: &Row<'_>) -> Result<OutboxRow> {
    Ok(OutboxRow {
        id: row.get(0).map_err(sql_err)?,
        job: OutboxJob {
            dedup_key: row.get(1).map_err(sql_err)?,
            command_id: row.get(2).map_err(sql_err)?,
            thread_id: ThreadId::from(row.get::<_, String>(3).map_err(sql_err)?),
            job: row.get(4).map_err(sql_err)?,
            ref_id: row.get(5).map_err(sql_err)?,
            payload_json: row.get(6).map_err(sql_err)?,
        },
        attempts: row.get(7).map_err(sql_err)?,
        next_attempt_at: parse_rfc3339(&row.get::<_, String>(8).map_err(sql_err)?)?,
    })
}

fn family_row(row: &Row<'_>) -> Result<FamilyRow> {
    Ok(FamilyRow {
        thread_id: ThreadId::from(row.get::<_, String>(0).map_err(sql_err)?),
        root_thread_id: ThreadId::from(row.get::<_, String>(1).map_err(sql_err)?),
        origin: row.get(2).map_err(sql_err)?,
    })
}

fn last_message_time(tx: &Transaction<'_>, thread_id: &str) -> Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT created_at FROM messages WHERE thread_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT 1",
            [thread_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    raw.map(|raw| parse_rfc3339(&raw)).transpose()
}

impl TramaDb {
    /// Enqueue `jobs` (skipping any whose dedup key or command id exists)
    /// and record `family` rows (ignoring existing ones), atomically.
    /// Returns the number of new jobs.
    pub async fn enqueue_collab_outbox(
        &self,
        jobs: Vec<OutboxJob>,
        family: Vec<FamilyRow>,
    ) -> Result<usize> {
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let now = rfc3339(Utc::now());
            for row in &family {
                tx.execute(
                    "INSERT OR IGNORE INTO collab_family (thread_id, root_thread_id, origin, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        row.thread_id.to_string(),
                        row.root_thread_id.to_string(),
                        row.origin,
                        now
                    ],
                )
                .map_err(sql_err)?;
            }
            let mut added = 0;
            for job in &jobs {
                added += tx
                    .execute(
                        "INSERT OR IGNORE INTO collab_outbox
                            (dedup_key, command_id, thread_id, job, ref_id, payload_json,
                             next_attempt_at, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                        params![
                            job.dedup_key,
                            job.command_id,
                            job.thread_id.to_string(),
                            job.job,
                            job.ref_id,
                            job.payload_json,
                            now
                        ],
                    )
                    .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(added)
        })
        .await
    }

    /// Whether a job with this dedup key was ever enqueued.
    pub async fn collab_outbox_has(&self, dedup_key: &str) -> Result<bool> {
        let key = dedup_key.to_owned();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT 1 FROM collab_outbox WHERE dedup_key = ?1",
                [key],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(sql_err)
        })
        .await
    }

    /// The oldest pending rows, in delivery order.
    pub async fn collab_outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {OUTBOX_COLUMNS} FROM collab_outbox WHERE state = 'pending'
                     ORDER BY id ASC LIMIT ?1"
                ))
                .map_err(sql_err)?;
            let mut rows = stmt.query([limit as i64]).map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                out.push(outbox_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Pending rows for any of `thread_ids`.
    pub async fn collab_outbox_pending_for(&self, thread_ids: &[ThreadId]) -> Result<usize> {
        let ids: Vec<String> = thread_ids.iter().map(ToString::to_string).collect();
        self.blocking(move |conn| {
            let mut total = 0usize;
            let mut stmt = conn
                .prepare(
                    "SELECT COUNT(*) FROM collab_outbox WHERE state = 'pending' AND thread_id = ?1",
                )
                .map_err(sql_err)?;
            for id in &ids {
                let count: i64 = stmt.query_row([id], |row| row.get(0)).map_err(sql_err)?;
                total += count as usize;
            }
            Ok(total)
        })
        .await
    }

    /// Mark rows delivered; their captured payload is dropped.
    pub async fn collab_outbox_delivered(&self, ids: &[i64]) -> Result<()> {
        let ids = ids.to_vec();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let now = rfc3339(Utc::now());
            for id in &ids {
                tx.execute(
                    "UPDATE collab_outbox SET state = 'delivered', delivered_at = ?2,
                         payload_json = NULL, last_error = NULL
                     WHERE id = ?1",
                    params![id, now],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// A permanent failure: the row is never retried.
    pub async fn collab_outbox_failed(&self, id: i64, error: &str) -> Result<()> {
        let error = error.chars().take(500).collect::<String>();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE collab_outbox SET state = 'failed', last_error = ?2, attempts = attempts + 1
                 WHERE id = ?1",
                params![id, error],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// A transient failure: retry the row (and everything after it) later.
    pub async fn collab_outbox_retry_at(
        &self,
        id: i64,
        at: DateTime<Utc>,
        error: &str,
    ) -> Result<()> {
        let error = error.chars().take(500).collect::<String>();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE collab_outbox SET attempts = attempts + 1, next_attempt_at = ?2,
                     last_error = ?3
                 WHERE id = ?1",
                params![id, rfc3339(at), error],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Count of rows per state (`pending`, `delivered`, `failed`).
    pub async fn collab_outbox_counts(&self) -> Result<HashMap<String, i64>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare("SELECT state, COUNT(*) FROM collab_outbox GROUP BY state")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(sql_err)?;
            let mut out = HashMap::new();
            for row in rows {
                let (state, count) = row.map_err(sql_err)?;
                out.insert(state, count);
            }
            Ok(out)
        })
        .await
    }

    /// Forget what was published for these threads (after an unlink), so
    /// sharing them again publishes everything to the new shared thread.
    pub async fn forget_collab_family(&self, thread_ids: &[ThreadId]) -> Result<()> {
        let ids: Vec<String> = thread_ids.iter().map(ToString::to_string).collect();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for id in &ids {
                tx.execute("DELETE FROM collab_outbox WHERE thread_id = ?1", [id])
                    .map_err(sql_err)?;
                tx.execute("DELETE FROM collab_family WHERE thread_id = ?1", [id])
                    .map_err(sql_err)?;
                tx.execute(
                    "DELETE FROM collab_message_origins WHERE thread_id = ?1 AND remote = 0",
                    [id],
                )
                .map_err(sql_err)?;
                tx.execute("DELETE FROM collab_links WHERE thread_id = ?1", [id])
                    .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn collab_family(&self, thread_id: &ThreadId) -> Result<Option<FamilyRow>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT thread_id, root_thread_id, origin FROM collab_family WHERE thread_id = ?1",
                [thread_id],
                |row| Ok(family_row(row)),
            )
            .optional()
            .map_err(sql_err)?
            .transpose()
        })
        .await
    }

    /// Family rows of `root`, oldest first.
    pub async fn collab_family_of(&self, root: &ThreadId) -> Result<Vec<FamilyRow>> {
        let root = root.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT thread_id, root_thread_id, origin FROM collab_family
                     WHERE root_thread_id = ?1 ORDER BY created_at ASC, rowid ASC",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query([root]).map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                out.push(family_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Origin keys of a thread's messages that have one: message id ->
    /// (origin, remote).
    pub async fn collab_message_origins(
        &self,
        thread_id: &ThreadId,
    ) -> Result<HashMap<MessageId, (String, bool)>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, origin_key, remote FROM collab_message_origins
                     WHERE thread_id = ?1",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([thread_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = HashMap::new();
            for row in rows {
                let (message, origin, remote) = row.map_err(sql_err)?;
                out.insert(MessageId::from(message), (origin, remote != 0));
            }
            Ok(out)
        })
        .await
    }

    /// The origin key a message is known by in the shared log.
    pub async fn collab_message_origin(&self, message_id: &MessageId) -> Result<Option<String>> {
        let message_id = message_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT origin_key FROM collab_message_origins WHERE message_id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// The local message known by `origin` in `thread_id`, if any. A local
    /// message published under its own id has no row: callers also try the
    /// origin as a message id.
    pub async fn collab_origin_message(
        &self,
        thread_id: &ThreadId,
        origin: &str,
    ) -> Result<Option<MessageId>> {
        let thread_id = thread_id.to_string();
        let origin = origin.to_owned();
        self.blocking(move |conn| {
            let mapped: Option<String> = conn
                .query_row(
                    "SELECT message_id FROM collab_message_origins
                     WHERE thread_id = ?1 AND origin_key = ?2",
                    params![thread_id, origin],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some(id) = mapped {
                return Ok(Some(MessageId::from(id)));
            }
            let own: Option<String> = conn
                .query_row(
                    "SELECT id FROM messages WHERE id = ?1 AND thread_id = ?2",
                    params![origin, thread_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            Ok(own.map(MessageId::from))
        })
        .await
    }

    /// Record that a local message is in the shared log as `origin` (this
    /// daemon's shared-turn prompt, `cmd:<command id>`).
    pub async fn set_collab_message_origin(
        &self,
        message_id: &MessageId,
        thread_id: &ThreadId,
        origin: &str,
    ) -> Result<()> {
        let (message_id, thread_id, origin) = (
            message_id.to_string(),
            thread_id.to_string(),
            origin.to_owned(),
        );
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO collab_message_origins
                     (message_id, thread_id, origin_key, remote)
                 VALUES (?1, ?2, ?3, 0)",
                params![message_id, thread_id, origin],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// The local comment published (or mirrored) as `seq` of a collab thread.
    pub async fn collab_comment_by_seq(
        &self,
        collab_thread_id: &str,
        seq: i64,
    ) -> Result<Option<CommentId>> {
        let collab_thread_id = collab_thread_id.to_owned();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT comment_id FROM collab_comments WHERE collab_thread_id = ?1 AND seq = ?2",
                params![collab_thread_id, seq],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|id| id.map(CommentId::from))
            .map_err(sql_err)
        })
        .await
    }

    /// Other participants' changes snapshots of a thread, newest first.
    pub async fn collab_remote_changes(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<RemoteChangesRow>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT author, payload_json, captured_at FROM collab_remote_changes
                     WHERE thread_id = ?1 ORDER BY captured_at DESC",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query([thread_id]).map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                out.push(RemoteChangesRow {
                    author: row.get(0).map_err(sql_err)?,
                    payload_json: row.get(1).map_err(sql_err)?,
                    captured_at: parse_rfc3339(&row.get::<_, String>(2).map_err(sql_err)?)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Apply one mirrored event exactly once: when `seq` is above the
    /// thread's cursor, write `apply` and advance the cursor atomically.
    /// `None` (nothing written) for an event at or below the cursor or an
    /// unlinked thread.
    pub async fn apply_collab_changes(
        &self,
        thread_id: &ThreadId,
        seq: i64,
        apply: CollabApply,
    ) -> Result<Option<CollabApplied>> {
        let thread = thread_id.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let thread_id = thread.to_string();
            let advanced = tx
                .execute(
                    "UPDATE collab_links SET cursor = ?2 WHERE thread_id = ?1 AND cursor < ?2",
                    params![thread_id, seq],
                )
                .map_err(sql_err)?;
            if advanced == 0 {
                return Ok(None);
            }
            let mut applied = CollabApplied::default();
            for (mut message, origin) in apply.messages {
                // Keep the shared log's order: never before the last row.
                if let Some(last) = last_message_time(&tx, &message.thread_id.0)? {
                    if message.created_at <= last {
                        message.created_at = last + Duration::microseconds(1);
                    }
                }
                insert_message(&tx, &message)?;
                if let Some(origin) = &origin {
                    tx.execute(
                        "INSERT OR IGNORE INTO collab_message_origins
                             (message_id, thread_id, origin_key, remote)
                         VALUES (?1, ?2, ?3, 1)",
                        params![message.id.to_string(), message.thread_id.to_string(), origin],
                    )
                    .map_err(sql_err)?;
                }
                let event = EventRecord::new(
                    Aggregate::Thread {
                        id: message.thread_id.clone(),
                    },
                    message.actor.clone(),
                    DomainEvent::MessageAdded {
                        message: message.clone(),
                    },
                    vec![],
                )?;
                insert_event(&tx, &event)?;
                applied.messages.push(message);
            }
            for event in &apply.events {
                insert_event(&tx, event)?;
            }
            if let Some((origin, part, items)) = &apply.stage_activity {
                tx.execute(
                    "INSERT OR REPLACE INTO collab_staged_activity
                         (thread_id, origin_key, part, items_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![thread_id, origin, part, items],
                )
                .map_err(sql_err)?;
            }
            if let Some((origin, message_id)) = &apply.take_activity {
                let mut stmt = tx
                    .prepare(
                        "SELECT items_json FROM collab_staged_activity
                         WHERE thread_id = ?1 AND origin_key = ?2 ORDER BY part ASC",
                    )
                    .map_err(sql_err)?;
                let parts = stmt
                    .query_map(params![thread_id, origin], |row| row.get::<_, String>(0))
                    .map_err(sql_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(sql_err)?;
                drop(stmt);
                let mut items: Vec<ActivityItem> = Vec::new();
                for part in parts {
                    if let Ok(mut chunk) = from_json::<Vec<ActivityItem>>(&part) {
                        items.append(&mut chunk);
                    }
                }
                tx.execute(
                    "DELETE FROM collab_staged_activity WHERE thread_id = ?1 AND origin_key = ?2",
                    params![thread_id, origin],
                )
                .map_err(sql_err)?;
                if !items.is_empty() {
                    tx.execute(
                        "INSERT OR REPLACE INTO turn_activity
                             (message_id, thread_id, items_json, item_count, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            message_id.to_string(),
                            thread_id,
                            to_json(&items)?,
                            items.len() as i64,
                            rfc3339(Utc::now())
                        ],
                    )
                    .map_err(sql_err)?;
                    let event = EventRecord::new(
                        Aggregate::Thread {
                            id: thread.clone(),
                        },
                        Actor::System,
                        DomainEvent::TurnActivityRecorded {
                            thread_id: thread.clone(),
                            message_id: message_id.clone(),
                            items: items.clone(),
                        },
                        vec![],
                    )?;
                    insert_event(&tx, &event)?;
                    applied.activity = Some((message_id.clone(), items));
                }
            }
            for (comment, collab_thread_id, comment_seq) in &apply.comments {
                tx.execute(
                    "INSERT OR IGNORE INTO thread_comments
                        (id, thread_id, parent_comment_id, author_json, target_json, body, excerpt,
                         created_at, resolved_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        comment.id.to_string(),
                        comment.thread_id.to_string(),
                        comment.parent_id.as_ref().map(|id| id.to_string()),
                        to_json(&comment.author)?,
                        to_json(&comment.target)?,
                        comment.body,
                        comment.excerpt,
                        rfc3339(comment.created_at),
                        comment.resolved_at.map(rfc3339),
                    ],
                )
                .map_err(sql_err)?;
                tx.execute(
                    "INSERT OR REPLACE INTO collab_comments (comment_id, collab_thread_id, seq)
                     VALUES (?1, ?2, ?3)",
                    params![comment.id.to_string(), collab_thread_id, comment_seq],
                )
                .map_err(sql_err)?;
            }
            for (comment_id, resolved_at) in &apply.resolved {
                tx.execute(
                    "UPDATE thread_comments SET resolved_at = ?2 WHERE id = ?1",
                    params![comment_id.to_string(), resolved_at.map(rfc3339)],
                )
                .map_err(sql_err)?;
            }
            for comment_id in &apply.deleted {
                tx.execute(
                    "DELETE FROM thread_comments WHERE parent_comment_id = ?1",
                    [comment_id.to_string()],
                )
                .map_err(sql_err)?;
                tx.execute(
                    "DELETE FROM thread_comments WHERE id = ?1",
                    [comment_id.to_string()],
                )
                .map_err(sql_err)?;
            }
            if let Some((child, status, detail)) = &apply.subagent_status {
                tx.execute(
                    "UPDATE subagents SET status = ?2, detail = ?3, updated_at = ?4
                     WHERE thread_id = ?1",
                    params![
                        child.to_string(),
                        crate::codec::enum_text(status)?,
                        detail,
                        rfc3339(Utc::now())
                    ],
                )
                .map_err(sql_err)?;
            }
            if let Some((author, payload)) = &apply.remote_changes {
                tx.execute(
                    "INSERT OR REPLACE INTO collab_remote_changes
                         (thread_id, author, payload_json, captured_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![thread_id, author, payload, rfc3339(Utc::now())],
                )
                .map_err(sql_err)?;
            }
            if let Some(record) = &apply.review_verdict {
                tx.execute(
                    "INSERT OR IGNORE INTO review_verdicts
                        (id, review_thread_id, parent_thread_id, verdict_json, snapshot_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        record.id,
                        record.review_thread_id.to_string(),
                        record.parent_thread_id.to_string(),
                        to_json(&record.verdict)?,
                        to_json(&record.snapshot)?,
                        rfc3339(record.created_at),
                    ],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(Some(applied))
        })
        .await
    }

    /// The stored activity of the turn `message_id` ends.
    pub async fn turn_activity_for_message(
        &self,
        message_id: &MessageId,
    ) -> Result<Option<Vec<ActivityItem>>> {
        let message_id = message_id.to_string();
        self.blocking(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT items_json FROM turn_activity WHERE message_id = ?1",
                    [message_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            Ok(raw.and_then(|raw| from_json::<Vec<ActivityItem>>(&raw).ok()))
        })
        .await
    }

    /// Record a thread created by the mirror as part of `root`'s family and
    /// link it to its collab thread, atomically.
    pub async fn link_remote_child(
        &self,
        thread_id: &ThreadId,
        root: &ThreadId,
        server_url: &str,
        collab_thread_id: &str,
    ) -> Result<()> {
        let (thread_id, root, server_url, collab_thread_id) = (
            thread_id.to_string(),
            root.to_string(),
            server_url.to_owned(),
            collab_thread_id.to_owned(),
        );
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let now = rfc3339(Utc::now());
            tx.execute(
                "INSERT OR IGNORE INTO collab_links
                     (thread_id, server_url, collab_thread_id, cursor, linked_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![thread_id, server_url, collab_thread_id, now],
            )
            .map_err(sql_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO collab_family (thread_id, root_thread_id, origin, created_at)
                 VALUES (?1, ?2, 'remote', ?3)",
                params![thread_id, root, now],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::thread::{Thread, ThreadKind};
    use trama_domain::workspace::Workspace;

    async fn linked_thread(db: &TramaDb) -> ThreadId {
        let workspace = Workspace::new("history");
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("t".into()),
            repo_attachments: vec![],
        };
        db.create_thread(&thread).await.unwrap();
        db.link_collab_thread(&thread.id, "http://127.0.0.1:1", "c1")
            .await
            .unwrap();
        thread.id
    }

    fn job(key: &str, thread: &ThreadId) -> OutboxJob {
        OutboxJob {
            dedup_key: key.into(),
            command_id: format!("cmd-{key}"),
            thread_id: thread.clone(),
            job: "message".into(),
            ref_id: None,
            payload_json: None,
        }
    }

    #[tokio::test]
    async fn outbox_dedups_keeps_order_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let db = TramaDb::open(&path).unwrap();
        let thread = linked_thread(&db).await;
        let added = db
            .enqueue_collab_outbox(vec![job("a", &thread), job("b", &thread)], vec![])
            .await
            .unwrap();
        assert_eq!(added, 2);
        // A rescan enqueues nothing twice.
        let added = db
            .enqueue_collab_outbox(vec![job("a", &thread), job("c", &thread)], vec![])
            .await
            .unwrap();
        assert_eq!(added, 1);
        drop(db);
        let db = TramaDb::open(&path).unwrap();
        let pending = db.collab_outbox_pending(10).await.unwrap();
        let keys: Vec<_> = pending.iter().map(|r| r.job.dedup_key.as_str()).collect();
        assert_eq!(keys, ["a", "b", "c"]);
        db.collab_outbox_retry_at(pending[0].id, Utc::now(), "down")
            .await
            .unwrap();
        db.collab_outbox_delivered(&[pending[0].id]).await.unwrap();
        db.collab_outbox_failed(pending[1].id, "rejected")
            .await
            .unwrap();
        let pending = db.collab_outbox_pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].job.dedup_key, "c");
        let counts = db.collab_outbox_counts().await.unwrap();
        assert_eq!(counts.get("delivered"), Some(&1));
        assert_eq!(counts.get("failed"), Some(&1));
    }

    #[tokio::test]
    async fn applied_messages_keep_log_order_and_apply_once() {
        let db = TramaDb::open_in_memory().unwrap();
        let thread = linked_thread(&db).await;
        let mut first = Message::new_system(thread.clone(), "first");
        first.created_at = Utc::now();
        let mut older = Message::new_system(thread.clone(), "second, stamped earlier");
        older.created_at = first.created_at - Duration::seconds(60);
        let apply = |message: Message, origin: &str| CollabApply {
            messages: vec![(message, Some(origin.to_owned()))],
            ..CollabApply::default()
        };
        assert!(
            db.apply_collab_changes(&thread, 1, apply(first, "o1"))
                .await
                .unwrap()
                .is_some()
        );
        // Staged activity is stored with the message that ends the turn.
        let stage = CollabApply {
            stage_activity: Some((
                "o2".into(),
                0,
                serde_json::to_string(&Vec::<ActivityItem>::new()).unwrap(),
            )),
            ..CollabApply::default()
        };
        db.apply_collab_changes(&thread, 2, stage).await.unwrap();
        let applied = db
            .apply_collab_changes(&thread, 3, apply(older, "o2"))
            .await
            .unwrap()
            .unwrap();
        assert!(applied.messages[0].created_at > Utc::now() - Duration::seconds(5));
        // Replaying an applied seq writes nothing.
        let again = Message::new_system(thread.clone(), "dup");
        assert!(
            db.apply_collab_changes(&thread, 3, apply(again, "o3"))
                .await
                .unwrap()
                .is_none()
        );
        let texts: Vec<String> = db
            .list_messages_by_thread(&thread)
            .await
            .unwrap()
            .iter()
            .map(Message::text)
            .collect();
        assert_eq!(texts, ["first", "second, stamped earlier"]);
        let origins = db.collab_message_origins(&thread).await.unwrap();
        assert_eq!(origins.len(), 2);
        assert!(origins.values().all(|(_, remote)| *remote));
        let id = db.collab_origin_message(&thread, "o2").await.unwrap();
        assert!(id.is_some());
    }
}
