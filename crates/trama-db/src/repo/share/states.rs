//! Per-message share state and the durable queue of pending share
//! operations (migration 0015).

use crate::{TramaDb, codec::sql_err};
use rusqlite::params;
use trama_domain::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareMessageState {
    pub message_id: String,
    pub share_id: String,
    /// `shared` or `not_shared_no_lease`.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharePendingOp {
    pub id: i64,
    pub share_id: String,
    pub kind: String,
    pub body: String,
    pub attempts: u32,
}

impl TramaDb {
    pub async fn record_share_message_states(
        &self,
        thread_id: &str,
        rows: Vec<ShareMessageState>,
    ) -> Result<()> {
        let thread = thread_id.to_owned();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for row in rows {
                tx.execute(
                    "INSERT INTO share_message_states (message_id, share_id, thread_id, state)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (message_id, share_id) DO UPDATE SET state = excluded.state",
                    params![row.message_id, row.share_id, thread, row.state],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)
        })
        .await
    }

    pub async fn share_message_states(&self, thread_id: &str) -> Result<Vec<ShareMessageState>> {
        let thread = thread_id.to_owned();
        self.blocking(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT message_id, share_id, state FROM share_message_states
                     WHERE thread_id = ?1 ORDER BY rowid",
                )
                .map_err(sql_err)?;
            statement
                .query_map([thread], |r| {
                    Ok(ShareMessageState {
                        message_id: r.get(0)?,
                        share_id: r.get(1)?,
                        state: r.get(2)?,
                    })
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)
        })
        .await
    }

    pub async fn enqueue_share_op(&self, share_id: &str, kind: &str, body: &str) -> Result<i64> {
        let (share, kind, body) = (share_id.to_owned(), kind.to_owned(), body.to_owned());
        let now = chrono::Utc::now().to_rfc3339();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO share_pending_ops (share_id, kind, body, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![share, kind, body, now],
            )
            .map_err(sql_err)?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// Oldest first.
    pub async fn share_pending_ops(&self) -> Result<Vec<SharePendingOp>> {
        self.blocking(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT id, share_id, kind, body, attempts FROM share_pending_ops ORDER BY id",
                )
                .map_err(sql_err)?;
            statement
                .query_map([], |r| {
                    Ok(SharePendingOp {
                        id: r.get(0)?,
                        share_id: r.get(1)?,
                        kind: r.get(2)?,
                        body: r.get(3)?,
                        attempts: r.get::<_, i64>(4)? as u32,
                    })
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)
        })
        .await
    }

    pub async fn finish_share_op(&self, id: i64) -> Result<()> {
        self.blocking(move |conn| {
            conn.execute("DELETE FROM share_pending_ops WHERE id = ?1", [id])
                .map(|_| ())
                .map_err(sql_err)
        })
        .await
    }

    pub async fn fail_share_op(&self, id: i64, error: &str) -> Result<()> {
        let error = error.chars().take(500).collect::<String>();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE share_pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
                params![id, error],
            )
            .map(|_| ())
            .map_err(sql_err)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::TramaDb;

    use super::ShareMessageState;

    #[tokio::test]
    async fn states_and_pending_ops_round_trip() {
        let db = TramaDb::open_in_memory().unwrap();
        let row = |id: &str, state: &str| ShareMessageState {
            message_id: id.into(),
            share_id: "sh1".into(),
            state: state.into(),
        };
        db.record_share_message_states(
            "t1",
            vec![row("m1", "shared"), row("m2", "not_shared_no_lease")],
        )
        .await
        .unwrap();
        db.record_share_message_states("t1", vec![row("m2", "shared")])
            .await
            .unwrap();
        let got = db.share_message_states("t1").await.unwrap();
        assert_eq!(got, vec![row("m1", "shared"), row("m2", "shared")]);
        let id = db.enqueue_share_op("sh1", "event", "{}").await.unwrap();
        db.fail_share_op(id, "offline").await.unwrap();
        let ops = db.share_pending_ops().await.unwrap();
        assert_eq!((ops.len(), ops[0].attempts), (1, 1));
        db.finish_share_op(id).await.unwrap();
        assert!(db.share_pending_ops().await.unwrap().is_empty());
    }
}
