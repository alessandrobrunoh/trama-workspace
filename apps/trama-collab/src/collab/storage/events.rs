//! Event log and idempotent command rows.

use super::*;

pub(crate) fn last_seq(connection: &Connection, thread_id: &str) -> Result<i64, AuthError> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM collab_events WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .map_err(db)
}

/// Appends one event with the next sequence number. Must run inside an
/// IMMEDIATE transaction so concurrent writers cannot pick the same `seq`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn append_event(
    connection: &Connection,
    outbox: &mut Outbox,
    thread_id: &str,
    kind: EventKind,
    author: Option<&Identity>,
    command_id: Option<&str>,
    payload: Value,
    now: i64,
) -> Result<EventEnvelope, AuthError> {
    let seq = last_seq(connection, thread_id)? + 1;
    connection
        .execute(
            "INSERT INTO collab_events (thread_id, seq, kind, author_id, command_id, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                thread_id,
                seq,
                tag(&kind),
                author.map(|a| a.user_id),
                command_id,
                payload.to_string(),
                now
            ],
        )
        .map_err(db)?;
    let event = EventEnvelope {
        thread_id: thread_id.to_owned(),
        seq,
        kind,
        author: author.map(|a| a.username.clone()),
        command_id: command_id.map(str::to_owned),
        created_at: now,
        payload,
    };
    outbox.events.push(event.clone());
    Ok(event)
}

/// Events after `after`, only if `user_id` is (still) a member. Every
/// backfill path goes through this check, so a revoked user cannot read
/// events through a reconnect or a lag recovery.
pub(crate) fn member_events(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
    after: i64,
    limit: i64,
) -> Result<EventsPage, AuthError> {
    member_thread(connection, thread_id, user_id)?;
    let mut statement = connection
        .prepare(
            "SELECT e.seq, e.kind, u.username, e.command_id, e.created_at, e.payload
             FROM collab_events e LEFT JOIN auth_users u ON u.id = e.author_id
             WHERE e.thread_id = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3",
        )
        .map_err(db)?;
    let events = statement
        .query_map(params![thread_id, after, limit], |row| {
            let payload: String = row.get(5)?;
            Ok(EventEnvelope {
                thread_id: thread_id.to_owned(),
                seq: row.get(0)?,
                kind: untag(row.get(1)?)?,
                author: row.get(2)?,
                command_id: row.get(3)?,
                created_at: row.get(4)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)?;
    Ok(EventsPage {
        events,
        last_seq: last_seq(connection, thread_id)?,
    })
}

/// Looks up an earlier command with the same (author, command_id). A retry
/// returns the original receipt; reuse for another thread or kind conflicts.
pub(crate) fn existing_command(
    connection: &Connection,
    author_id: i64,
    command_id: &str,
    thread_id: &str,
    kind: CommandKind,
) -> Result<Option<CommandReceipt>, AuthError> {
    let row: Option<(String, String, String, i64)> = connection
        .query_row(
            "SELECT thread_id, kind, state, seq FROM collab_commands
             WHERE author_id = ?1 AND command_id = ?2",
            params![author_id, command_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(db)?;
    let Some((existing_thread, existing_kind, state, seq)) = row else {
        return Ok(None);
    };
    if existing_thread != thread_id || existing_kind != tag(&kind) {
        return Err(AuthError::Conflict);
    }
    Ok(Some(CommandReceipt {
        command_id: command_id.to_owned(),
        thread_id: thread_id.to_owned(),
        kind,
        state: untag(state).map_err(db)?,
        seq,
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_command(
    connection: &Connection,
    author_id: i64,
    command_id: &str,
    thread_id: &str,
    kind: CommandKind,
    state: CommandState,
    seq: i64,
    now: i64,
) -> Result<CommandReceipt, AuthError> {
    connection
        .execute(
            "INSERT INTO collab_commands
                 (author_id, command_id, thread_id, kind, state, seq, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                author_id,
                command_id,
                thread_id,
                tag(&kind),
                tag(&state),
                seq,
                now
            ],
        )
        .map_err(db)?;
    Ok(CommandReceipt {
        command_id: command_id.to_owned(),
        thread_id: thread_id.to_owned(),
        kind,
        state,
        seq,
    })
}

pub(crate) fn user_identity(connection: &Connection, user_id: i64) -> Result<Identity, AuthError> {
    connection
        .query_row(
            "SELECT username FROM auth_users WHERE id = ?1",
            [user_id],
            |row| row.get(0),
        )
        .map(|username| Identity { user_id, username })
        .map_err(db)
}
