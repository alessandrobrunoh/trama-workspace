//! Owner-only member revocation. The server forgets the grant at once (the
//! live stream drops the member at its next grant check); removing the
//! member's devices from the MLS group is the owner daemon's job, using
//! the returned device list.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use rusqlite::{OptionalExtension, params};
use trama_collab_protocol::vnext::api::{RevokedMember, ShareRole};

use super::{ShareError, grant, validate_id, write};
use crate::{AppState, collab, valid_username};

pub(crate) async fn revoke(
    State(state): State<AppState>,
    Path((share_id, username)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<RevokedMember>, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    if !valid_username(&username) {
        return Err(ShareError::Invalid("invalid username"));
    }
    let revoked = write(&state, move |c| {
        if grant(c, &share_id, identity.user_id)?.role != ShareRole::Owner {
            return Err(ShareError::Forbidden("only the share owner can do that"));
        }
        let user: i64 = c
            .query_row(
                "SELECT id FROM auth_users WHERE username = ?1",
                params![username],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(ShareError::NotFound)?;
        if user == identity.user_id {
            return Err(ShareError::Invalid("the owner cannot be removed"));
        }
        let removed = c.execute(
            "DELETE FROM v3_share_members WHERE share_id = ?1 AND user_id = ?2",
            params![share_id, user],
        )?;
        if removed == 0 {
            return Err(ShareError::NotFound);
        }
        let mut statement = c.prepare(
            "SELECT device_id FROM v3_share_devices WHERE share_id = ?1 AND user_id = ?2
             ORDER BY device_id",
        )?;
        let devices = statement
            .query_map(params![share_id, user], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        // Device bindings stay: a revoked device id can never be rebound to
        // another user, and its replay head keeps old events verifiable.
        for table in ["v3_share_key_packages", "v3_share_leases"] {
            c.execute(
                &format!("DELETE FROM {table} WHERE share_id = ?1 AND user_id = ?2"),
                params![share_id, user],
            )?;
        }
        Ok(RevokedMember { username, devices })
    })
    .await?;
    Ok(Json(revoked))
}
