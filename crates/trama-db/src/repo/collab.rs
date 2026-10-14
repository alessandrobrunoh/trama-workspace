//! Shared-thread bindings: local thread <-> collab thread links with a durable
//! cursor, the turns this daemon submitted, and published comment sequence
//! numbers. See migration 0006 and ADR 0009.

use crate::codec::{parse_rfc3339, rfc3339, sql_err};
use crate::projections::{insert_event, insert_message};
use crate::store::TramaDb;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};
use trama_domain::event::EventRecord;
use trama_domain::ids::{CommentId, ThreadId};
use trama_domain::message::Message;
use trama_domain::{DomainError, Result};

/// A local thread mirroring one collab thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabLink {
    pub thread_id: ThreadId,
    pub server_url: String,
    pub collab_thread_id: String,
    /// Highest collab event `seq` applied locally.
    pub cursor: i64,
    pub linked_at: DateTime<Utc>,
}

/// A turn this daemon submitted on behalf of its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabTurn {
    pub command_id: String,
    pub thread_id: ThreadId,
    pub collab_thread_id: String,
    pub text: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
}

fn link_from_row(row: &Row<'_>) -> Result<CollabLink> {
    Ok(CollabLink {
        thread_id: ThreadId::from(row.get::<_, String>(0).map_err(sql_err)?),
        server_url: row.get(1).map_err(sql_err)?,
        collab_thread_id: row.get(2).map_err(sql_err)?,
        cursor: row.get(3).map_err(sql_err)?,
        linked_at: parse_rfc3339(&row.get::<_, String>(4).map_err(sql_err)?)?,
    })
}

fn turn_from_row(row: &Row<'_>) -> Result<CollabTurn> {
    Ok(CollabTurn {
        command_id: row.get(0).map_err(sql_err)?,
        thread_id: ThreadId::from(row.get::<_, String>(1).map_err(sql_err)?),
        collab_thread_id: row.get(2).map_err(sql_err)?,
        text: row.get(3).map_err(sql_err)?,
        state: row.get(4).map_err(sql_err)?,
        created_at: parse_rfc3339(&row.get::<_, String>(5).map_err(sql_err)?)?,
    })
}

const LINK_COLUMNS: &str = "thread_id, server_url, collab_thread_id, cursor, linked_at";
const TURN_COLUMNS: &str = "command_id, thread_id, collab_thread_id, text, state, created_at";

