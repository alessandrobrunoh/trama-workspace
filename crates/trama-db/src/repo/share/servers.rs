//! Collaboration servers known to this device, keyed by persistent identity.

use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, params};
use trama_domain::{DomainError, Result, share::ServerId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollaborationServerRow {
    pub server_id: ServerId,
    pub label: String,
    pub base_url: String,
    pub server_fingerprint: Option<String>,
    pub trust_policy: String,
    pub created_at: String,
}

impl TramaDb {
    /// Registers a server. A known `server_id` keeps its pinned fingerprint:
    /// changing it is refused instead of silently re-trusting the server.
    pub async fn upsert_collaboration_server(&self, row: CollaborationServerRow) -> Result<()> {
        self.blocking(move |conn| {
            let pinned: Option<Option<String>> = conn
                .query_row(
                    "SELECT server_fingerprint FROM collaboration_servers WHERE server_id=?1",
                    [&row.server_id.0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if let Some(Some(old)) = &pinned
                && row.server_fingerprint.as_ref() != Some(old)
            {
                return Err(DomainError::Invalid(
                    "server fingerprint differs from the pinned one".into(),
                ));
            }
            conn.execute(
                "INSERT INTO collaboration_servers
                   (server_id, label, base_url, server_fingerprint, trust_policy, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(server_id) DO UPDATE SET
                   label=excluded.label, base_url=excluded.base_url,
                   server_fingerprint=excluded.server_fingerprint,
                   trust_policy=excluded.trust_policy",
                params![
                    row.server_id.0,
                    row.label,
                    row.base_url,
                    row.server_fingerprint,
                    row.trust_policy,
                    row.created_at
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn collaboration_server(
        &self,
        server_id: &ServerId,
    ) -> Result<Option<CollaborationServerRow>> {
        let id = server_id.0.clone();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT server_id, label, base_url, server_fingerprint, trust_policy, created_at
                 FROM collaboration_servers WHERE server_id=?1",
                [id],
                server_row,
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    pub async fn collaboration_servers(&self) -> Result<Vec<CollaborationServerRow>> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT server_id, label, base_url, server_fingerprint, trust_policy, created_at
                     FROM collaboration_servers ORDER BY created_at, server_id",
                )
                .map_err(sql_err)?;
            let rows = stmt.query_map([], server_row).map_err(sql_err)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql_err)
        })
        .await
    }
}

fn server_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CollaborationServerRow> {
    Ok(CollaborationServerRow {
        server_id: ServerId(r.get(0)?),
        label: r.get(1)?,
        base_url: r.get(2)?,
        server_fingerprint: r.get(3)?,
        trust_policy: r.get(4)?,
        created_at: r.get(5)?,
    })
}
