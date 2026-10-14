//! Share registration, info and member grants.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use rusqlite::{OptionalExtension, params};
use trama_collab_protocol::vnext::api::{
    RegisterShareRequest, ShareInfo, ShareMemberRequest, ShareRole,
};

use super::{ShareError, grant, read, role_str, validate_id, write};
use crate::{AppState, collab, valid_username};

pub(crate) fn info_row(
    c: &rusqlite::Connection,
    share_id: &str,
    user_id: i64,
) -> Result<ShareInfo, ShareError> {
    let g = grant(c, share_id, user_id)?;
    let (owner, last_seq, version): (String, i64, i64) = c.query_row(
        "SELECT u.username, s.last_seq, s.manifest_version
         FROM v3_shares s JOIN auth_users u ON u.id = s.owner_id WHERE s.share_id = ?1",
        params![share_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(ShareInfo {
        share_id: share_id.to_owned(),
        owner,
        role: g.role,
        read_code: g.read_code,
        latest_manifest_version: version as u64,
        last_server_seq: last_seq as u64,
    })
}

pub(crate) async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RegisterShareRequest>,
) -> Result<(StatusCode, Json<ShareInfo>), ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&input.share_id)?;
    let now = state.auth.now();
    let share_id = input.share_id;
    let info = write(&state, move |c| {
        let exists = c
            .query_row(
                "SELECT 1 FROM v3_shares WHERE share_id = ?1",
                params![share_id],
                |_| Ok(()),
            )
            .optional()?;
        if exists.is_some() {
            return Err(ShareError::Conflict("share already exists"));
        }
        c.execute(
            "INSERT INTO v3_shares (share_id, owner_id, created_at) VALUES (?1, ?2, ?3)",
            params![share_id, identity.user_id, now],
        )?;
        c.execute(
            "INSERT INTO v3_share_members (share_id, user_id, role, read_code)
             VALUES (?1, ?2, 'owner', 1)",
            params![share_id, identity.user_id],
        )?;
        info_row(c, &share_id, identity.user_id)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(info)))
}

pub(crate) async fn info(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ShareInfo>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    Ok(Json(
        read(&state, move |c| info_row(c, &share_id, identity.user_id)).await?,
    ))
}

/// Owner only. The owner's own grant cannot be changed, and nobody else can
/// be made owner.
pub(crate) async fn add_member(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<ShareMemberRequest>,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    if !valid_username(&input.username) {
        return Err(ShareError::Invalid("invalid username"));
    }
    if input.role == ShareRole::Owner {
        return Err(ShareError::Invalid("a share has exactly one owner"));
    }
    write(&state, move |c| {
        if grant(c, &share_id, identity.user_id)?.role != ShareRole::Owner {
            return Err(ShareError::Forbidden("only the share owner can do that"));
        }
        let user: Option<i64> = c
            .query_row(
                "SELECT id FROM auth_users WHERE username = ?1",
                params![input.username],
                |r| r.get(0),
            )
            .optional()?;
        let user = user.ok_or(ShareError::NotFound)?;
        if user == identity.user_id {
            return Err(ShareError::Invalid("the owner's grant is fixed"));
        }
        c.execute(
            "INSERT INTO v3_share_members (share_id, user_id, role, read_code)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (share_id, user_id) DO UPDATE SET role = ?3, read_code = ?4",
            params![share_id, user, role_str(input.role), input.read_code as i64],
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
