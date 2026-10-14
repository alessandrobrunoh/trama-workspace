//! MLS join plumbing: a granted device posts a key package, the owner
//! lists pending ones and posts the Welcome, the device fetches it. All
//! blobs are opaque base64; the server never parses MLS.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use trama_collab_protocol::vnext::api::{
    KeyPackageRequest, MAX_MLS_BLOB_BYTES, PendingKeyPackage, ShareRole, WelcomeBlob,
};

use super::{ShareError, grant, read, validate_id, write};
use crate::{AppState, collab};

fn check_blob(blob: &str) -> Result<(), ShareError> {
    if blob.is_empty() || blob.len() > MAX_MLS_BLOB_BYTES || STANDARD.decode(blob).is_err() {
        return Err(ShareError::Invalid("invalid MLS blob"));
    }
    Ok(())
}

pub(crate) fn migrate(c: &rusqlite::Connection) -> Result<(), String> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS v3_share_key_packages (
             share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
             device_id TEXT NOT NULL,
             user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
             key_package TEXT NOT NULL,
             welcome TEXT,
             epoch INTEGER,
             PRIMARY KEY (share_id, device_id)
         );",
    )
    .map_err(|e| format!("initialize key package schema: {e}"))
}

pub(crate) async fn post_key_package(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<KeyPackageRequest>,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&input.device_id)?;
    check_blob(&input.key_package)?;
    write(&state, move |c| {
        grant(c, &share_id, identity.user_id)?;
        let bound: Option<i64> = c
            .query_row(
                "SELECT user_id FROM v3_share_devices WHERE share_id = ?1 AND device_id = ?2",
                params![share_id, input.device_id],
                |r| r.get(0),
            )
            .optional()?;
        match bound {
            Some(user) if user != identity.user_id => {
                return Err(ShareError::Forbidden("device belongs to another member"));
            }
            Some(_) => {}
            None => {
                c.execute(
                    "INSERT INTO v3_share_devices (share_id, device_id, user_id) VALUES (?1, ?2, ?3)",
                    params![share_id, input.device_id, identity.user_id],
                )?;
            }
        }
        c.execute(
            "INSERT INTO v3_share_key_packages (share_id, device_id, user_id, key_package)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (share_id, device_id)
             DO UPDATE SET key_package = ?4, welcome = NULL, epoch = NULL",
            params![share_id, input.device_id, identity.user_id, input.key_package],
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn pending(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<PendingKeyPackage>>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let rows = read(&state, move |c| {
        if grant(c, &share_id, identity.user_id)?.role != ShareRole::Owner {
            return Err(ShareError::Forbidden("only the share owner can do that"));
        }
        let mut statement = c.prepare(
            "SELECT u.username, k.device_id, k.key_package
             FROM v3_share_key_packages k JOIN auth_users u ON u.id = k.user_id
             WHERE k.share_id = ?1 AND k.welcome IS NULL ORDER BY k.device_id",
        )?;
        let rows = statement
            .query_map(params![share_id], |r| {
                Ok(PendingKeyPackage {
                    username: r.get(0)?,
                    device_id: r.get(1)?,
                    key_package: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await?;
    Ok(Json(rows))
}

pub(crate) async fn post_welcome(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<WelcomeBlob>,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&input.device_id)?;
    check_blob(&input.welcome)?;
    let epoch = i64::try_from(input.epoch).map_err(|_| ShareError::Invalid("epoch"))?;
    write(&state, move |c| {
        if grant(c, &share_id, identity.user_id)?.role != ShareRole::Owner {
            return Err(ShareError::Forbidden("only the share owner can do that"));
        }
        let changed = c.execute(
            "UPDATE v3_share_key_packages SET welcome = ?3, epoch = ?4
             WHERE share_id = ?1 AND device_id = ?2",
            params![share_id, input.device_id, input.welcome, epoch],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(ShareError::NotFound)
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn get_welcome(
    State(state): State<AppState>,
    Path((share_id, device_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<WelcomeBlob>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&device_id)?;
    let blob = read(&state, move |c| {
        grant(c, &share_id, identity.user_id)?;
        let row: Option<(String, i64)> = c
            .query_row(
                "SELECT welcome, epoch FROM v3_share_key_packages
                 WHERE share_id = ?1 AND device_id = ?2 AND user_id = ?3
                   AND welcome IS NOT NULL",
                params![share_id, device_id, identity.user_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (welcome, epoch) = row.ok_or(ShareError::NotFound)?;
        Ok(WelcomeBlob {
            device_id,
            welcome,
            epoch: epoch as u64,
        })
    })
    .await?;
    Ok(Json(blob))
}
