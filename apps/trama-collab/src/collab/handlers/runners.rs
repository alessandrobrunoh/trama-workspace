//! Runner registration handlers.

use super::*;

pub(crate) async fn create_runner(
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

pub(crate) async fn list_runners(
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
pub(crate) async fn revoke_runner(
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
