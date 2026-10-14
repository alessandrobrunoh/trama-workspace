//! Writer leases: one device at a time may append history to a thread.
//! A lease expires on its own; the holder renews or releases it.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use rusqlite::{OptionalExtension, params};
use trama_collab_protocol::vnext::api::{LeaseInfo, LeaseRequest, MAX_LEASE_TTL_SECS};

use super::{ShareError, grant, read, validate_id, write};
use crate::{AppState, collab};

pub(crate) async fn acquire(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<LeaseRequest>,
) -> Result<Json<LeaseInfo>, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&input.thread_gid)?;
    validate_id(&input.device_id)?;
    if input.ttl_secs == 0 || input.ttl_secs > MAX_LEASE_TTL_SECS {
        return Err(ShareError::Invalid("ttl_secs must be 1..=600"));
    }
    let now = state.auth.now();
    let expires_at = now + input.ttl_secs as i64;
    let info = write(&state, move |c| {
        if !grant(c, &share_id, identity.user_id)?.can_write() {
            return Err(ShareError::Forbidden("read-only member"));
        }
        let held: Option<(i64, String, i64)> = c
            .query_row(
                "SELECT user_id, device_id, expires_at FROM v3_share_leases
                 WHERE share_id = ?1 AND thread_gid = ?2",
                params![share_id, input.thread_gid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((user, device, until)) = held
            && until > now
            && (user != identity.user_id || device != input.device_id)
        {
            return Err(ShareError::Conflict("lease held by another device"));
        }
        c.execute(
            "INSERT INTO v3_share_leases (share_id, thread_gid, user_id, device_id, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (share_id, thread_gid)
             DO UPDATE SET user_id = ?3, device_id = ?4, expires_at = ?5",
            params![
                share_id,
                input.thread_gid,
                identity.user_id,
                input.device_id,
                expires_at
            ],
        )?;
        Ok(LeaseInfo {
            thread_gid: input.thread_gid,
            holder: identity.username,
            device_id: input.device_id,
            expires_at,
        })
    })
    .await?;
    Ok(Json(info))
}

/// Only the holding user may release; releasing a free lease is a no-op.
pub(crate) async fn release(
    State(state): State<AppState>,
    Path((share_id, thread_gid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&thread_gid)?;
    write(&state, move |c| {
        grant(c, &share_id, identity.user_id)?;
        c.execute(
            "DELETE FROM v3_share_leases WHERE share_id = ?1 AND thread_gid = ?2 AND user_id = ?3",
            params![share_id, thread_gid, identity.user_id],
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Active leases of the share.
pub(crate) async fn list(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<LeaseInfo>>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let now = state.auth.now();
    let leases = read(&state, move |c| {
        grant(c, &share_id, identity.user_id)?;
        let mut statement = c.prepare(
            "SELECT l.thread_gid, u.username, l.device_id, l.expires_at
             FROM v3_share_leases l JOIN auth_users u ON u.id = l.user_id
             WHERE l.share_id = ?1 AND l.expires_at > ?2 ORDER BY l.thread_gid",
        )?;
        let rows = statement
            .query_map(params![share_id, now], |r| {
                Ok(LeaseInfo {
                    thread_gid: r.get(0)?,
                    holder: r.get(1)?,
                    device_id: r.get(2)?,
                    expires_at: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await?;
    Ok(Json(leases))
}
