//! Archived threads and managed checkouts removed from disk.

use crate::codec::{parse_rfc3339, rfc3339, sql_err};
use crate::store::TramaDb;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;
use std::path::PathBuf;
use trama_domain::Result;
use trama_domain::ids::{CheckoutId, RepositoryId, ThreadId, WorkspaceId};

/// A recorded repo-local checkout location of a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadCheckoutRow {
    pub thread_id: ThreadId,
    pub repository_id: RepositoryId,
    pub checkout_id: CheckoutId,
    pub path: PathBuf,
    /// Set when the directory was removed (snapshot commit of its state).
    pub removed_snapshot: Option<String>,
}

impl TramaDb {
    /// Archive (`true`) or unarchive a thread. Idempotent.
    pub async fn set_thread_archived(&self, thread_id: &ThreadId, archived: bool) -> Result<()> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            if archived {
                conn.execute(
                    "INSERT INTO thread_archive (thread_id, archived_at) VALUES (?1, ?2)
                     ON CONFLICT(thread_id) DO NOTHING",
                    params![thread_id, rfc3339(Utc::now())],
                )
            } else {
                conn.execute(
                    "DELETE FROM thread_archive WHERE thread_id = ?1",
                    [thread_id],
                )
            }
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn is_thread_archived(&self, thread_id: &ThreadId) -> Result<bool> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT 1 FROM thread_archive WHERE thread_id = ?1",
                [thread_id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(sql_err)
        })
        .await
    }

    pub async fn archived_thread_ids(&self) -> Result<HashSet<ThreadId>> {
        self.blocking(move |conn| {
            let mut statement = conn
                .prepare("SELECT thread_id FROM thread_archive")
                .map_err(sql_err)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql_err)?;
            rows.map(|row| row.map(ThreadId::from).map_err(sql_err))
                .collect()
        })
        .await
    }

    /// Mark a recorded checkout as removed from disk.
    pub async fn record_removed_checkout(
        &self,
        thread_id: &ThreadId,
        checkout_id: &CheckoutId,
        snapshot_commit: &str,
    ) -> Result<()> {
        let thread_id = thread_id.to_string();
        let checkout_id = checkout_id.to_string();
        let snapshot_commit = snapshot_commit.to_owned();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO removed_checkouts (checkout_id, thread_id, snapshot_commit, removed_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(checkout_id) DO UPDATE SET snapshot_commit = excluded.snapshot_commit,
                     removed_at = excluded.removed_at",
                params![checkout_id, thread_id, snapshot_commit, rfc3339(Utc::now())],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// The checkout was recreated.
    pub async fn clear_removed_checkout(&self, checkout_id: &CheckoutId) -> Result<()> {
        let checkout_id = checkout_id.to_string();
        self.blocking(move |conn| {
            conn.execute(
                "DELETE FROM removed_checkouts WHERE checkout_id = ?1",
                [checkout_id],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Snapshot commit of a removed checkout, `None` when it is on disk.
    pub async fn removed_checkout(&self, checkout_id: &CheckoutId) -> Result<Option<String>> {
        let checkout_id = checkout_id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT snapshot_commit FROM removed_checkouts WHERE checkout_id = ?1",
                [checkout_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// Recorded repo-local checkouts of every thread (of `workspace_id`
    /// when set), removed ones included.
    pub async fn list_thread_checkouts(
        &self,
        workspace_id: Option<&WorkspaceId>,
    ) -> Result<Vec<ThreadCheckoutRow>> {
        let workspace = workspace_id.map(ToString::to_string);
        self.blocking(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT l.thread_id, l.repository_id, l.checkout_id, l.path, r.snapshot_commit
                     FROM checkout_locations l
                     JOIN threads t ON t.id = l.thread_id
                     LEFT JOIN removed_checkouts r ON r.checkout_id = l.checkout_id
                     WHERE ?1 IS NULL OR t.workspace_id = ?1
                     ORDER BY l.thread_id, l.repository_id",
                )
                .map_err(sql_err)?;
            let rows = statement
                .query_map([workspace], |row| {
                    Ok(ThreadCheckoutRow {
                        thread_id: ThreadId::from(row.get::<_, String>(0)?),
                        repository_id: RepositoryId::from(row.get::<_, String>(1)?),
                        checkout_id: CheckoutId::from(row.get::<_, String>(2)?),
                        path: PathBuf::from(row.get::<_, String>(3)?),
                        removed_snapshot: row.get(4)?,
                    })
                })
                .map_err(sql_err)?;
            rows.map(|row| row.map_err(sql_err)).collect()
        })
        .await
    }

    /// The thread's latest message time, else its creation time.
    pub async fn thread_last_active(&self, thread_id: &ThreadId) -> Result<Option<DateTime<Utc>>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT COALESCE(
                         (SELECT MAX(created_at) FROM messages WHERE thread_id = ?1),
                         (SELECT created_at FROM threads WHERE id = ?1))",
                    [thread_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?
                .flatten();
            raw.as_deref().map(parse_rfc3339).transpose()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CheckoutLocation;
    use trama_domain::message::Message;
    use trama_domain::thread::{AttachMode, Thread, ThreadKind, ThreadRepositoryAttachment};
    use trama_domain::workspace::{Repository, Workspace};

    #[tokio::test]
    async fn archive_flags_removed_checkouts_and_last_activity() {
        let dir = tempfile::tempdir().unwrap();
        let db = TramaDb::open(&dir.path().join("archive.db")).unwrap();
        let workspace = Workspace::new("archive");
        db.create_workspace(&workspace).await.unwrap();
        let repository = RepositoryId::generate();
        db.create_repository(&Repository {
            id: repository.clone(),
            canonical_remote: None,
            local_path: Some(PathBuf::from("/src/repo")),
            default_branch: Some("main".into()),
        })
        .await
        .unwrap();
        let checkout = CheckoutId::from("checkout");
        let thread = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("archive me".into()),
            repo_attachments: vec![ThreadRepositoryAttachment {
                repository_id: repository.clone(),
                base_revision: Some("abc".into()),
                checkout_id: Some(checkout.clone()),
                attach_mode: AttachMode::Isolated,
            }],
        };
        let path = PathBuf::from("/src/repo/.trama/worktrees/t/checkout");
        db.create_thread_with_locations(
            &thread,
            &[CheckoutLocation {
                checkout_id: checkout.clone(),
                repository_id: repository.clone(),
                path: path.clone(),
            }],
        )
        .await
        .unwrap();

        assert!(!db.is_thread_archived(&thread.id).await.unwrap());
        db.set_thread_archived(&thread.id, true).await.unwrap();
        db.set_thread_archived(&thread.id, true).await.unwrap();
        assert!(db.is_thread_archived(&thread.id).await.unwrap());
        assert!(db.archived_thread_ids().await.unwrap().contains(&thread.id));

        db.record_removed_checkout(&thread.id, &checkout, "snap")
            .await
            .unwrap();
        assert_eq!(
            db.removed_checkout(&checkout).await.unwrap().as_deref(),
            Some("snap")
        );
        let rows = db.list_thread_checkouts(Some(&workspace.id)).await.unwrap();
        assert_eq!(
            rows,
            vec![ThreadCheckoutRow {
                thread_id: thread.id.clone(),
                repository_id: repository.clone(),
                checkout_id: checkout.clone(),
                path,
                removed_snapshot: Some("snap".into()),
            }]
        );
        assert!(
            db.list_thread_checkouts(Some(&WorkspaceId::generate()))
                .await
                .unwrap()
                .is_empty()
        );
        db.clear_removed_checkout(&checkout).await.unwrap();
        assert_eq!(db.removed_checkout(&checkout).await.unwrap(), None);
        db.set_thread_archived(&thread.id, false).await.unwrap();
        assert!(!db.is_thread_archived(&thread.id).await.unwrap());

        let created = db.thread_last_active(&thread.id).await.unwrap().unwrap();
        let message = Message::new_user(thread.id.clone(), "user", "hello");
        db.append_message(&message).await.unwrap();
        let active = db.thread_last_active(&thread.id).await.unwrap().unwrap();
        assert!(active >= created);
        assert_eq!(active, message.created_at);
    }
}
