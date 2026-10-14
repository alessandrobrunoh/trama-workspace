//! Runner rows and tokens.

use super::*;

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
