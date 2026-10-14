//! Replay heads and the stream cursor of a share (migration 0013).

use super::{to_i64, to_u64};
use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{Result, share::ShareId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareDeviceHead {
    pub device_id: String,
    pub author_seq: u64,
    pub event_id: String,
}

impl TramaDb {
    /// Records an applied event and advances the stream cursor atomically.
    /// Heads and the cursor only move forward.
    pub async fn record_share_event(
        &self,
        share_id: &ShareId,
        head: ShareDeviceHead,
        server_seq: Option<u64>,
    ) -> Result<()> {
        let id = share_id.0.clone();
        let seq = to_i64(head.author_seq, "author_seq")?;
        let cursor = server_seq.map(|s| to_i64(s, "server_seq")).transpose()?;
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "INSERT INTO share_device_heads (share_id, device_id, author_seq, event_id)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (share_id, device_id) DO UPDATE SET
                   author_seq = excluded.author_seq, event_id = excluded.event_id
                 WHERE excluded.author_seq > share_device_heads.author_seq",
                params![id, head.device_id, seq, head.event_id],
            )
            .map_err(sql_err)?;
            if let Some(cursor) = cursor {
                tx.execute(
                    "INSERT INTO share_stream_cursors (share_id, server_seq) VALUES (?1, ?2)
                     ON CONFLICT (share_id) DO UPDATE SET server_seq = excluded.server_seq
                     WHERE excluded.server_seq > share_stream_cursors.server_seq",
                    params![id, cursor],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)
        })
        .await
    }

    pub async fn share_device_heads(&self, share_id: &ShareId) -> Result<Vec<ShareDeviceHead>> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT device_id, author_seq, event_id FROM share_device_heads
                     WHERE share_id = ?1 ORDER BY device_id",
                )
                .map_err(sql_err)?;
            let rows = statement
                .query_map([&id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get(2)?))
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            rows.into_iter()
                .map(|(device_id, seq, event_id)| {
                    Ok(ShareDeviceHead {
                        device_id,
                        author_seq: to_u64(seq, "author_seq")?,
                        event_id,
                    })
                })
                .collect()
        })
        .await
    }

    /// 0 when nothing was applied yet.
    pub async fn share_stream_cursor(&self, share_id: &ShareId) -> Result<u64> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let seq: Option<i64> = conn
                .query_row(
                    "SELECT server_seq FROM share_stream_cursors WHERE share_id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            to_u64(seq.unwrap_or(0), "server_seq")
        })
        .await
    }
}
