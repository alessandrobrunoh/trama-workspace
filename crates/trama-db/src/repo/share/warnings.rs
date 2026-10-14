//! Share warnings (migration 0016), kept per share up to [`MAX_WARNINGS`].

use crate::{TramaDb, codec::sql_err};
use rusqlite::params;
use trama_domain::Result;

pub const MAX_WARNINGS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareWarningRow {
    pub code: String,
    pub message: String,
    pub created_at: String,
}

impl TramaDb {
    pub async fn add_share_warning(&self, share_id: &str, code: &str, message: &str) -> Result<()> {
        let (share, code, message) = (share_id.to_owned(), code.to_owned(), message.to_owned());
        let now = chrono::Utc::now().to_rfc3339();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "INSERT INTO share_warnings (share_id, code, message, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![share, code, message, now],
            )
            .map_err(sql_err)?;
            tx.execute(
                "DELETE FROM share_warnings WHERE share_id = ?1 AND id NOT IN
                 (SELECT id FROM share_warnings WHERE share_id = ?1 ORDER BY id DESC LIMIT ?2)",
                params![share, MAX_WARNINGS as i64],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)
        })
        .await
    }

    /// Newest first.
    pub async fn share_warnings(&self, share_id: &str) -> Result<Vec<ShareWarningRow>> {
        let share = share_id.to_owned();
        self.blocking(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT code, message, created_at FROM share_warnings
                     WHERE share_id = ?1 ORDER BY id DESC",
                )
                .map_err(sql_err)?;
            statement
                .query_map([share], |r| {
                    Ok(ShareWarningRow {
                        code: r.get(0)?,
                        message: r.get(1)?,
                        created_at: r.get(2)?,
                    })
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn warnings_are_bounded_and_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::TramaDb::open(&dir.path().join("t.db")).unwrap();
        for i in 0..(super::MAX_WARNINGS + 3) {
            db.add_share_warning("s1", "content_skipped", &format!("w{i}"))
                .await
                .unwrap();
        }
        db.add_share_warning("s2", "x", "other").await.unwrap();
        let rows = db.share_warnings("s1").await.unwrap();
        assert_eq!(rows.len(), super::MAX_WARNINGS);
        assert_eq!(rows[0].message, format!("w{}", super::MAX_WARNINGS + 2));
    }
}
