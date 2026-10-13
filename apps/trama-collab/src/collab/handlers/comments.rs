//! Comment and review verdict handlers.

use super::*;

pub(crate) async fn comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<CommentRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_text(&input.body, MAX_COMMENT_BYTES, true)?;
    if input
        .anchor
        .as_ref()
        .is_some_and(|anchor| json_len(anchor) > MAX_ANCHOR_BYTES)
    {
        return Err(AuthError::InvalidInput);
    }
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Comment,
        move |connection, outbox, identity, command_id, now| match input.parent_seq {
            Some(parent_seq) => {
                require_comment(connection, &thread, parent_seq)?;
                append_event(
                    connection,
                    outbox,
                    &thread,
                    EventKind::CommentReplied,
                    Some(identity),
                    Some(command_id),
                    json!({"parent_seq": parent_seq, "body": input.body}),
                    now,
                )
            }
            None => append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentCreated,
                Some(identity),
                Some(command_id),
                json!({"body": input.body, "anchor": input.anchor}),
                now,
            ),
        },
    )
    .await?;
    Ok(Json(receipt))
}

pub(crate) fn require_comment(
    connection: &Connection,
    thread_id: &str,
    seq: i64,
) -> Result<(), AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_events WHERE thread_id = ?1 AND seq = ?2 AND kind = 'comment_created'",
            params![thread_id, seq],
            |_| Ok(()),
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::InvalidInput)
}

pub(crate) async fn resolve_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, seq)): Path<(String, i64)>,
    Json(input): Json<ResolveRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Resolve,
        move |connection, outbox, identity, command_id, now| {
            require_comment(connection, &thread, seq)?;
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentResolved,
                Some(identity),
                Some(command_id),
                json!({"comment_seq": seq}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

pub(crate) async fn verdict(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<VerdictRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    if let Some(body) = &input.body {
        validate_text(body, MAX_COMMENT_BYTES, true)?;
    }
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Verdict,
        move |connection, outbox, identity, command_id, now| {
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::ReviewVerdict,
                Some(identity),
                Some(command_id),
                json!({"verdict": input.verdict, "body": input.body}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

pub(crate) async fn reopen_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, seq)): Path<(String, i64)>,
    Json(input): Json<ResolveRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Reopen,
        move |connection, outbox, identity, command_id, now| {
            require_comment(connection, &thread, seq)?;
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentReopened,
                Some(identity),
                Some(command_id),
                json!({"comment_seq": seq}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

/// Only the comment's author may delete it (and its replies, implicitly).
pub(crate) async fn delete_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, seq)): Path<(String, i64)>,
    Json(input): Json<ResolveRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Delete,
        move |connection, outbox, identity, command_id, now| {
            require_comment(connection, &thread, seq)?;
            let author: Option<i64> = connection
                .query_row(
                    "SELECT author_id FROM collab_events WHERE thread_id = ?1 AND seq = ?2",
                    params![thread, seq],
                    |row| row.get(0),
                )
                .map_err(db)?;
            if author != Some(identity.user_id) {
                return Err(AuthError::Forbidden);
            }
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentDeleted,
                Some(identity),
                Some(command_id),
                json!({"comment_seq": seq}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}