impl TramaDb {
    /// Link a local thread to a collab thread. Re-linking the same pair is a
    /// no-op that keeps the cursor; linking a thread (or a collab thread)
    /// that is already bound elsewhere is rejected.
    pub async fn link_collab_thread(
        &self,
        thread_id: &ThreadId,
        server_url: &str,
        collab_thread_id: &str,
    ) -> Result<CollabLink> {
        let thread_id = thread_id.to_string();
        let server_url = server_url.to_owned();
        let collab_thread_id = collab_thread_id.to_owned();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let existing = tx
                .query_row(
                    &format!(
                        "SELECT {LINK_COLUMNS} FROM collab_links
                         WHERE thread_id = ?1 OR (server_url = ?2 AND collab_thread_id = ?3)"
                    ),
                    params![thread_id, server_url, collab_thread_id],
                    |row| Ok(link_from_row(row)),
                )
                .optional()
                .map_err(sql_err)?
                .transpose()?;
            if let Some(link) = existing {
                if link.thread_id.0 == thread_id
                    && link.server_url == server_url
                    && link.collab_thread_id == collab_thread_id
                {
                    return Ok(link);
                }
                return Err(DomainError::Invalid(
                    "this thread or shared thread is already linked to another one".into(),
                ));
            }
            let now = Utc::now();
            tx.execute(
                "INSERT INTO collab_links (thread_id, server_url, collab_thread_id, cursor, linked_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![thread_id, server_url, collab_thread_id, rfc3339(now)],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
            Ok(CollabLink {
                thread_id: ThreadId::from(thread_id),
                server_url,
                collab_thread_id,
                cursor: 0,
                linked_at: now,
            })
        })
        .await
    }

    pub async fn unlink_collab_thread(&self, thread_id: &ThreadId) -> Result<bool> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let changed = conn
                .execute("DELETE FROM collab_links WHERE thread_id = ?1", [thread_id])
                .map_err(sql_err)?;
            Ok(changed > 0)
        })
        .await
    }

    pub async fn collab_link(&self, thread_id: &ThreadId) -> Result<Option<CollabLink>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                &format!("SELECT {LINK_COLUMNS} FROM collab_links WHERE thread_id = ?1"),
                [thread_id],
                |row| Ok(link_from_row(row)),
            )
            .optional()
            .map_err(sql_err)?
            .transpose()
        })
        .await
    }

    pub async fn collab_link_by_remote(
        &self,
        server_url: &str,
        collab_thread_id: &str,
    ) -> Result<Option<CollabLink>> {
        let server_url = server_url.to_owned();
        let collab_thread_id = collab_thread_id.to_owned();
        self.blocking(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {LINK_COLUMNS} FROM collab_links
                     WHERE server_url = ?1 AND collab_thread_id = ?2"
                ),
                params![server_url, collab_thread_id],
                |row| Ok(link_from_row(row)),
            )
            .optional()
            .map_err(sql_err)?
            .transpose()
        })
        .await
    }

    /// Links to `server_url`, oldest first.
    pub async fn list_collab_links(&self, server_url: &str) -> Result<Vec<CollabLink>> {
        let server_url = server_url.to_owned();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {LINK_COLUMNS} FROM collab_links WHERE server_url = ?1
                     ORDER BY linked_at ASC, rowid ASC"
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([server_url], |row| Ok(link_from_row(row)))
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(sql_err)??);
            }
            Ok(out)
        })
        .await
    }

    /// Apply one collab event to a linked thread exactly once: when `seq` is
    /// above the stored cursor, insert `messages` and `events` and advance the
    /// cursor in one transaction. Returns `false` (and writes nothing) for an
    /// event at or below the cursor, or when the thread is no longer linked.
    pub async fn apply_collab_event(
        &self,
        thread_id: &ThreadId,
        seq: i64,
        messages: &[Message],
        events: &[EventRecord],
    ) -> Result<bool> {
        let thread_id = thread_id.to_string();
        let messages = messages.to_vec();
        let events = events.to_vec();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let advanced = tx
                .execute(
                    "UPDATE collab_links SET cursor = ?2 WHERE thread_id = ?1 AND cursor < ?2",
                    params![thread_id, seq],
                )
                .map_err(sql_err)?;
            if advanced == 0 {
                return Ok(false);
            }
            for message in &messages {
                insert_message(&tx, message)?;
            }
            for event in &events {
                insert_event(&tx, event)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(true)
        })
        .await
    }

    pub async fn insert_collab_turn(&self, turn: &CollabTurn) -> Result<()> {
        let turn = turn.clone();
        self.blocking(move |conn| {
            let now = rfc3339(Utc::now());
            conn.execute(
                "INSERT INTO collab_turns
                    (command_id, thread_id, collab_thread_id, text, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    turn.command_id,
                    turn.thread_id.to_string(),
                    turn.collab_thread_id,
                    turn.text,
                    turn.state,
                    rfc3339(turn.created_at),
                    now
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn collab_turn(&self, command_id: &str) -> Result<Option<CollabTurn>> {
        let command_id = command_id.to_owned();
        self.blocking(move |conn| {
            conn.query_row(
                &format!("SELECT {TURN_COLUMNS} FROM collab_turns WHERE command_id = ?1"),
                [command_id],
                |row| Ok(turn_from_row(row)),
            )
            .optional()
            .map_err(sql_err)?
            .transpose()
        })
        .await
    }

    /// Move a turn to `to` only if it is currently in one of `from`.
    /// Returns whether the row changed.
    pub async fn transition_collab_turn(
        &self,
        command_id: &str,
        from: &[&str],
        to: &str,
    ) -> Result<bool> {
        let command_id = command_id.to_owned();
        let from: Vec<String> = from.iter().map(|s| (*s).to_owned()).collect();
        let to = to.to_owned();
        self.blocking(move |conn| {
            let placeholders = (0..from.len())
                .map(|i| format!("?{}", i + 4))
                .collect::<Vec<_>>()
                .join(", ");
            if placeholders.is_empty() {
                return Ok(false);
            }
            let sql = format!(
                "UPDATE collab_turns SET state = ?2, updated_at = ?3
                 WHERE command_id = ?1 AND state IN ({placeholders})"
            );
            let mut values: Vec<String> = vec![command_id, to, rfc3339(Utc::now())];
            values.extend(from);
            let changed = conn
                .execute(&sql, rusqlite::params_from_iter(values.iter()))
                .map_err(sql_err)?;
            Ok(changed > 0)
        })
        .await
    }

    /// Turns in any of `states`, oldest first.
    pub async fn collab_turns_in_states(&self, states: &[&str]) -> Result<Vec<CollabTurn>> {
        let states: Vec<String> = states.iter().map(|s| (*s).to_owned()).collect();
        self.blocking(move |conn| {
            if states.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = (1..=states.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {TURN_COLUMNS} FROM collab_turns WHERE state IN ({placeholders})
                     ORDER BY created_at ASC, rowid ASC"
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(states.iter()), |row| {
                    Ok(turn_from_row(row))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(sql_err)??);
            }
            Ok(out)
        })
        .await
    }

    pub async fn set_collab_comment_seq(
        &self,
        comment_id: &CommentId,
        collab_thread_id: &str,
        seq: i64,
    ) -> Result<()> {
        let comment_id = comment_id.to_string();
        let collab_thread_id = collab_thread_id.to_owned();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO collab_comments (comment_id, collab_thread_id, seq)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(comment_id) DO UPDATE SET collab_thread_id = ?2, seq = ?3",
                params![comment_id, collab_thread_id, seq],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// `(collab_thread_id, seq)` of a published comment.
    pub async fn collab_comment_seq(
        &self,
        comment_id: &CommentId,
    ) -> Result<Option<(String, i64)>> {
        let comment_id = comment_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT collab_thread_id, seq FROM collab_comments WHERE comment_id = ?1",
                [comment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// Write a daemon-wide value.
    pub async fn set_collab_setting(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO collab_settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(sql_err)
        })
        .await
    }

    /// Read a daemon-wide value, inserting `default` first when it is absent.
    pub async fn collab_setting_or_insert(&self, key: &str, default: &str) -> Result<String> {
        let key = key.to_owned();
        let default = default.to_owned();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO collab_settings (key, value) VALUES (?1, ?2)",
                params![key, default],
            )
            .map_err(sql_err)?;
            conn.query_row(
                "SELECT value FROM collab_settings WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .map_err(sql_err)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::event::{Aggregate, DomainEvent};
    use trama_domain::message::Actor;
    use trama_domain::thread::{Thread, ThreadKind};
    use trama_domain::workspace::Workspace;

    async fn thread(db: &TramaDb) -> ThreadId {
        let workspace = Workspace::new("collab");
        db.create_workspace(&workspace).await.unwrap();
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("shared".into()),
            repo_attachments: vec![],
        };
        db.create_thread(&thread).await.unwrap();
        thread.id
    }

    fn remote_message(thread_id: &ThreadId, text: &str) -> (Message, EventRecord) {
        let message = Message::new_user(thread_id.clone(), "collab:bob", text);
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            Actor::Human {
                id: "collab:bob".into(),
            },
            DomainEvent::MessageAdded {
                message: message.clone(),
            },
            vec![],
        )
        .unwrap();
        (message, event)
    }

    #[tokio::test]
    async fn collab_events_apply_once_and_cursor_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let db = TramaDb::open(&path).unwrap();
        let thread_id = thread(&db).await;
        let (message, event) = remote_message(&thread_id, "hello");
        // Not linked yet: nothing is applied.
        assert!(
            !db.apply_collab_event(&thread_id, 1, &[message.clone()], &[event.clone()])
                .await
                .unwrap()
        );
        db.link_collab_thread(&thread_id, "http://127.0.0.1:1", "ct1")
            .await
            .unwrap();
        assert!(
            db.apply_collab_event(&thread_id, 1, &[message.clone()], &[event.clone()])
                .await
                .unwrap()
        );
        // A redelivered seq writes nothing, even with fresh ids.
        let (again, again_event) = remote_message(&thread_id, "hello");
        assert!(
            !db.apply_collab_event(&thread_id, 1, &[again], &[again_event])
                .await
                .unwrap()
        );
        assert!(
            db.apply_collab_event(&thread_id, 2, &[], &[])
                .await
                .unwrap()
        );
        drop(db);

        let db = TramaDb::open(&path).unwrap();
        let link = db.collab_link(&thread_id).await.unwrap().unwrap();
        assert_eq!(link.cursor, 2);
        assert_eq!(
            db.list_messages_by_thread(&thread_id).await.unwrap().len(),
            1
        );
        // Relinking the same pair keeps the cursor; another pair is rejected.
        let same = db
            .link_collab_thread(&thread_id, "http://127.0.0.1:1", "ct1")
            .await
            .unwrap();
        assert_eq!(same.cursor, 2);
        assert!(
            db.link_collab_thread(&thread_id, "http://127.0.0.1:1", "ct2")
                .await
                .is_err()
        );
        assert_eq!(
            db.collab_link_by_remote("http://127.0.0.1:1", "ct1")
                .await
                .unwrap()
                .unwrap()
                .thread_id,
            thread_id
        );
        assert_eq!(
            db.list_collab_links("http://127.0.0.1:1")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(db.unlink_collab_thread(&thread_id).await.unwrap());
        assert!(db.collab_link(&thread_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn collab_turn_transitions_are_conditional() {
        let db = TramaDb::open_in_memory().unwrap();
        let thread_id = thread(&db).await;
        db.insert_collab_turn(&CollabTurn {
            command_id: "trd.x.1".into(),
            thread_id: thread_id.clone(),
            collab_thread_id: "ct1".into(),
            text: "run".into(),
            state: "submitted".into(),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
        assert!(
            db.transition_collab_turn("trd.x.1", &["submitted"], "claiming")
                .await
                .unwrap()
        );
        assert!(
            !db.transition_collab_turn("trd.x.1", &["submitted"], "claiming")
                .await
                .unwrap()
        );
        let stale = db
            .collab_turns_in_states(&["claiming", "claimed"])
            .await
            .unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].text, "run");
        assert!(
            db.transition_collab_turn("trd.x.1", &["claiming", "claimed"], "ambiguous")
                .await
                .unwrap()
        );
        assert_eq!(
            db.collab_turn("trd.x.1").await.unwrap().unwrap().state,
            "ambiguous"
        );
        assert!(db.collab_turn("missing").await.unwrap().is_none());

        let comment = CommentId::from("c1");
        db.set_collab_comment_seq(&comment, "ct1", 7).await.unwrap();
        assert_eq!(
            db.collab_comment_seq(&comment).await.unwrap(),
            Some(("ct1".into(), 7))
        );
        let first = db.collab_setting_or_insert("install", "a").await.unwrap();
        let second = db.collab_setting_or_insert("install", "b").await.unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("a", "a"));
    }
}
