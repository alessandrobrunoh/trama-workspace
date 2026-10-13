//! Event listing, command and message handlers.

use super::*;

pub(crate) async fn list_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Query(query): Query<AfterQuery>,
) -> Result<Json<EventsPage>, AuthError> {
    let identity = session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    if query.after < 0 || !(1..=MAX_PAGE).contains(&limit) {
        return Err(AuthError::InvalidInput);
    }
    let page = read(&state, move |connection| {
        member_events(connection, &thread_id, identity.user_id, query.after, limit)
    })
    .await?;
    Ok(Json(page))
}

pub(crate) async fn list_commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<CommandInfo>>, AuthError> {
    let identity = session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let commands = read(&state, move |connection| {
        member_thread(connection, &thread_id, identity.user_id)?;
        let mut statement = connection
            .prepare(
                "SELECT c.command_id, c.kind, u.username, c.state, c.seq, r.name
                 FROM collab_commands c
                 JOIN auth_users u ON u.id = c.author_id
                 LEFT JOIN collab_runners r ON r.id = c.runner_id
                 WHERE c.thread_id = ?1 AND c.kind = 'turn' ORDER BY c.seq DESC LIMIT ?2",
            )
            .map_err(db)?;
        let mut commands = statement
            .query_map(params![thread_id, MAX_PAGE], |row| {
                Ok(CommandInfo {
                    command_id: row.get(0)?,
                    thread_id: thread_id.clone(),
                    kind: untag(row.get(1)?)?,
                    author: row.get(2)?,
                    state: untag(row.get(3)?)?,
                    seq: row.get(4)?,
                    runner: row.get(5)?,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)?;
        commands.reverse();
        Ok(commands)
    })
    .await?;
    Ok(Json(commands))
}

/// Shared shape of every idempotent thread command: membership check, replay
/// of an earlier (author, command_id), else apply and record.
pub(crate) async fn run_command(
    state: &AppState,
    identity: Identity,
    thread_id: String,
    command_id: String,
    kind: CommandKind,
    apply: impl FnOnce(
        &Connection,
        &mut Outbox,
        &Identity,
        &str,
        i64,
    ) -> Result<EventEnvelope, AuthError>
    + Send
    + 'static,
) -> Result<(CommandReceipt, bool), AuthError> {
    validate_thread_id(&thread_id)?;
    validate_command_id(&command_id)?;
    let now = state.auth.now();
    write(state, move |connection, outbox| {
        member_thread(connection, &thread_id, identity.user_id)?;
        if let Some(receipt) =
            existing_command(connection, identity.user_id, &command_id, &thread_id, kind)?
        {
            return Ok((receipt, true));
        }
        let event = apply(connection, outbox, &identity, &command_id, now)?;
        let state = if kind == CommandKind::Turn {
            CommandState::Pending
        } else {
            CommandState::Applied
        };
        let receipt = insert_command(
            connection,
            identity.user_id,
            &command_id,
            &thread_id,
            kind,
            state,
            event.seq,
            now,
        )?;
        if kind == CommandKind::Turn {
            outbox.turns.push((
                identity.user_id,
                PendingTurn {
                    command_id: command_id.clone(),
                    thread_id: thread_id.clone(),
                    seq: event.seq,
                    text: event.payload["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                },
            ));
        }
        Ok((receipt, false))
    })
    .await
}

/// Send: the sender's text (or current live draft) becomes one user turn
/// authored by the sender. Only the sender's own runner may claim it.
pub(crate) async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<SendRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let text = input
        .text
        .or_else(|| state.hub.draft(&thread_id, &identity.username));
    let username = identity.username.clone();
    let sent = text.clone();
    let (receipt, replayed) = run_command(
        &state,
        identity,
        thread_id.clone(),
        input.command_id,
        CommandKind::Turn,
        move |connection, outbox, identity, command_id, now| {
            let text = text.ok_or(AuthError::InvalidInput)?;
            validate_text(&text, MAX_TEXT_BYTES, true)?;
            append_event(
                connection,
                outbox,
                &thread_id,
                EventKind::MessageAppended,
                Some(identity),
                Some(command_id),
                json!({"text": text}),
                now,
            )
        },
    )
    .await?;
    if !replayed && let Some(sent) = sent {
        state
            .hub
            .clear_sent_draft(&receipt.thread_id, &username, &sent, state.auth.now());
    }
    Ok(Json(receipt))
}
