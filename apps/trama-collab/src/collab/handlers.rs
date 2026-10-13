//! HTTP handlers for collaboration routes.

use super::*;

pub(super) async fn create_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateThreadRequest>,
) -> Result<(StatusCode, Json<ThreadSummary>), AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_text(&input.title, MAX_TITLE_BYTES, false)?;
    let now = state.auth.now();
    let thread_id = random_hex(16)?;
    let summary = write(&state, move |connection, outbox| {
        connection
            .execute(
                "INSERT INTO collab_threads (id, owner_id, title, access, created_at)
                 VALUES (?1, ?2, ?3, 'invited', ?4)",
                params![thread_id, identity.user_id, input.title, now],
            )
            .map_err(db)?;
        connection
            .execute(
                "INSERT INTO collab_members (thread_id, user_id, via, added_at) VALUES (?1, ?2, 'owner', ?3)",
                params![thread_id, identity.user_id, now],
            )
            .map_err(db)?;
        let event = append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::MemberJoined,
            Some(&identity),
            None,
            json!({"username": identity.username, "via": MemberVia::Owner}),
            now,
        )?;
        Ok(ThreadSummary {
            thread_id,
            title: input.title,
            owner: identity.username,
            access: AccessLevel::Invited,
            via: MemberVia::Owner,
            last_seq: event.seq,
            parent_thread_id: None,
            root_thread_id: None,
            kind: None,
        })
    })
    .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

