//! SQLite storage: thread, event, command, runner and turn rows.

use super::*;

pub(super) struct ThreadRow {
    pub(super) title: String,
    /// Owner and access of the root (a child has no ACL of its own).
    pub(super) owner_id: i64,
    pub(super) owner: String,
    pub(super) access: AccessLevel,
    pub(super) via: MemberVia,
    pub(super) parent_id: Option<String>,
    pub(super) root_id: Option<String>,
    pub(super) kind: Option<String>,
}

impl ThreadRow {
    pub(super) fn summary(self, thread_id: String, last_seq: i64) -> ThreadSummary {
        ThreadSummary {
            thread_id,
            title: self.title,
            owner: self.owner,
            access: self.access,
            via: self.via,
            last_seq,
            parent_thread_id: self.parent_id,
            root_thread_id: self.root_id,
            kind: self.kind,
        }
    }
}

/// Returns the thread only if `user_id` is a member of it, or of the root
/// a child thread belongs to; otherwise `NotFound`, whether or not the
/// thread exists. Child threads inherit the root's membership, so revoking
/// access to the root revokes the whole family at once.
pub(super) fn member_thread(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<ThreadRow, AuthError> {
    connection
        .query_row(
            "SELECT t.title, r.owner_id, o.username, r.access, m.via, t.parent_id, t.root_id, t.kind
             FROM collab_threads t
             JOIN collab_threads r ON r.id = COALESCE(t.root_id, t.id)
             JOIN collab_members m ON m.thread_id = r.id
             JOIN auth_users o ON o.id = r.owner_id
             WHERE t.id = ?1 AND m.user_id = ?2",
            params![thread_id, user_id],
            |row| {
                Ok(ThreadRow {
                    title: row.get(0)?,
                    owner_id: row.get(1)?,
                    owner: row.get(2)?,
                    access: untag(row.get(3)?)?,
                    via: untag(row.get(4)?)?,
                    parent_id: row.get(5)?,
                    root_id: row.get(6)?,
                    kind: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::NotFound)
}

/// Owner-only actions apply to a root thread; a child's access is its
/// root's, so managing it there is `400`.
pub(super) fn owner_thread(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<ThreadRow, AuthError> {
    let thread = member_thread(connection, thread_id, user_id)?;
    if thread.owner_id != user_id {
        return Err(AuthError::Forbidden);
    }
    if thread.root_id.is_some() {
        return Err(AuthError::InvalidInput);
    }
    Ok(thread)
}

pub(crate) fn is_member(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<bool, AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_threads t
             JOIN collab_members m ON m.thread_id = COALESCE(t.root_id, t.id)
             WHERE t.id = ?1 AND m.user_id = ?2",
            params![thread_id, user_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(db)
}

/// Usernames with access to `thread_id` (the root's members).
pub(crate) fn member_names(
    connection: &Connection,
    thread_id: &str,
) -> Result<Vec<String>, AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
             WHERE m.thread_id = (SELECT COALESCE(root_id, id) FROM collab_threads WHERE id = ?1)",
        )
        .map_err(db)?;
    statement
        .query_map([thread_id], |row| row.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

/// A root and every child under it: the threads whose sockets must
/// re-check access when the root's membership changes.
pub(super) fn family_ids(connection: &Connection, root_id: &str) -> Result<Vec<String>, AuthError> {
    let mut statement = connection
        .prepare("SELECT id FROM collab_threads WHERE id = ?1 OR root_id = ?1")
        .map_err(db)?;
    statement
        .query_map([root_id], |row| row.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

pub(super) fn last_seq(connection: &Connection, thread_id: &str) -> Result<i64, AuthError> {
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
pub(super) fn append_event(
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
pub(super) fn existing_command(
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
pub(super) fn insert_command(
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

pub(super) fn user_identity(connection: &Connection, user_id: i64) -> Result<Identity, AuthError> {
    connection
        .query_row(
            "SELECT username FROM auth_users WHERE id = ?1",
            [user_id],
            |row| row.get(0),
        )
        .map(|username| Identity { user_id, username })
        .map_err(db)
}

pub(crate) struct CreatedRunner {
    pub runner_id: i64,
    pub name: String,
    pub token: String,
}

pub(crate) fn insert_runner(
    connection: &Connection,
    user_id: i64,
    name: &str,
    now: i64,
) -> Result<CreatedRunner, AuthError> {
    if !valid_username(name) {
        return Err(AuthError::InvalidInput);
    }
    let active: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM collab_runners WHERE user_id = ?1 AND revoked_at IS NULL",
            [user_id],
            |row| row.get(0),
        )
        .map_err(db)?;
    if active >= MAX_ACTIVE_RUNNERS_PER_USER {
        return Err(AuthError::RateLimited);
    }
    let token = format!("{RUNNER_TOKEN_PREFIX}{}", random_hex(32)?);
    connection
        .execute(
            "INSERT INTO collab_runners (user_id, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, name, digest(&token), now],
        )
        .map_err(db)?;
    Ok(CreatedRunner {
        runner_id: connection.last_insert_rowid(),
        name: name.to_owned(),
        token,
    })
}

/// An authenticated runner credential. `owner` is the only user whose turns
/// this runner may see or claim.
#[derive(Clone, Debug)]
pub(crate) struct RunnerIdentity {
    pub runner_id: i64,
    pub name: String,
    pub owner: Identity,
}

pub(crate) fn runner_token(header: Option<&str>) -> Result<String, AuthError> {
    let token = header
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AuthError::Unauthorized)?;
    match token.strip_prefix(RUNNER_TOKEN_PREFIX) {
        Some(secret) if is_lower_hex(secret, 64) => Ok(token.to_owned()),
        _ => Err(AuthError::Unauthorized),
    }
}

pub(crate) fn lookup_runner(
    connection: &Connection,
    token: &str,
) -> Result<RunnerIdentity, AuthError> {
    connection
        .query_row(
            "SELECT r.id, r.name, u.id, u.username FROM collab_runners r
             JOIN auth_users u ON u.id = r.user_id
             WHERE r.token_hash = ?1 AND r.revoked_at IS NULL",
            [digest(token)],
            |row| {
                Ok(RunnerIdentity {
                    runner_id: row.get(0)?,
                    name: row.get(1)?,
                    owner: Identity {
                        user_id: row.get(2)?,
                        username: row.get(3)?,
                    },
                })
            },
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::Unauthorized)
}

pub(crate) fn runner_active(connection: &Connection, runner_id: i64) -> Result<bool, AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_runners WHERE id = ?1 AND revoked_at IS NULL",
            [runner_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(db)
}

/// Pending turns authored by `owner_id` in threads the owner still belongs
/// to. Turns authored by anyone else are never listed.
pub(crate) fn pending_turns(
    connection: &Connection,
    owner_id: i64,
) -> Result<Vec<PendingTurn>, AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT c.command_id, c.thread_id, c.seq, e.payload
             FROM collab_commands c
             JOIN collab_events e ON e.thread_id = c.thread_id AND e.seq = c.seq
             JOIN collab_threads t ON t.id = c.thread_id
             JOIN collab_members m
                 ON m.thread_id = COALESCE(t.root_id, t.id) AND m.user_id = c.author_id
             WHERE c.author_id = ?1 AND c.kind = 'turn' AND c.state = 'pending'
             ORDER BY c.id LIMIT ?2",
        )
        .map_err(db)?;
    statement
        .query_map(params![owner_id, MAX_PAGE], |row| {
            let payload: String = row.get(3)?;
            Ok(PendingTurn {
                command_id: row.get(0)?,
                thread_id: row.get(1)?,
                seq: row.get(2)?,
                text: turn_text(&payload),
            })
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

pub(super) fn turn_text(payload: &str) -> String {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value["text"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

pub(crate) enum ClaimOutcome {
    Claimed(PendingTurn),
    Rejected(&'static str),
}

pub(super) struct TurnRow {
    pub(super) id: i64,
    pub(super) thread_id: String,
    pub(super) seq: i64,
    pub(super) state: CommandState,
    pub(super) runner_conn: Option<String>,
}

/// Finds a turn authored by the runner's owner. The author filter is the
/// enforcement point of the sender-runs-turn rule.
pub(super) fn owner_turn(
    connection: &Connection,
    runner: &RunnerIdentity,
    command_id: &str,
) -> Result<Option<TurnRow>, AuthError> {
    connection
        .query_row(
            "SELECT id, thread_id, seq, state, runner_conn FROM collab_commands
             WHERE author_id = ?1 AND command_id = ?2 AND kind = 'turn'",
            params![runner.owner.user_id, command_id],
            |row| {
                Ok(TurnRow {
                    id: row.get(0)?,
                    thread_id: row.get(1)?,
                    seq: row.get(2)?,
                    state: untag(row.get(3)?)?,
                    runner_conn: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(db)
}

pub(crate) fn claim_turn(
    connection: &Connection,
    outbox: &mut Outbox,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
    now: i64,
) -> Result<ClaimOutcome, AuthError> {
    if validate_command_id(command_id).is_err() {
        return Ok(ClaimOutcome::Rejected("not found"));
    }
    let Some(turn) = owner_turn(connection, runner, command_id)? else {
        return Ok(ClaimOutcome::Rejected("not found"));
    };
    if !is_member(connection, &turn.thread_id, runner.owner.user_id)? {
        return Ok(ClaimOutcome::Rejected("not found"));
    }
    let text: String = connection
        .query_row(
            "SELECT payload FROM collab_events WHERE thread_id = ?1 AND seq = ?2",
            params![turn.thread_id, turn.seq],
            |row| row.get(0),
        )
        .map_err(db)?;
    let pending = PendingTurn {
        command_id: command_id.to_owned(),
        thread_id: turn.thread_id.clone(),
        seq: turn.seq,
        text: turn_text(&text),
    };
    match turn.state {
        CommandState::Pending => {}
        // A retried claim on the same connection is idempotent.
        CommandState::Claimed if turn.runner_conn.as_deref() == Some(connection_id) => {
            return Ok(ClaimOutcome::Claimed(pending));
        }
        CommandState::Claimed => return Ok(ClaimOutcome::Rejected("already claimed")),
        _ => return Ok(ClaimOutcome::Rejected("not pending")),
    }
    // Conditional update: only a still-pending turn can be claimed.
    let changed = connection
        .execute(
            "UPDATE collab_commands SET state = 'claimed', runner_id = ?1, runner_conn = ?2, updated_at = ?3
             WHERE id = ?4 AND state = 'pending'",
            params![runner.runner_id, connection_id, now, turn.id],
        )
        .map_err(db)?;
    if changed != 1 {
        return Ok(ClaimOutcome::Rejected("already claimed"));
    }
    append_event(
        connection,
        outbox,
        &turn.thread_id,
        EventKind::CommandClaimed,
        Some(&runner.owner),
        Some(command_id),
        json!({"runner": runner.name}),
        now,
    )?;
    Ok(ClaimOutcome::Claimed(pending))
}

/// The turn must be claimed by this very connection and its author must
/// still be a member of the thread.
pub(super) fn claimed_by(
    connection: &Connection,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
) -> Result<TurnRow, &'static str> {
    let turn = owner_turn(connection, runner, command_id)
        .map_err(|_| "internal error")?
        .ok_or("not found")?;
    if turn.state != CommandState::Claimed || turn.runner_conn.as_deref() != Some(connection_id) {
        return Err("turn is not claimed by this runner connection");
    }
    if !is_member(connection, &turn.thread_id, runner.owner.user_id)
        .map_err(|_| "internal error")?
    {
        return Err("not found");
    }
    Ok(turn)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn report_turn(
    connection: &Connection,
    outbox: &mut Outbox,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
    kind: RunnerReportKind,
    payload: Value,
    now: i64,
) -> Result<Result<i64, &'static str>, AuthError> {
    if json_len(&payload) > MAX_RUNNER_PAYLOAD_BYTES {
        return Ok(Err("payload too large"));
    }
    let turn = match claimed_by(connection, runner, connection_id, command_id) {
        Ok(turn) => turn,
        Err(reason) => return Ok(Err(reason)),
    };
    let kind = match kind {
        RunnerReportKind::AgentEvent => EventKind::AgentEvent,
        RunnerReportKind::ChangesSummary => EventKind::ChangesSummary,
    };
    let event = append_event(
        connection,
        outbox,
        &turn.thread_id,
        kind,
        Some(&runner.owner),
        Some(command_id),
        payload,
        now,
    )?;
    Ok(Ok(event.seq))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_turn(
    connection: &Connection,
    outbox: &mut Outbox,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
    outcome: TurnOutcome,
    result: Value,
    now: i64,
) -> Result<Result<(CommandState, i64), &'static str>, AuthError> {
    if json_len(&result) > MAX_RUNNER_PAYLOAD_BYTES {
        return Ok(Err("payload too large"));
    }
    let turn = match claimed_by(connection, runner, connection_id, command_id) {
        Ok(turn) => turn,
        Err(reason) => return Ok(Err(reason)),
    };
    let state = match outcome {
        TurnOutcome::Completed => CommandState::Completed,
        TurnOutcome::Failed => CommandState::Failed,
    };
    connection
        .execute(
            "UPDATE collab_commands SET state = ?1, runner_conn = NULL, updated_at = ?2 WHERE id = ?3",
            params![tag(&state), now, turn.id],
        )
        .map_err(db)?;
    let event = append_event(
        connection,
        outbox,
        &turn.thread_id,
        EventKind::CommandFinished,
        Some(&runner.owner),
        Some(command_id),
        json!({"state": state, "result": result}),
        now,
    )?;
    Ok(Ok((state, event.seq)))
}

/// Marks claims as ambiguous: those held by `connection_id`, or every open
/// claim when `None` (process restart). Never re-queues them.
pub(super) fn abandon_claims(
    connection: &Connection,
    outbox: &mut Outbox,
    connection_id: Option<&str>,
    now: i64,
) -> Result<(), AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT c.id, c.thread_id, c.command_id, c.author_id, r.name
             FROM collab_commands c LEFT JOIN collab_runners r ON r.id = c.runner_id
             WHERE c.state = 'claimed' AND (?1 IS NULL OR c.runner_conn = ?1)",
        )
        .map_err(db)?;
    let claims = statement
        .query_map([connection_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)?;
    drop(statement);
    for (id, thread_id, command_id, author_id, runner) in claims {
        connection
            .execute(
                "UPDATE collab_commands SET state = 'ambiguous', runner_conn = NULL, updated_at = ?1
                 WHERE id = ?2",
                params![now, id],
            )
            .map_err(db)?;
        let author = user_identity(connection, author_id)?;
        let reason = if connection_id.is_some() {
            "runner disconnected after claiming"
        } else {
            "service restarted while the turn was claimed"
        };
        append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::CommandAmbiguous,
            Some(&author),
            Some(&command_id),
            json!({"runner": runner, "reason": reason}),
            now,
        )?;
    }
    Ok(())
}

pub(crate) fn abandon_connection(
    connection: &Connection,
    outbox: &mut Outbox,
    connection_id: &str,
    now: i64,
) -> Result<(), AuthError> {
    abandon_claims(connection, outbox, Some(connection_id), now)
}

/// Startup recovery; no subscribers exist yet, so the outbox is discarded.
pub(crate) fn abandon_all_claims(connection: &Connection, now: i64) -> Result<(), AuthError> {
    connection.execute_batch("BEGIN IMMEDIATE").map_err(db)?;
    let result = abandon_claims(connection, &mut Outbox::default(), None, now);
    let finish = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
    connection.execute_batch(finish).map_err(db)?;
    result
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

/// Runs `op` in an IMMEDIATE transaction while holding the hub write lock,
/// then publishes its outbox before releasing the lock, so live delivery
/// order equals commit (sequence) order.
pub(crate) async fn write<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection, &mut Outbox) -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    let database = state.auth.database.clone();
    let hub = state.hub.clone();
    tokio_blocking(move || {
        let mut connection = open_connection(&database).map_err(db)?;
        let _guard = hub
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let mut outbox = Outbox::default();
        let value = op(&transaction, &mut outbox)?;
        transaction.commit().map_err(db)?;
        hub.publish(outbox);
        Ok(value)
    })
    .await
}

pub(crate) async fn read<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection) -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    let database = state.auth.database.clone();
    tokio_blocking(move || {
        let connection = open_connection(&database).map_err(db)?;
        op(&connection)
    })
    .await
}

pub(crate) async fn session(state: &AppState, headers: &HeaderMap) -> Result<Identity, AuthError> {
    let token = bearer_token(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()))?;
    state.auth.authenticate(token).await
}

pub(super) async fn mutating_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Identity, AuthError> {
    let identity = session(state, headers).await?;
    state
        .hub
        .allow_mutation(identity.user_id, state.auth.now())?;
    Ok(identity)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------
