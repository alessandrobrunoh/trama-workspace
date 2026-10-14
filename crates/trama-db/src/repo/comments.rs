//! Comment projections: anchored comments, replies, delivery and resolution.

use crate::codec::{from_json, parse_rfc3339, rfc3339, sql_err, to_json};
use crate::projections::{insert_event, insert_message};
use crate::store::TramaDb;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use trama_domain::Result;
use trama_domain::comment::{Comment, CommentDelivery};
use trama_domain::event::EventRecord;
use trama_domain::ids::{CommentId, MessageId, ThreadId};
use trama_domain::message::Message;

/// A stored comment plus its delivery, if it was sent to the agent.
#[derive(Debug, Clone)]
pub struct CommentRecord {
    pub comment: Comment,
    pub delivery: Option<CommentDelivery>,
    /// Set while a pending comment is taken off the next send.
    pub detached_at: Option<DateTime<Utc>>,
}

const COLUMNS: &str = "id, thread_id, parent_comment_id, author_json, target_json, body, excerpt,
     created_at, resolved_at, delivered_message_id, delivered_turn, delivered_at,
     answer_message_id, detached_at";

fn record_from_row(row: &Row<'_>) -> Result<CommentRecord> {
    let get_text = |idx: usize| row.get::<_, String>(idx).map_err(sql_err);
    let get_opt = |idx: usize| row.get::<_, Option<String>>(idx).map_err(sql_err);
    let delivered_message: Option<String> = get_opt(9)?;
    let delivery = match delivered_message {
        Some(message_id) => {
            let turn: i64 = row.get(10).map_err(sql_err)?;
            let delivered_at = get_text(11)?;
            Some(CommentDelivery {
                message_id: MessageId::from(message_id),
                turn: turn as u32,
                delivered_at: parse_rfc3339(&delivered_at)?,
                answer_message_id: get_opt(12)?.map(MessageId::from),
            })
        }
        None => None,
    };
    Ok(CommentRecord {
        comment: Comment {
            id: CommentId::from(get_text(0)?),
            thread_id: ThreadId::from(get_text(1)?),
            parent_id: get_opt(2)?.map(CommentId::from),
            author: from_json(&get_text(3)?)?,
            target: from_json(&get_text(4)?)?,
            body: get_text(5)?,
            excerpt: get_opt(6)?,
            created_at: parse_rfc3339(&get_text(7)?)?,
            resolved_at: get_opt(8)?.map(|raw| parse_rfc3339(&raw)).transpose()?,
        },
        delivery,
        detached_at: get_opt(13)?.map(|raw| parse_rfc3339(&raw)).transpose()?,
    })
}

fn insert_events(tx: &rusqlite::Transaction<'_>, events: &[EventRecord]) -> Result<()> {
    for event in events {
        insert_event(tx, event)?;
    }
    Ok(())
}

