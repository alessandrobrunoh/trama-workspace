use crate::codec::{from_json, is_constraint_violation, rfc3339, sql_err, to_json};
use crate::store::TramaDb;
use async_trait::async_trait;
use rusqlite::params;
use trama_domain::event::{Aggregate, EventRecord, EventStore};
use trama_domain::{DomainError, Result};

#[async_trait]
impl EventStore for TramaDb {
    async fn append(&self, records: Vec<EventRecord>) -> Result<()> {
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for record in &records {
                let aggregate_json = to_json(&record.aggregate)?;
                let actor_json = to_json(&record.actor)?;
                let payload_json = to_json(&record.payload)?;
                let parents_json = to_json(&record.parents)?;
                let created_at = rfc3339(record.created_at);
                let insert = tx.execute(
                    "INSERT INTO events (
                        id, aggregate_json, actor_json, schema_version, kind,
                        payload_json, parents_json, created_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        record.id,
                        aggregate_json,
                        actor_json,
                        record.schema_version as i64,
                        record.kind,
                        payload_json,
                        parents_json,
                        created_at,
                    ],
                );
                match insert {
                    Ok(_) => {}
                    Err(err) if is_constraint_violation(&err) => {
                        return Err(DomainError::Invalid(format!(
                            "duplicate event id: {}",
                            record.id
                        )));
                    }
                    Err(err) => return Err(sql_err(err)),
                }
            }
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    async fn for_aggregate(
        &self,
        aggregate: &Aggregate,
        after_seq: Option<u64>,
        limit: u32,
    ) -> Result<Vec<EventRecord>> {
        let aggregate = aggregate.clone();
        self.blocking(move |conn| {
            let aggregate_json = to_json(&aggregate)?;
            let after = after_seq.unwrap_or(0) as i64;
            let mut stmt = conn
                .prepare(
                    "SELECT seq, id, aggregate_json, actor_json, schema_version,
                            kind, payload_json, parents_json, created_at
                     FROM events
                     WHERE aggregate_json = ?1 AND seq > ?2
                     ORDER BY seq ASC
                     LIMIT ?3",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![aggregate_json, after, limit as i64], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (seq, id, agg, actor, schema_version, kind, payload, parents, created_at) =
                    row.map_err(sql_err)?;
                out.push(EventRecord {
                    id,
                    seq: Some(seq as u64),
                    aggregate: from_json(&agg)?,
                    actor: from_json(&actor)?,
                    schema_version: schema_version as u16,
                    kind,
                    payload: from_json(&payload)?,
                    parents: from_json(&parents)?,
                    created_at: crate::codec::parse_rfc3339(&created_at)?,
                });
            }
            Ok(out)
        })
        .await
    }
}