pub(super) async fn list_threads(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ThreadSummary>>, AuthError> {
    let identity = session(&state, &headers).await?;
    let threads = read(&state, move |connection| {
        let mut statement = connection
            .prepare(
                "SELECT t.id, t.title, o.username, t.access, m.via,
                        (SELECT COALESCE(MAX(e.seq), 0) FROM collab_events e WHERE e.thread_id = t.id)
                 FROM collab_members m
                 JOIN collab_threads t ON t.id = m.thread_id
                 JOIN auth_users o ON o.id = t.owner_id
                 WHERE m.user_id = ?1 ORDER BY t.created_at, t.id LIMIT ?2",
            )
            .map_err(db)?;
        statement
            .query_map(params![identity.user_id, MAX_PAGE], |row| {
                Ok(ThreadSummary {
                    thread_id: row.get(0)?,
                    title: row.get(1)?,
                    owner: row.get(2)?,
                    access: untag(row.get(3)?)?,
                    via: untag(row.get(4)?)?,
                    last_seq: row.get(5)?,
                    parent_thread_id: None,
                    root_thread_id: None,
                    kind: None,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)
    })
    .await?;
    Ok(Json(threads))
}

pub(super) async fn get_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadDetail>, AuthError> {
    let identity = session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let detail = read(&state, move |connection| {
        let thread = member_thread(connection, &thread_id, identity.user_id)?;
        let mut statement = connection
            .prepare(
                "SELECT u.username, m.via FROM collab_members m JOIN auth_users u ON u.id = m.user_id
                 WHERE m.thread_id = (SELECT COALESCE(root_id, id) FROM collab_threads WHERE id = ?1)
                 ORDER BY m.added_at, u.username",
            )
            .map_err(db)?;
        let members = statement
            .query_map([&thread_id], |row| {
                Ok(MemberInfo {
                    username: row.get(0)?,
                    via: untag(row.get(1)?)?,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)?;
        let last_seq = last_seq(connection, &thread_id)?;
        Ok(ThreadDetail {
            summary: thread.summary(thread_id, last_seq),
            members,
        })
    })
    .await?;
    Ok(Json(detail))
}

/// Owner only. `link` issues a fresh link token (rotating any previous one);
/// `invited` revokes the link and removes everyone who joined through it.
pub(super) async fn set_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<SetAccessRequest>,
) -> Result<Json<SetAccessResponse>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let now = state.auth.now();
    let link_token = match input.access {
        AccessLevel::Link => Some(random_hex(32)?),
        AccessLevel::Invited => None,
    };
    let response = SetAccessResponse {
        access: input.access,
        link_token: link_token.clone(),
    };
    write(&state, move |connection, outbox| {
        let thread = owner_thread(connection, &thread_id, identity.user_id)?;
        connection
            .execute(
                "UPDATE collab_threads SET access = ?1, link_token_hash = ?2 WHERE id = ?3",
                params![
                    tag(&input.access),
                    link_token.as_deref().map(digest),
                    thread_id
                ],
            )
            .map_err(db)?;
        if input.access == AccessLevel::Invited {
            let mut statement = connection
                .prepare(
                    "SELECT u.id, u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
                     WHERE m.thread_id = ?1 AND m.via = 'link'",
                )
                .map_err(db)?;
            let removed = statement
                .query_map([&thread_id], |row| {
                    Ok(Identity {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                    })
                })
                .map_err(db)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db)?;
            drop(statement);
            connection
                .execute(
                    "DELETE FROM collab_members WHERE thread_id = ?1 AND via = 'link'",
                    [&thread_id],
                )
                .map_err(db)?;
            let family = family_ids(connection, &thread_id)?;
            for member in removed {
                append_event(
                    connection,
                    outbox,
                    &thread_id,
                    EventKind::MemberRemoved,
                    Some(&identity),
                    None,
                    json!({"username": member.username}),
                    now,
                )?;
                for id in &family {
                    outbox.removed.push((id.clone(), member.username.clone()));
                }
            }
        }
        if thread.access != input.access {
            append_event(
                connection,
                outbox,
                &thread_id,
                EventKind::AccessChanged,
                Some(&identity),
                None,
                json!({"access": input.access}),
                now,
            )?;
        }
        outbox.access_changed.extend(family_ids(connection, &thread_id)?);
        Ok(())
    })
    .await?;
    Ok(Json(response))
}

pub(super) async fn invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<InviteRequest>,
) -> Result<StatusCode, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if !valid_username(&input.username) {
        return Err(AuthError::InvalidInput);
    }
    let now = state.auth.now();
    write(&state, move |connection, outbox| {
        owner_thread(connection, &thread_id, identity.user_id)?;
        let invitee: Identity = connection
            .query_row(
                "SELECT id, username FROM auth_users WHERE username = ?1 COLLATE NOCASE",
                [&input.username],
                |row| {
                    Ok(Identity {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db)?
            .ok_or(AuthError::NotFound)?;
        let current: Option<String> = connection
            .query_row(
                "SELECT via FROM collab_members WHERE thread_id = ?1 AND user_id = ?2",
                params![thread_id, invitee.user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        match current.as_deref() {
            // Idempotent: already invited (or the owner).
            Some("invite") | Some("owner") => return Ok(()),
            // An explicit invite outlives link revocation.
            Some(_) => {
                connection
                    .execute(
                        "UPDATE collab_members SET via = 'invite' WHERE thread_id = ?1 AND user_id = ?2",
                        params![thread_id, invitee.user_id],
                    )
                    .map_err(db)?;
                return Ok(());
            }
            None => {}
        }
        connection
            .execute(
                "INSERT INTO collab_members (thread_id, user_id, via, added_at) VALUES (?1, ?2, 'invite', ?3)",
                params![thread_id, invitee.user_id, now],
            )
            .map_err(db)?;
        append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::MemberJoined,
            Some(&identity),
            None,
            json!({"username": invitee.username, "via": MemberVia::Invite}),
            now,
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, username)): Path<(String, String)>,
) -> Result<StatusCode, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if !valid_username(&username) {
        return Err(AuthError::NotFound);
    }
    let now = state.auth.now();
    write(&state, move |connection, outbox| {
        let thread = owner_thread(connection, &thread_id, identity.user_id)?;
        let member: Identity = connection
            .query_row(
                "SELECT u.id, u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
                 WHERE m.thread_id = ?1 AND u.username = ?2 COLLATE NOCASE",
                params![thread_id, username],
                |row| {
                    Ok(Identity {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db)?
            .ok_or(AuthError::NotFound)?;
        if member.user_id == thread.owner_id {
            return Err(AuthError::InvalidInput);
        }
        connection
            .execute(
                "DELETE FROM collab_members WHERE thread_id = ?1 AND user_id = ?2",
                params![thread_id, member.user_id],
            )
            .map_err(db)?;
        append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::MemberRemoved,
            Some(&identity),
            None,
            json!({"username": member.username}),
            now,
        )?;
        let family = family_ids(connection, &thread_id)?;
        for id in &family {
            outbox.removed.push((id.clone(), member.username.clone()));
        }
        outbox.access_changed.extend(family);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Joins through the current link token. Any mismatch (no link, wrong or
/// rotated token, unknown thread) is the same `404`.
pub(super) async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<JoinRequest>,
) -> Result<Json<ThreadSummary>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if !is_lower_hex(&input.link_token, 64) {
        return Err(AuthError::NotFound);
    }
    let now = state.auth.now();
    let summary = write(&state, move |connection, outbox| {
        let linked = connection
            .query_row(
                "SELECT 1 FROM collab_threads
                 WHERE id = ?1 AND access = 'link' AND link_token_hash = ?2",
                params![thread_id, digest(&input.link_token)],
                |_| Ok(()),
            )
            .optional()
            .map_err(db)?
            .is_some();
        if !linked {
            return Err(AuthError::NotFound);
        }
        if !is_member(connection, &thread_id, identity.user_id)? {
            connection
                .execute(
                    "INSERT INTO collab_members (thread_id, user_id, via, added_at) VALUES (?1, ?2, 'link', ?3)",
                    params![thread_id, identity.user_id, now],
                )
                .map_err(db)?;
            append_event(
                connection,
                outbox,
                &thread_id,
                EventKind::MemberJoined,
                Some(&identity),
                None,
                json!({"username": identity.username, "via": MemberVia::Link}),
                now,
            )?;
        }
        let thread = member_thread(connection, &thread_id, identity.user_id)?;
        let last_seq = last_seq(connection, &thread_id)?;
        Ok(thread.summary(thread_id, last_seq))
    })
    .await?;
    Ok(Json(summary))
}

#[derive(Deserialize)]
pub(crate) struct AfterQuery {
    #[serde(default)]
    pub after: i64,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub(super) async fn list_events(
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

pub(super) async fn list_commands(
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
pub(super) async fn run_command(
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
pub(super) async fn send(
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

pub(super) async fn comment(
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

pub(super) fn require_comment(
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

pub(super) async fn resolve_comment(
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

pub(super) async fn verdict(
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

pub(super) async fn reopen_comment(
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
pub(super) async fn delete_comment(
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

pub(super) fn valid_label(text: &str, max: usize) -> bool {
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
pub(super) async fn create_child(
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
pub(super) async fn publish(
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

pub(super) async fn create_runner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRunnerRequest>,
) -> Result<(StatusCode, Json<RunnerCreated>), AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let now = state.auth.now();
    let created = read(&state, move |connection| {
        insert_runner(connection, identity.user_id, &input.name, now)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(RunnerCreated {
            runner_id: created.runner_id,
            name: created.name,
            token: created.token,
        }),
    ))
}

pub(super) async fn list_runners(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<RunnerInfo>>, AuthError> {
    let identity = session(&state, &headers).await?;
    let runners = read(&state, move |connection| {
        let mut statement = connection
            .prepare(
                "SELECT id, name, created_at, revoked_at IS NOT NULL FROM collab_runners
                 WHERE user_id = ?1 ORDER BY id",
            )
            .map_err(db)?;
        statement
            .query_map([identity.user_id], |row| {
                Ok(RunnerInfo {
                    runner_id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    revoked: row.get(3)?,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)
    })
    .await?;
    Ok(Json(runners))
}

/// Only the owner can revoke a runner; another user's runner id is `404`.
/// Open runner sockets close and their claims become ambiguous.
pub(super) async fn revoke_runner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(runner_id): Path<i64>,
) -> Result<StatusCode, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let now = state.auth.now();
    let changed = read(&state, move |connection| {
        connection
            .execute(
                "UPDATE collab_runners SET revoked_at = ?1
                 WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
                params![now, runner_id, identity.user_id],
            )
            .map_err(db)
    })
    .await?;
    if changed == 0 {
        return Err(AuthError::NotFound);
    }
    state.hub.credentials_changed();
    Ok(StatusCode::NO_CONTENT)
}
