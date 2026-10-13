//! Threads inside a share's perimeter, mapped to local thread ids.

use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{
    Result,
    ids::ThreadId,
    share::{ShareId, ThreadGid},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareThreadRow {
    pub share_id: ShareId,
    pub thread_gid: ThreadGid,
    pub local_thread_id: Option<String>,
    /// `None` at the share boundary.
    pub parent_gid: Option<ThreadGid>,
    pub source_scope_root: bool,
    pub kind: String,
}

const COLUMNS: &str = "share_id, thread_gid, local_thread_id, parent_gid, source_scope_root, kind";

impl TramaDb {
    /// Inserts or refreshes perimeter rows atomically.
    pub async fn upsert_share_threads(&self, rows: Vec<ShareThreadRow>) -> Result<()> {
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for row in rows {
                tx.execute(
                    &format!(
                        "INSERT INTO share_threads ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6)
                         ON CONFLICT(share_id, thread_gid) DO UPDATE SET
                           local_thread_id=excluded.local_thread_id,
                           parent_gid=excluded.parent_gid,
                           source_scope_root=excluded.source_scope_root,
                           kind=excluded.kind"
                    ),
                    params![
                        row.share_id.0,
                        row.thread_gid.0,
                        row.local_thread_id,
                        row.parent_gid.map(|g| g.0),
                        row.source_scope_root,
                        row.kind
                    ],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)
        })
        .await
    }

    pub async fn share_threads(&self, share_id: &ShareId) -> Result<Vec<ShareThreadRow>> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM share_threads WHERE share_id=?1 ORDER BY thread_gid"
                ))
                .map_err(sql_err)?;
            let rows = stmt.query_map([id], thread_row).map_err(sql_err)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
        })
        .await
    }

    /// Global id of a local thread inside a share, if mapped.
    pub async fn share_thread_gid(
        &self,
        share_id: &ShareId,
        local: &ThreadId,
    ) -> Result<Option<ThreadGid>> {
        let (id, local) = (share_id.0.clone(), local.to_string());
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT thread_gid FROM share_threads WHERE share_id=?1 AND local_thread_id=?2",
                params![id, local],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map(|gid| gid.map(ThreadGid))
            .map_err(sql_err)
        })
        .await
    }

    /// Removes a thread from the perimeter (its checkout states cascade).
    pub async fn remove_share_thread(&self, share_id: &ShareId, gid: &ThreadGid) -> Result<bool> {
        let (id, gid) = (share_id.0.clone(), gid.0.clone());
        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM share_threads WHERE share_id=?1 AND thread_gid=?2",
                params![id, gid],
            )
            .map(|n| n == 1)
            .map_err(sql_err)
        })
        .await
    }
}

fn thread_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ShareThreadRow> {
    Ok(ShareThreadRow {
        share_id: ShareId(r.get(0)?),
        thread_gid: ThreadGid(r.get(1)?),
        local_thread_id: r.get(2)?,
        parent_gid: r.get::<_, Option<String>>(3)?.map(ThreadGid),
        source_scope_root: r.get(4)?,
        kind: r.get(5)?,
    })
}
