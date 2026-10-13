//! Member listing and device signing-key bindings. A binding ties a
//! device's manifest signing key to the authenticated account (the GitHub
//! identity behind the session), so a joiner can check that a snapshot was
//! signed by a device of the share owner, not just pin the first key seen.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use trama_collab_protocol::vnext::api::{ShareMemberInfo, SigningKeyBinding, SigningKeyRequest};

use super::{ShareError, grant, read, validate_id, write};
use crate::{AppState, collab};

pub(crate) fn migrate(c: &rusqlite::Connection) -> Result<(), String> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS v3_signing_keys (
             device_id TEXT PRIMARY KEY NOT NULL,
             user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
             public_key TEXT NOT NULL,
             created_at INTEGER NOT NULL
         );",
    )
    .map_err(|e| format!("initialize signing key schema: {e}"))
}

fn parse_role(role: &str) -> trama_collab_protocol::vnext::api::ShareRole {
    use trama_collab_protocol::vnext::api::ShareRole;
    match role {
        "owner" => ShareRole::Owner,
        "writer" => ShareRole::Writer,
        _ => ShareRole::Reader,
    }
}

pub(crate) async fn members(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<ShareMemberInfo>>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let out = read(&state, move |c| {
        grant(c, &share_id, identity.user_id)?;
        let mut statement = c.prepare(
            "SELECT u.id, u.username, m.role, m.read_code FROM v3_share_members m
             JOIN auth_users u ON u.id = m.user_id WHERE m.share_id = ?1 ORDER BY u.username",
        )?;
        let rows = statement
            .query_map(params![share_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut devices = c.prepare(
            "SELECT device_id FROM v3_share_devices WHERE share_id = ?1 AND user_id = ?2
             ORDER BY device_id",
        )?;
        let mut out = Vec::new();
        for (id, username, role, read_code) in rows {
            let list = devices
                .query_map(params![share_id, id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            out.push(ShareMemberInfo {
                username,
                role: parse_role(&role),
                read_code: read_code != 0,
                devices: list,
            });
        }
        Ok(out)
    })
    .await?;
    Ok(Json(out))
}

pub(crate) async fn bind_signing_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SigningKeyRequest>,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&input.device_id)?;
    let ok_key = STANDARD
        .decode(&input.public_key)
        .is_ok_and(|b| b.len() == 32);
    if !ok_key {
        return Err(ShareError::Invalid("public_key must be 32 bytes of base64"));
    }
    let now = state.auth.now();
    write(&state, move |c| {
        let existing: Option<(i64, String)> = c
            .query_row(
                "SELECT user_id, public_key FROM v3_signing_keys WHERE device_id = ?1",
                params![input.device_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((user, key)) if user == identity.user_id && key == input.public_key => Ok(()),
            Some(_) => Err(ShareError::Conflict(
                "device already has a different signing key",
            )),
            None => {
                c.execute(
                    "INSERT INTO v3_signing_keys (device_id, user_id, public_key, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![input.device_id, identity.user_id, input.public_key, now],
                )?;
                Ok(())
            }
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn signing_keys(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<SigningKeyBinding>>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let out = read(&state, move |c| {
        grant(c, &share_id, identity.user_id)?;
        let mut statement = c.prepare(
            "SELECT u.username, k.device_id, k.public_key FROM v3_signing_keys k
             JOIN auth_users u ON u.id = k.user_id
             JOIN v3_share_members m ON m.user_id = k.user_id AND m.share_id = ?1
             ORDER BY k.device_id",
        )?;
        let rows = statement
            .query_map(params![share_id], |r| {
                Ok(SigningKeyBinding {
                    username: r.get(0)?,
                    device_id: r.get(1)?,
                    public_key: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await?;
    Ok(Json(out))
}
