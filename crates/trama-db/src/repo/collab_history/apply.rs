//! Applying remote collab changes and linking remote children.

use super::*;

impl TramaDb {
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
