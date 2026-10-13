//! Turn claiming, reporting and abandonment.

use super::*;

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

pub(crate) fn turn_text(payload: &str) -> String {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value["text"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

pub(crate) enum ClaimOutcome {
    Claimed(PendingTurn),
    Rejected(&'static str),
}

pub(crate) struct TurnRow {
    pub(crate) id: i64,
    pub(crate) thread_id: String,
    pub(crate) seq: i64,
    pub(crate) state: CommandState,
    pub(crate) runner_conn: Option<String>,
}

/// Finds a turn authored by the runner's owner. The author filter is the
/// enforcement point of the sender-runs-turn rule.
pub(crate) fn owner_turn(
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
pub(crate) fn claimed_by(
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
pub(crate) fn abandon_claims(
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
