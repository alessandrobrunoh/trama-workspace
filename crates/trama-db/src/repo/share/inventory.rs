//! Encrypted object inventory and durable per-device acknowledgements.

use super::{invalid, to_i64, to_u64};
use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{
    DomainError, Result,
    share::{DeviceId, ShareId},
};

/// Lifecycle of an encrypted content object known locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectState {
    Pending,
    Uploaded,
    Downloaded,
    Verified,
    Orphaned,
}

impl ObjectState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Uploaded => "uploaded",
            Self::Downloaded => "downloaded",
            Self::Verified => "verified",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "uploaded" => Self::Uploaded,
            "downloaded" => Self::Downloaded,
            "verified" => Self::Verified,
            "orphaned" => Self::Orphaned,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareObjectRow {
    pub share_id: ShareId,
    pub content_id: String,
    pub epoch: u64,
    pub byte_len: u64,
    pub state: ObjectState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePeerAck {
    pub share_id: ShareId,
    pub device_id: DeviceId,
    pub event_cursor: String,
    pub content_watermark: Option<String>,
}

impl TramaDb {
    /// Records an object or moves it to a new state. The byte length of a
    /// known `(content_id, epoch)` is immutable.
    pub async fn upsert_share_object(&self, row: ShareObjectRow) -> Result<()> {
        let epoch = to_i64(row.epoch, "epoch")?;
        let len = to_i64(row.byte_len, "byte_len")?;
        self.blocking(move |conn| {
            let known: Option<i64> = conn
                .query_row(
                    "SELECT byte_len FROM share_object_inventory
                     WHERE share_id=?1 AND content_id=?2 AND epoch=?3",
                    params![row.share_id.0, row.content_id, epoch],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if known.is_some_and(|k| k != len) {
                return Err(DomainError::Invalid("object byte length changed".into()));
            }
            conn.execute(
                "INSERT INTO share_object_inventory (share_id, content_id, epoch, byte_len, state)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(share_id, content_id, epoch) DO UPDATE SET state=excluded.state",
                params![
                    row.share_id.0,
                    row.content_id,
                    epoch,
                    len,
                    row.state.as_str()
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn share_objects(
        &self,
        share_id: &ShareId,
        state: Option<ObjectState>,
    ) -> Result<Vec<ShareObjectRow>> {
        let id = share_id.0.clone();
        let state = state.map(|s| s.as_str());
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT share_id, content_id, epoch, byte_len, state FROM share_object_inventory
                     WHERE share_id=?1 AND (?2 IS NULL OR state=?2)
                     ORDER BY content_id, epoch",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![id, state], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(sql_err)?;
            rows.map(|r| {
                let (share, content_id, epoch, len, state) = r.map_err(sql_err)?;
                Ok(ShareObjectRow {
                    share_id: ShareId(share),
                    content_id,
                    epoch: to_u64(epoch, "epoch")?,
                    byte_len: to_u64(len, "byte_len")?,
                    state: ObjectState::parse(&state)
                        .ok_or_else(|| invalid("object state", &state))?,
                })
            })
            .collect()
        })
        .await
    }

    /// Stores a durable ack. Call only after the acknowledged data was
    /// persisted and verified (plan 03 §5).
    pub async fn put_share_peer_ack(&self, ack: SharePeerAck) -> Result<()> {
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO share_peer_acks (share_id, device_id, event_cursor, content_watermark)
                 VALUES (?1,?2,?3,?4)
                 ON CONFLICT(share_id, device_id) DO UPDATE SET
                   event_cursor=excluded.event_cursor,
                   content_watermark=excluded.content_watermark",
                params![
                    ack.share_id.0,
                    ack.device_id.0,
                    ack.event_cursor,
                    ack.content_watermark
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn share_peer_acks(&self, share_id: &ShareId) -> Result<Vec<SharePeerAck>> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT share_id, device_id, event_cursor, content_watermark
                     FROM share_peer_acks WHERE share_id=?1 ORDER BY device_id",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([id], |r| {
                    Ok(SharePeerAck {
                        share_id: ShareId(r.get(0)?),
                        device_id: DeviceId(r.get(1)?),
                        event_cursor: r.get(2)?,
                        content_watermark: r.get(3)?,
                    })
                })
                .map_err(sql_err)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
        })
        .await
    }
}
