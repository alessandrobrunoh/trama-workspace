//! Encrypted object parts (message slices and Git bundles). The server
//! stores opaque ciphertext; only the owner writes, parts are immutable
//! (an identical re-upload is accepted, different bytes are a conflict),
//! and `content` objects need `read_code`.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use trama_collab_protocol::vnext::api::{
    MAX_OBJECT_PART_BYTES, MAX_OBJECT_PARTS, ObjectClass, ShareRole,
};

use super::{ShareError, grant, read, validate_id, write};
use crate::{AppState, collab};

pub(crate) fn migrate(c: &rusqlite::Connection) -> Result<(), String> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS v3_share_objects (
             share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
             content_id TEXT NOT NULL,
             part INTEGER NOT NULL,
             class TEXT NOT NULL CHECK (class IN ('history', 'content')),
             data BLOB NOT NULL,
             PRIMARY KEY (share_id, content_id, part)
         );",
    )
    .map_err(|e| format!("initialize object schema: {e}"))
}

#[derive(Deserialize)]
pub(crate) struct ClassQuery {
    class: ObjectClass,
}

fn class_str(class: ObjectClass) -> &'static str {
    match class {
        ObjectClass::History => "history",
        ObjectClass::Content => "content",
    }
}

pub(crate) async fn put_part(
    State(state): State<AppState>,
    Path((share_id, content_id, part)): Path<(String, String, u32)>,
    Query(query): Query<ClassQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&content_id)?;
    if part >= MAX_OBJECT_PARTS {
        return Err(ShareError::Invalid("too many parts"));
    }
    if body.is_empty() || body.len() > MAX_OBJECT_PART_BYTES {
        return Err(ShareError::Invalid("part size out of range"));
    }
    let class = class_str(query.class);
    write(&state, move |c| {
        if grant(c, &share_id, identity.user_id)?.role != ShareRole::Owner {
            return Err(ShareError::Forbidden(
                "only the share owner uploads objects",
            ));
        }
        let existing: Option<(String, Vec<u8>)> = c
            .query_row(
                "SELECT class, data FROM v3_share_objects
                 WHERE share_id = ?1 AND content_id = ?2 AND part = ?3",
                params![share_id, content_id, part],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((cls, data)) if cls == class && data == body.as_ref() => Ok(()),
            Some(_) => Err(ShareError::Conflict("object parts are immutable")),
            None => {
                c.execute(
                    "INSERT INTO v3_share_objects (share_id, content_id, part, class, data)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![share_id, content_id, part, class, body.as_ref()],
                )?;
                Ok(())
            }
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn get_part(
    State(state): State<AppState>,
    Path((share_id, content_id, part)): Path<(String, String, u32)>,
    headers: HeaderMap,
) -> Result<Vec<u8>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    validate_id(&content_id)?;
    read(&state, move |c| {
        let g = grant(c, &share_id, identity.user_id)?;
        let row: Option<(String, Vec<u8>)> = c
            .query_row(
                "SELECT class, data FROM v3_share_objects
                 WHERE share_id = ?1 AND content_id = ?2 AND part = ?3",
                params![share_id, content_id, part],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (class, data) = row.ok_or(ShareError::NotFound)?;
        if class == "content" && !g.read_code {
            return Err(ShareError::Forbidden("code access is not granted"));
        }
        Ok(data)
    })
    .await
}
