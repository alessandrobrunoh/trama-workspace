//! Compare-and-swap manifest commits and fetches. Manifest parts are opaque
//! base64 ciphertext; the content part is served only with `read_code`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use trama_collab_protocol::vnext::api::{
    CommitManifestRequest, MAX_MANIFEST_CIPHERTEXT_BYTES, ManifestBlob, ManifestPart,
    ShareServerFrame,
};

use super::{ShareError, grant, read, validate_id, write};
use crate::{AppState, collab};

fn check_blob(blob: &str) -> Result<(), ShareError> {
    if blob.is_empty() || blob.len() > MAX_MANIFEST_CIPHERTEXT_BYTES {
        return Err(ShareError::Invalid("manifest part size out of range"));
    }
    STANDARD
        .decode(blob)
        .map(|_| ())
        .map_err(|_| ShareError::Invalid("manifest part must be base64"))
}

pub(crate) async fn commit(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<CommitManifestRequest>,
) -> Result<StatusCode, ShareError> {
    let identity = collab::mutating_session(&state, &headers).await?;
    validate_id(&share_id)?;
    check_blob(&input.history)?;
    if let Some(content) = &input.content {
        check_blob(content)?;
    }
    if input.expected_version.checked_add(1) != Some(input.version)
        || input.version > i64::MAX as u64
    {
        return Err(ShareError::Invalid("version must be expected_version + 1"));
    }
    let now = state.auth.now();
    let id = share_id.clone();
    let version = input.version;
    write(&state, move |c| {
        if !grant(c, &id, identity.user_id)?.can_write() {
            return Err(ShareError::Forbidden("read-only member"));
        }
        let changed = c.execute(
            "UPDATE v3_shares SET manifest_version = ?3
             WHERE share_id = ?1 AND manifest_version = ?2",
            params![id, input.expected_version as i64, input.version as i64],
        )?;
        if changed != 1 {
            return Err(ShareError::Conflict("manifest version changed"));
        }
        c.execute(
            "INSERT INTO v3_share_manifests
             (share_id, version, history, content, publisher_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                input.version as i64,
                input.history,
                input.content,
                identity.user_id,
                now
            ],
        )?;
        Ok(())
    })
    .await?;
    state.shares.send(
        &share_id,
        vec![ShareServerFrame::ManifestCommitted { version }],
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(crate) struct PartQuery {
    #[serde(default)]
    part: Option<ManifestPart>,
}

/// `version` 0 is the latest committed version.
pub(crate) async fn fetch(
    State(state): State<AppState>,
    Path((share_id, version)): Path<(String, u64)>,
    Query(query): Query<PartQuery>,
    headers: HeaderMap,
) -> Result<Json<ManifestBlob>, ShareError> {
    let identity = collab::session(&state, &headers).await?;
    validate_id(&share_id)?;
    let part = query.part.unwrap_or(ManifestPart::History);
    let blob = read(&state, move |c| {
        let g = grant(c, &share_id, identity.user_id)?;
        if part == ManifestPart::Content && !g.read_code {
            return Err(ShareError::Forbidden("no read_code grant"));
        }
        let version = if version == 0 {
            c.query_row(
                "SELECT manifest_version FROM v3_shares WHERE share_id = ?1",
                params![share_id],
                |r| r.get::<_, i64>(0),
            )?
        } else {
            i64::try_from(version).map_err(|_| ShareError::NotFound)?
        };
        let row: Option<(String, Option<String>)> = c
            .query_row(
                "SELECT history, content FROM v3_share_manifests
                 WHERE share_id = ?1 AND version = ?2",
                params![share_id, version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (history, content) = row.ok_or(ShareError::NotFound)?;
        let ciphertext = match part {
            ManifestPart::History => history,
            ManifestPart::Content => content.ok_or(ShareError::NotFound)?,
        };
        Ok(ManifestBlob {
            version: version as u64,
            part,
            ciphertext,
        })
    })
    .await?;
    Ok(Json(blob))
}
