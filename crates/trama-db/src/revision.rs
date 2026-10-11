use crate::codec::{is_constraint_violation, parse_rfc3339, path_to_text, rfc3339, sql_err};
use crate::store::TramaDb;
use async_trait::async_trait;
use rusqlite::params;
use std::path::{Path, PathBuf};
use trama_domain::ids::RepositoryId;
use trama_domain::revision::{FileRevision, RevisionStore};
use trama_domain::{DomainError, Result};

fn row_to_revision(
    repository_id: String,
    path: String,
    revision: i64,
    content_hash: String,
    size_bytes: i64,
    event_seq: Option<i64>,
    recorded_at: String,
) -> Result<FileRevision> {
    Ok(FileRevision {
        repository_id: RepositoryId::from(repository_id),
        path: PathBuf::from(path),
        revision: revision as u64,
        content_hash,
        size_bytes: size_bytes as u64,
        event_seq: event_seq.map(|s| s as u64),
        recorded_at: parse_rfc3339(&recorded_at)?,
    })
}

#[async_trait]
impl RevisionStore for TramaDb {
    async fn record(&self, revision: FileRevision) -> Result<()> {
        self.blocking(move |conn| {
            let path = path_to_text(&revision.path)?;
            let recorded_at = rfc3339(revision.recorded_at);
            let insert = conn.execute(
                "INSERT INTO file_revisions (
                    repository_id, path, revision, content_hash, size_bytes, event_seq, recorded_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    revision.repository_id.to_string(),
                    path,
                    revision.revision as i64,
                    revision.content_hash,
                    revision.size_bytes as i64,
                    revision.event_seq.map(|s| s as i64),
                    recorded_at,
                ],
            );
            match insert {
                Ok(_) => Ok(()),
                Err(err) if is_constraint_violation(&err) => Err(DomainError::Invalid(format!(
                    "duplicate file revision {} @ {}",
                    revision.revision,
                    revision.path.display()
                ))),
                Err(err) => Err(sql_err(err)),
            }
        })
        .await
    }

    async fn latest(
        &self,
        repository_id: &RepositoryId,
        path: &Path,
    ) -> Result<Option<FileRevision>> {
        let repository_id = repository_id.to_string();
        let path = path_to_text(path)?;
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT repository_id, path, revision, content_hash, size_bytes, event_seq, recorded_at
                     FROM file_revisions
                     WHERE repository_id = ?1 AND path = ?2
                     ORDER BY revision DESC
                     LIMIT 1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![repository_id, path]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_revision(
                    row.get(0).map_err(sql_err)?,
                    row.get(1).map_err(sql_err)?,
                    row.get(2).map_err(sql_err)?,
                    row.get(3).map_err(sql_err)?,
                    row.get(4).map_err(sql_err)?,
                    row.get(5).map_err(sql_err)?,
                    row.get(6).map_err(sql_err)?,
                )?)),
            }
        })
        .await
    }

    async fn history(
        &self,
        repository_id: &RepositoryId,
        path: &Path,
        limit: u32,
    ) -> Result<Vec<FileRevision>> {
        let repository_id = repository_id.to_string();
        let path = path_to_text(path)?;
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT repository_id, path, revision, content_hash, size_bytes, event_seq, recorded_at
                     FROM file_revisions
                     WHERE repository_id = ?1 AND path = ?2
                     ORDER BY revision DESC
                     LIMIT ?3",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![repository_id, path, limit as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (repo, path, revision, hash, size, seq, recorded_at) = row.map_err(sql_err)?;
                out.push(row_to_revision(
                    repo, path, revision, hash, size, seq, recorded_at,
                )?);
            }
            Ok(out)
        })
        .await
    }
}
