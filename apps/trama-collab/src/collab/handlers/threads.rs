//! Thread lifecycle and membership handlers.

use super::*;

pub(crate) async fn create_thread(
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

pub(crate) async fn list_threads(
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

pub(crate) async fn get_thread(
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
pub(crate) async fn set_access(
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

pub(crate) async fn invite(
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

pub(crate) async fn remove_member(
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
pub(crate) async fn join(
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
