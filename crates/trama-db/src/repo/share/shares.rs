//! Shares: scope, policy, status and the snapshot version pin.

use super::{invalid, to_i64, to_u64};
use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{
    DomainError, Result,
    share::{ServerId, ShareId, SharePolicy, ShareScope, ThreadGid},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareStatus {
    Preparing,
    Incomplete,
    Ready,
    Paused,
    Closed,
}

impl ShareStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Incomplete => "incomplete",
            Self::Ready => "ready",
            Self::Paused => "paused",
            Self::Closed => "closed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "preparing" => Self::Preparing,
            "incomplete" => Self::Incomplete,
            "ready" => Self::Ready,
            "paused" => Self::Paused,
            "closed" => Self::Closed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShareRow {
    pub share_id: ShareId,
    pub server_id: Option<ServerId>,
    pub scope: ShareScope,
    pub policy: SharePolicy,
    pub root_gid: Option<ThreadGid>,
    pub current_snapshot_version: u64,
    pub status: ShareStatus,
    pub created_at: String,
}

const COLUMNS: &str = "share_id, server_id, scope, policy_json, root_gid,
    current_snapshot_version, status, created_at";

impl TramaDb {
    /// Inserts a new share. An existing `share_id` is an error.
    pub async fn insert_share(&self, row: ShareRow) -> Result<()> {
        let policy = serde_json::to_string(&row.policy)?;
        let version = to_i64(row.current_snapshot_version, "snapshot version")?;
        self.blocking(move |conn| {
            conn.execute(
                &format!("INSERT INTO shares ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)"),
                params![
                    row.share_id.0,
                    row.server_id.map(|s| s.0),
                    row.scope.as_str(),
                    policy,
                    row.root_gid.map(|g| g.0),
                    version,
                    row.status.as_str(),
                    row.created_at
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn share(&self, share_id: &ShareId) -> Result<Option<ShareRow>> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let raw = conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM shares WHERE share_id=?1"),
                    [id],
                    raw_row,
                )
                .optional()
                .map_err(sql_err)?;
            raw.map(decode).transpose()
        })
        .await
    }

    pub async fn shares(&self) -> Result<Vec<ShareRow>> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM shares ORDER BY created_at, share_id"
                ))
                .map_err(sql_err)?;
            let rows = stmt.query_map([], raw_row).map_err(sql_err)?;
            rows.map(|r| r.map_err(sql_err).and_then(decode)).collect()
        })
        .await
    }

    pub async fn set_share_status(&self, share_id: &ShareId, status: ShareStatus) -> Result<()> {
        let id = share_id.0.clone();
        self.blocking(move |conn| {
            let current: Option<String> = conn
                .query_row("SELECT status FROM shares WHERE share_id=?1", [&id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(sql_err)?;
            match current.as_deref() {
                None => Err(DomainError::NotFound(format!("share {id}"))),
                Some("closed") if status != ShareStatus::Closed => Err(DomainError::Invalid(
                    "a closed share cannot be reopened".into(),
                )),
                Some(_) => {
                    conn.execute(
                        "UPDATE shares SET status=?2 WHERE share_id=?1",
                        params![id, status.as_str()],
                    )
                    .map_err(sql_err)?;
                    Ok(())
                }
            }
        })
        .await
    }

    /// Compare-and-swap of the snapshot version pin. The new version must be
    /// strictly greater (rollback protection, plan 06 §7); returns `false`
    /// when `expected` no longer matches.
    pub async fn advance_share_snapshot(
        &self,
        share_id: &ShareId,
        expected: u64,
        next: u64,
    ) -> Result<bool> {
        if next <= expected {
            return Err(DomainError::Invalid(
                "snapshot version must increase".into(),
            ));
        }
        let id = share_id.0.clone();
        let expected = to_i64(expected, "snapshot version")?;
        let next = to_i64(next, "snapshot version")?;
        self.blocking(move |conn| {
            let changed = conn
                .execute(
                    "UPDATE shares SET current_snapshot_version=?3
                     WHERE share_id=?1 AND current_snapshot_version=?2",
                    params![id, expected, next],
                )
                .map_err(sql_err)?;
            Ok(changed == 1)
        })
        .await
    }
}

type RawShare = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    i64,
    String,
    String,
);

fn raw_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawShare> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

fn decode(raw: RawShare) -> Result<ShareRow> {
    let (share_id, server_id, scope, policy, root_gid, version, status, created_at) = raw;
    Ok(ShareRow {
        share_id: ShareId(share_id),
        server_id: server_id.map(ServerId),
        scope: ShareScope::parse(&scope).ok_or_else(|| invalid("share scope", &scope))?,
        policy: serde_json::from_str(&policy)?,
        root_gid: root_gid.map(ThreadGid),
        current_snapshot_version: to_u64(version, "snapshot version")?,
        status: ShareStatus::parse(&status).ok_or_else(|| invalid("share status", &status))?,
        created_at,
    })
}
