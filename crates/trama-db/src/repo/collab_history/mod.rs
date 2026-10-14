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

mod apply;
mod origins;
mod outbox;

#[cfg(test)]
mod tests;
