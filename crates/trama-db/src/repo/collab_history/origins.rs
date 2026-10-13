//! Collab families and message/comment origins.

use super::*;

impl TramaDb {
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
}