impl TramaDb {
    /// Insert a pending comment and its `CommentCreated` event atomically.
    pub async fn create_comment(&self, comment: &Comment, event: &EventRecord) -> Result<()> {
        let comment = comment.clone();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "INSERT INTO thread_comments
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
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn get_comment(&self, id: &CommentId) -> Result<Option<CommentRecord>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            let sql = format!("SELECT {COLUMNS} FROM thread_comments WHERE id = ?1");
            let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
            let mut rows = stmt.query(params![id]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                Some(row) => Ok(Some(record_from_row(row)?)),
                None => Ok(None),
            }
        })
        .await
    }

    /// Every comment of one thread (roots and replies), oldest first.
    pub async fn list_comments(&self, thread_id: &ThreadId) -> Result<Vec<CommentRecord>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let sql = format!(
                "SELECT {COLUMNS} FROM thread_comments WHERE thread_id = ?1
                 ORDER BY created_at ASC, rowid ASC"
            );
            let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
            let mut rows = stmt.query(params![thread_id]).map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                out.push(record_from_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Set (`Some`) or clear (`None`) a comment's resolution.
    pub async fn set_comment_resolved(
        &self,
        id: &CommentId,
        resolved_at: Option<DateTime<Utc>>,
        event: &EventRecord,
    ) -> Result<()> {
        let id = id.to_string();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "UPDATE thread_comments SET resolved_at = ?2 WHERE id = ?1",
                params![id, resolved_at.map(rfc3339)],
            )
            .map_err(sql_err)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Remove comments from the projection (the events keep the history).
    /// Replies must be listed before their parent.
    pub async fn delete_comments(&self, ids: &[CommentId], event: &EventRecord) -> Result<()> {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for id in &ids {
                tx.execute("DELETE FROM thread_comments WHERE id = ?1", params![id])
                    .map_err(sql_err)?;
            }
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Take pending comments off the next send (`Some`) or put them back
    /// (`None`). Delivered comments are left unchanged: their copy on the
    /// prompt that carried them is history.
    pub async fn set_comments_detached(
        &self,
        ids: &[CommentId],
        detached_at: Option<DateTime<Utc>>,
        event: &EventRecord,
    ) -> Result<()> {
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for id in &ids {
                tx.execute(
                    "UPDATE thread_comments SET detached_at = ?2
                     WHERE id = ?1 AND delivered_message_id IS NULL",
                    params![id, detached_at.map(rfc3339)],
                )
                .map_err(sql_err)?;
            }
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Number of user prompts stored on `thread_id` (its own messages only).
    pub async fn count_user_prompts(&self, thread_id: &ThreadId) -> Result<u32> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE thread_id = ?1 AND role = 'user'",
                    params![thread_id],
                    |row| row.get(0),
                )
                .map_err(sql_err)?;
            Ok(count as u32)
        })
        .await
    }

    /// Store the user prompt that carries `ids` and mark those comments
    /// delivered with it (as the thread's `turn`-th prompt), plus `events`,
    /// in one transaction. Already delivered comments are left unchanged.
    pub async fn deliver_comments(
        &self,
        message: &Message,
        ids: &[CommentId],
        turn: u32,
        events: &[EventRecord],
    ) -> Result<()> {
        let message = message.clone();
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let events = events.to_vec();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            insert_message(&tx, &message)?;
            for id in &ids {
                tx.execute(
                    "UPDATE thread_comments
                     SET delivered_message_id = ?2, delivered_turn = ?3, delivered_at = ?4,
                         answer_message_id = NULL
                     WHERE id = ?1 AND delivered_message_id IS NULL AND detached_at IS NULL",
                    params![
                        id,
                        message.id.to_string(),
                        i64::from(turn),
                        rfc3339(message.created_at)
                    ],
                )
                .map_err(sql_err)?;
            }
            insert_events(&tx, &events)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Comments delivered with the user prompt `message_id`.
    pub async fn comments_delivered_with(&self, message_id: &MessageId) -> Result<Vec<CommentId>> {
        let message_id = message_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM thread_comments WHERE delivered_message_id = ?1
                     ORDER BY created_at ASC, rowid ASC",
                )
                .map_err(sql_err)?;
            let ids = stmt
                .query_map(params![message_id], |row| row.get::<_, String>(0))
                .map_err(sql_err)?
                .map(|id| id.map(CommentId::from))
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            Ok(ids)
        })
        .await
    }

    /// Link the comments delivered with `delivered_message_id` to the
    /// assistant message that answered that turn.
    pub async fn record_comment_answer(
        &self,
        delivered_message_id: &MessageId,
        answer_message_id: &MessageId,
        event: &EventRecord,
    ) -> Result<()> {
        let delivered = delivered_message_id.to_string();
        let answer = answer_message_id.to_string();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "UPDATE thread_comments SET answer_message_id = ?2
                 WHERE delivered_message_id = ?1",
                params![delivered, answer],
            )
            .map_err(sql_err)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Make the comments delivered with `delivered_message_id` pending again.
    pub async fn return_comments_to_pending(
        &self,
        delivered_message_id: &MessageId,
        event: &EventRecord,
    ) -> Result<()> {
        let delivered = delivered_message_id.to_string();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "UPDATE thread_comments
                 SET delivered_message_id = NULL, delivered_turn = NULL, delivered_at = NULL,
                     answer_message_id = NULL
                 WHERE delivered_message_id = ?1",
                params![delivered],
            )
            .map_err(sql_err)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Thread a comment belongs to, if it exists.
    pub async fn comment_thread(&self, id: &CommentId) -> Result<Option<ThreadId>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT thread_id FROM thread_comments WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|thread| thread.map(ThreadId::from))
            .map_err(sql_err)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::comment::{CodeAnchor, CommentTarget};
    use trama_domain::event::{Aggregate, DomainEvent, EventStore};
    use trama_domain::ids::RepositoryId;
    use trama_domain::message::Actor;
    use trama_domain::thread::{Thread, ThreadKind};
    use trama_domain::workspace::Workspace;

    fn event(thread_id: &ThreadId, event: DomainEvent) -> EventRecord {
        EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            Actor::Human { id: "user".into() },
            event,
            vec![],
        )
        .unwrap()
    }

    fn comment(thread_id: &ThreadId, parent: Option<&CommentId>, body: &str) -> Comment {
        Comment {
            id: CommentId::generate(),
            thread_id: thread_id.clone(),
            author: Actor::Human { id: "user".into() },
            target: CommentTarget::Code {
                anchor: CodeAnchor {
                    repository_id: RepositoryId::from("repo"),
                    path: "src/lib.rs".into(),
                    revision: 0,
                    start_line: 2,
                    end_line: 3,
                    text_hash: Some("abc".into()),
                    context_hash: None,
                },
            },
            body: body.into(),
            created_at: Utc::now(),
            resolved_at: None,
            parent_id: parent.cloned(),
            excerpt: Some("fn a() {}\nfn b() {}".into()),
        }
    }

    #[tokio::test]
    async fn comment_lifecycle_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let db = TramaDb::open(&path).unwrap();
        let workspace = Workspace::new("comments");
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("t".into()),
            repo_attachments: vec![],
        };
        db.create_thread(&thread).await.unwrap();
        let tid = thread.id.clone();

        let root = comment(&tid, None, "rename this");
        db.create_comment(
            &root,
            &event(
                &tid,
                DomainEvent::CommentCreated {
                    comment: root.clone(),
                },
            ),
        )
        .await
        .unwrap();
        let reply = comment(&tid, Some(&root.id), "and add a test");
        db.create_comment(
            &reply,
            &event(
                &tid,
                DomainEvent::CommentCreated {
                    comment: reply.clone(),
                },
            ),
        )
        .await
        .unwrap();
        let doomed = comment(&tid, None, "never mind");
        db.create_comment(
            &doomed,
            &event(
                &tid,
                DomainEvent::CommentCreated {
                    comment: doomed.clone(),
                },
            ),
        )
        .await
        .unwrap();
        db.delete_comments(
            std::slice::from_ref(&doomed.id),
            &event(
                &tid,
                DomainEvent::CommentsDeleted {
                    thread_id: tid.clone(),
                    comment_ids: vec![doomed.id.clone()],
                },
            ),
        )
        .await
        .unwrap();

        let skipped = comment(&tid, None, "later");
        db.create_comment(
            &skipped,
            &event(
                &tid,
                DomainEvent::CommentCreated {
                    comment: skipped.clone(),
                },
            ),
        )
        .await
        .unwrap();
        let detach = |ids: Vec<CommentId>| {
            event(
                &tid,
                DomainEvent::CommentsDetached {
                    thread_id: tid.clone(),
                    comment_ids: ids,
                },
            )
        };
        db.set_comments_detached(
            std::slice::from_ref(&skipped.id),
            Some(Utc::now()),
            &detach(vec![skipped.id.clone()]),
        )
        .await
        .unwrap();

        let prompt = Message::new_user(tid.clone(), "user", "go");
        let ids = vec![root.id.clone(), reply.id.clone()];
        assert_eq!(db.count_user_prompts(&tid).await.unwrap(), 0);
        db.deliver_comments(
            &prompt,
            &ids,
            1,
            &[event(
                &tid,
                DomainEvent::CommentsDelivered {
                    thread_id: tid.clone(),
                    comment_ids: ids.clone(),
                    message_id: prompt.id.clone(),
                    turn: 1,
                },
            )],
        )
        .await
        .unwrap();
        assert_eq!(db.count_user_prompts(&tid).await.unwrap(), 1);
        assert_eq!(db.comments_delivered_with(&prompt.id).await.unwrap(), ids);
        // Delivered comments cannot be detached.
        db.set_comments_detached(
            std::slice::from_ref(&root.id),
            Some(Utc::now()),
            &detach(vec![root.id.clone()]),
        )
        .await
        .unwrap();
        assert!(
            db.get_comment(&root.id)
                .await
                .unwrap()
                .unwrap()
                .detached_at
                .is_none()
        );
        let answer = MessageId::from("answer");
        db.record_comment_answer(
            &prompt.id,
            &answer,
            &event(
                &tid,
                DomainEvent::CommentsAnswered {
                    thread_id: tid.clone(),
                    delivered_message_id: prompt.id.clone(),
                    answer_message_id: answer.clone(),
                },
            ),
        )
        .await
        .unwrap();
        let resolved_at = Utc::now();
        db.set_comment_resolved(
            &root.id,
            Some(resolved_at),
            &event(
                &tid,
                DomainEvent::CommentResolved {
                    thread_id: tid.clone(),
                    comment_id: root.id.clone(),
                    resolved_at,
                },
            ),
        )
        .await
        .unwrap();
        drop(db);

        let db = TramaDb::open(&path).unwrap();
        let listed = db.list_comments(&tid).await.unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].comment.id, root.id);
        assert!(listed[2].detached_at.is_some());
        assert!(listed[2].delivery.is_none());
        assert_eq!(listed[0].comment.excerpt, root.excerpt);
        assert!(listed[0].comment.resolved_at.is_some());
        assert_eq!(listed[1].comment.parent_id.as_ref(), Some(&root.id));
        let delivery = listed[1].delivery.clone().unwrap();
        assert_eq!(delivery.message_id, prompt.id);
        assert_eq!(delivery.turn, 1);
        assert_eq!(delivery.answer_message_id, Some(answer));
        assert_eq!(
            db.comment_thread(&reply.id).await.unwrap(),
            Some(tid.clone())
        );
        assert!(db.get_comment(&doomed.id).await.unwrap().is_none());

        db.return_comments_to_pending(
            &prompt.id,
            &event(
                &tid,
                DomainEvent::CommentDeliveryFailed {
                    thread_id: tid.clone(),
                    comment_ids: ids.clone(),
                    message_id: prompt.id.clone(),
                },
            ),
        )
        .await
        .unwrap();
        assert!(
            db.get_comment(&reply.id)
                .await
                .unwrap()
                .unwrap()
                .delivery
                .is_none()
        );
        let kinds: Vec<String> = db
            .for_aggregate(&Aggregate::Thread { id: tid }, None, 20)
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                "comment.created",
                "comment.created",
                "comment.created",
                "comment.deleted",
                "comment.created",
                "comment.detached",
                "comment.delivered",
                "comment.detached",
                "comment.answered",
                "comment.resolved",
                "comment.delivery_failed"
            ]
        );
    }
}
