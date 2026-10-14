//! Immutable captured checkout generations per (share, thread, repository).

use super::{to_i64, to_u64};
use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{
    DomainError, Result,
    share::{RepoGid, ShareId, ThreadGid},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareCheckoutStateRow {
    pub share_id: ShareId,
    pub thread_gid: ThreadGid,
    pub repo_gid: RepoGid,
    pub generation: u64,
    pub previous_generation: Option<u64>,
    pub head_oid: String,
    pub index_tree_oid: String,
    pub working_tree_oid: String,
    pub manifest_content_id: String,
    pub status: String,
    pub created_at: String,
}

const COLUMNS: &str = "share_id, thread_gid, repo_gid, generation, previous_generation,
    head_oid, index_tree_oid, working_tree_oid, manifest_content_id, status, created_at";

impl TramaDb {
    /// Appends a generation. Rows are immutable: `previous_generation` must
    /// be the current latest one (CAS, plan 03 §5), `None` only for the first.
    pub async fn append_share_checkout_state(&self, row: ShareCheckoutStateRow) -> Result<()> {
        let generation = to_i64(row.generation, "generation")?;
        let previous = row
            .previous_generation
            .map(|p| to_i64(p, "generation"))
            .transpose()?;
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let latest: Option<i64> = tx
                .query_row(
                    "SELECT MAX(generation) FROM share_checkout_states
                     WHERE share_id=?1 AND thread_gid=?2 AND repo_gid=?3",
                    params![row.share_id.0, row.thread_gid.0, row.repo_gid.0],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            if latest != previous {
                return Err(DomainError::Invalid(
                    "checkout generation conflict: previous generation is not the latest".into(),
                ));
            }
            tx.execute(
                &format!(
                    "INSERT INTO share_checkout_states ({COLUMNS})
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)"
                ),
                params![
                    row.share_id.0,
                    row.thread_gid.0,
                    row.repo_gid.0,
                    generation,
                    previous,
                    row.head_oid,
                    row.index_tree_oid,
                    row.working_tree_oid,
                    row.manifest_content_id,
                    row.status,
                    row.created_at
                ],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)
        })
        .await
    }

    pub async fn latest_share_checkout_state(
        &self,
        share_id: &ShareId,
        thread_gid: &ThreadGid,
        repo_gid: &RepoGid,
    ) -> Result<Option<ShareCheckoutStateRow>> {
        let keys = (share_id.0.clone(), thread_gid.0.clone(), repo_gid.0.clone());
        self.blocking(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM share_checkout_states
                     WHERE share_id=?1 AND thread_gid=?2 AND repo_gid=?3
                     ORDER BY generation DESC LIMIT 1"
                ),
                params![keys.0, keys.1, keys.2],
                raw_row,
            )
            .optional()
            .map_err(sql_err)?
            .map(decode)
            .transpose()
        })
        .await
    }

    /// All generations of a share, ordered by thread, repository, generation.
    pub async fn share_checkout_states(
        &self,
        share_id: &ShareId,
    ) -> Result<Vec<ShareCheckoutStateRow>> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM share_checkout_states WHERE share_id=?1
                     ORDER BY thread_gid, repo_gid, generation"
                ))
                .map_err(sql_err)?;
            let rows = stmt.query_map([id], raw_row).map_err(sql_err)?;
            rows.map(|r| r.map_err(sql_err).and_then(decode)).collect()
        })
        .await
    }
}

struct Raw {
    row: ShareCheckoutStateRow,
    generation: i64,
    previous: Option<i64>,
}

fn raw_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        row: ShareCheckoutStateRow {
            share_id: ShareId(r.get(0)?),
            thread_gid: ThreadGid(r.get(1)?),
            repo_gid: RepoGid(r.get(2)?),
            generation: 0,
            previous_generation: None,
            head_oid: r.get(5)?,
            index_tree_oid: r.get(6)?,
            working_tree_oid: r.get(7)?,
            manifest_content_id: r.get(8)?,
            status: r.get(9)?,
            created_at: r.get(10)?,
        },
        generation: r.get(3)?,
        previous: r.get(4)?,
    })
}

fn decode(raw: Raw) -> Result<ShareCheckoutStateRow> {
    let mut row = raw.row;
    row.generation = to_u64(raw.generation, "generation")?;
    row.previous_generation = raw.previous.map(|p| to_u64(p, "generation")).transpose()?;
    Ok(row)
}
