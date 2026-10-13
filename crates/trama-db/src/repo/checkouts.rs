//! Concrete repo-local checkout positions, bound to persisted attachments.

use crate::{TramaDb, codec::sql_err};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::{Component, PathBuf};
use trama_domain::{
    DomainError, Result,
    ids::{CheckoutId, RepositoryId, ThreadId},
    thread::ThreadRepositoryAttachment,
};

#[derive(Debug, Clone)]
pub struct CheckoutLocation {
    pub checkout_id: CheckoutId,
    pub repository_id: RepositoryId,
    pub path: PathBuf,
}

impl TramaDb {
    pub async fn checkout_location(
        &self,
        thread_id: &ThreadId,
        repository_id: &RepositoryId,
        checkout_id: &CheckoutId,
    ) -> Result<Option<PathBuf>> {
        let thread = thread_id.to_string();
        let repository = repository_id.to_string();
        let checkout = checkout_id.to_string();
        self.blocking(move |conn| {
            let row: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT thread_id, repository_id, path FROM checkout_locations WHERE checkout_id=?1",
                    [checkout],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(sql_err)?;
            match row {
                Some((owner, repo, path)) if owner == thread && repo == repository => {
                    Ok(Some(PathBuf::from(path)))
                }
                Some(_) => Err(DomainError::Invalid("checkout belongs to another attachment".into())),
                None => Ok(None),
            }
        })
        .await
    }

    /// The thread that owns `checkout_id`, if recorded.
    pub async fn checkout_owner(&self, checkout_id: &CheckoutId) -> Result<Option<ThreadId>> {
        let checkout = checkout_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT thread_id FROM checkout_locations WHERE checkout_id=?1",
                [checkout],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|owner| owner.map(ThreadId::from))
            .map_err(sql_err)
        })
        .await
    }
}

pub(crate) fn insert_locations(
    tx: &Transaction<'_>,
    thread_id: &str,
    attachments: &[ThreadRepositoryAttachment],
    locations: &[CheckoutLocation],
) -> Result<()> {
    for location in locations {
        if !attachments.iter().any(|attachment| {
            attachment.repository_id == location.repository_id
                && attachment.checkout_id.as_ref() == Some(&location.checkout_id)
        }) {
            return Err(DomainError::Invalid(
                "checkout location has no matching attachment".into(),
            ));
        }
        if !location.path.is_absolute()
            || location
                .path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(DomainError::Invalid(
                "checkout location must be an absolute path".into(),
            ));
        }
        let path = location
            .path
            .to_str()
            .ok_or_else(|| DomainError::Invalid("checkout path must be valid UTF-8".into()))?;
        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT thread_id, repository_id, path FROM checkout_locations WHERE checkout_id=?1",
                [location.checkout_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((thread, repository, old_path)) = existing {
            if thread != thread_id
                || repository != location.repository_id.to_string()
                || old_path != path
            {
                return Err(DomainError::Invalid(
                    "an existing checkout location cannot be retargeted".into(),
                ));
            }
            continue;
        }
        tx.execute(
            "INSERT INTO checkout_locations (checkout_id,thread_id,repository_id,path) VALUES (?1,?2,?3,?4)",
            params![location.checkout_id.to_string(), thread_id, location.repository_id.to_string(), path],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}
