//! Collab outbox queue and family forgetting.

use super::*;

impl TramaDb {
    /// Enqueue `jobs` (skipping any whose dedup key or command id exists)
    /// and record `family` rows (ignoring existing ones), atomically.
    /// Returns the number of new jobs.
    pub async fn enqueue_collab_outbox(
        &self,
        jobs: Vec<OutboxJob>,
        family: Vec<FamilyRow>,
    ) -> Result<usize> {
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let now = rfc3339(Utc::now());
            for row in &family {
                tx.execute(
                    "INSERT OR IGNORE INTO collab_family (thread_id, root_thread_id, origin, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        row.thread_id.to_string(),
                        row.root_thread_id.to_string(),
                        row.origin,
                        now
                    ],
                )
                .map_err(sql_err)?;
            }
            let mut added = 0;
            for job in &jobs {
                added += tx
                    .execute(
                        "INSERT OR IGNORE INTO collab_outbox
                            (dedup_key, command_id, thread_id, job, ref_id, payload_json,
                             next_attempt_at, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                        params![
                            job.dedup_key,
                            job.command_id,
                            job.thread_id.to_string(),
                            job.job,
                            job.ref_id,
                            job.payload_json,
                            now
                        ],
                    )
                    .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(added)
        })
        .await
    }

    /// Whether a job with this dedup key was ever enqueued.
    pub async fn collab_outbox_has(&self, dedup_key: &str) -> Result<bool> {
        let key = dedup_key.to_owned();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT 1 FROM collab_outbox WHERE dedup_key = ?1",
                [key],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(sql_err)
        })
        .await
    }

    /// The oldest pending rows, in delivery order.
    pub async fn collab_outbox_pending(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {OUTBOX_COLUMNS} FROM collab_outbox WHERE state = 'pending'
                     ORDER BY id ASC LIMIT ?1"
                ))
                .map_err(sql_err)?;
            let mut rows = stmt.query([limit as i64]).map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_err)? {
                out.push(outbox_row(row)?);
            }
            Ok(out)
        })
        .await
    }

    /// Pending rows for any of `thread_ids`.
    pub async fn collab_outbox_pending_for(&self, thread_ids: &[ThreadId]) -> Result<usize> {
        let ids: Vec<String> = thread_ids.iter().map(ToString::to_string).collect();
        self.blocking(move |conn| {
            let mut total = 0usize;
            let mut stmt = conn
                .prepare(
                    "SELECT COUNT(*) FROM collab_outbox WHERE state = 'pending' AND thread_id = ?1",
                )
                .map_err(sql_err)?;
            for id in &ids {
                let count: i64 = stmt.query_row([id], |row| row.get(0)).map_err(sql_err)?;
                total += count as usize;
            }
            Ok(total)
        })
        .await
    }

    /// Mark rows delivered; their captured payload is dropped.
    pub async fn collab_outbox_delivered(&self, ids: &[i64]) -> Result<()> {
        let ids = ids.to_vec();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let now = rfc3339(Utc::now());
            for id in &ids {
                tx.execute(
                    "UPDATE collab_outbox SET state = 'delivered', delivered_at = ?2,
                         payload_json = NULL, last_error = NULL
                     WHERE id = ?1",
                    params![id, now],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// A permanent failure: the row is never retried.
    pub async fn collab_outbox_failed(&self, id: i64, error: &str) -> Result<()> {
        let error = error.chars().take(500).collect::<String>();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE collab_outbox SET state = 'failed', last_error = ?2, attempts = attempts + 1
                 WHERE id = ?1",
                params![id, error],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// A transient failure: retry the row (and everything after it) later.
    pub async fn collab_outbox_retry_at(
        &self,
        id: i64,
        at: DateTime<Utc>,
        error: &str,
    ) -> Result<()> {
        let error = error.chars().take(500).collect::<String>();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE collab_outbox SET attempts = attempts + 1, next_attempt_at = ?2,
                     last_error = ?3
                 WHERE id = ?1",
                params![id, rfc3339(at), error],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Count of rows per state (`pending`, `delivered`, `failed`).
    pub async fn collab_outbox_counts(&self) -> Result<HashMap<String, i64>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare("SELECT state, COUNT(*) FROM collab_outbox GROUP BY state")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(sql_err)?;
            let mut out = HashMap::new();
            for row in rows {
                let (state, count) = row.map_err(sql_err)?;
                out.insert(state, count);
            }
            Ok(out)
        })
        .await
    }

    /// Forget what was published for these threads (after an unlink), so
    /// sharing them again publishes everything to the new shared thread.
    pub async fn forget_collab_family(&self, thread_ids: &[ThreadId]) -> Result<()> {
        let ids: Vec<String> = thread_ids.iter().map(ToString::to_string).collect();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for id in &ids {
                tx.execute("DELETE FROM collab_outbox WHERE thread_id = ?1", [id])
                    .map_err(sql_err)?;
                tx.execute("DELETE FROM collab_family WHERE thread_id = ?1", [id])
                    .map_err(sql_err)?;
                tx.execute(
                    "DELETE FROM collab_message_origins WHERE thread_id = ?1 AND remote = 0",
                    [id],
                )
                .map_err(sql_err)?;
                tx.execute("DELETE FROM collab_links WHERE thread_id = ?1", [id])
                    .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }
}
