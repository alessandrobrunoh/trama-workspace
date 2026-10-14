//! Child thread creation and publishing handlers.

use super::*;

pub(crate) fn valid_label(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Create a child thread (subthread, fork or subagent) under any thread of
/// a family. Any member may: the child is how their own agent's work in the
/// shared thread stays visible. The child inherits the root's membership;
/// a `child_thread_added` event in the parent's log announces it.
pub(crate) async fn create_child(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(parent_id): Path<String>,
    Json(input): Json<CreateChildRequest>,
) -> Result<(StatusCode, Json<ChildCreated>), AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&parent_id)?;
    validate_command_id(&input.command_id)?;
    validate_text(&input.title, MAX_TITLE_BYTES, false)?;
    if !valid_label(&input.kind, 32)
        || input
            .subagent
            .as_ref()
            .is_some_and(|value| !value.is_object() || json_len(value) > MAX_CHILD_SUBAGENT_BYTES)
        || input
            .fork_origin
            .as_deref()
            .is_some_and(|origin| !valid_label(origin, MAX_FORK_ORIGIN_BYTES))
    {
        return Err(AuthError::InvalidInput);
    }
    let now = state.auth.now();
    let child_id = random_hex(16)?;
    let created = write(&state, move |connection, outbox| {
        let parent = member_thread(connection, &parent_id, identity.user_id)?;
        if let Some(receipt) = existing_command(
            connection,
            identity.user_id,
            &input.command_id,
            &parent_id,
            CommandKind::Child,
        )? {
            // A retry: the child created the first time.
            let payload: String = connection
                .query_row(
                    "SELECT payload FROM collab_events WHERE thread_id = ?1 AND seq = ?2",
                    params![parent_id, receipt.seq],
                    |row| row.get(0),
                )
                .map_err(db)?;
            let child = serde_json::from_str::<Value>(&payload)
                .ok()
                .and_then(|v| v["child_thread_id"].as_str().map(str::to_owned))
                .ok_or(AuthError::Database)?;
            let row = member_thread(connection, &child, identity.user_id)?;
            let last = last_seq(connection, &child)?;
            return Ok(ChildCreated {
                thread: row.summary(child, last),
                seq: receipt.seq,
            });
        }
        let root = parent.root_id.clone().unwrap_or_else(|| parent_id.clone());
        let children: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM collab_threads WHERE root_id = ?1",
                [&root],
                |row| row.get(0),
            )
            .map_err(db)?;
        if children >= MAX_CHILDREN_PER_ROOT {
            return Err(AuthError::RateLimited);
        }
        connection
            .execute(
                "INSERT INTO collab_threads
                     (id, owner_id, title, access, created_at, parent_id, root_id, kind)
                 VALUES (?1, ?2, ?3, 'invited', ?4, ?5, ?6, ?7)",
                params![
                    child_id,
                    identity.user_id,
                    input.title,
                    now,
                    parent_id,
                    root,
                    input.kind
                ],
            )
            .map_err(db)?;
        let event = append_event(
            connection,
            outbox,
            &parent_id,
            EventKind::ChildThreadAdded,
            Some(&identity),
            Some(&input.command_id),
            json!({
                "child_thread_id": child_id,
                "title": input.title,
                "kind": input.kind,
                "subagent": input.subagent,
                "fork_origin": input.fork_origin,
            }),
            now,
        )?;
        insert_command(
            connection,
            identity.user_id,
            &input.command_id,
            &parent_id,
            CommandKind::Child,
            CommandState::Applied,
            event.seq,
            now,
        )?;
        let row = member_thread(connection, &child_id, identity.user_id)?;
        Ok(ChildCreated {
            thread: row.summary(child_id, 0),
            seq: event.seq,
        })
    })
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Append a batch of history, activity, changes or status events authored
/// by the caller, in order and in one transaction. Each event is idempotent
/// by its `command_id`. Counted as one mutation for rate limiting.
pub(crate) async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<PublishRequest>,
) -> Result<Json<PublishResponse>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if input.events.is_empty() || input.events.len() > MAX_PUBLISH_EVENTS {
        return Err(AuthError::InvalidInput);
    }
    for event in &input.events {
        validate_command_id(&event.command_id)?;
        if !event.payload.is_object() || json_len(&event.payload) > MAX_PUBLISH_PAYLOAD_BYTES {
            return Err(AuthError::InvalidInput);
        }
    }
    let now = state.auth.now();
    let receipts = write(&state, move |connection, outbox| {
        member_thread(connection, &thread_id, identity.user_id)?;
        let mut receipts = Vec::with_capacity(input.events.len());
        for event in input.events {
            if let Some(receipt) = existing_command(
                connection,
                identity.user_id,
                &event.command_id,
                &thread_id,
                CommandKind::Publish,
            )? {
                receipts.push(receipt);
                continue;
            }
            let appended = append_event(
                connection,
                outbox,
                &thread_id,
                event.kind.event_kind(),
                Some(&identity),
                Some(&event.command_id),
                event.payload,
                now,
            )?;
            receipts.push(insert_command(
                connection,
                identity.user_id,
                &event.command_id,
                &thread_id,
                CommandKind::Publish,
                CommandState::Applied,
                appended.seq,
                now,
            )?);
        }
        Ok(receipts)
    })
    .await?;
    Ok(Json(PublishResponse { receipts }))
}
