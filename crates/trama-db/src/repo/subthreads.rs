//! Subthread projections: fork points, review verdicts.

use crate::codec::{enum_from_text, from_json, parse_rfc3339, rfc3339, sql_err, to_json};
use crate::projections::{insert_event, insert_message, insert_thread, load_attachments};
use crate::store::TramaDb;
use chrono::{DateTime, Utc};
use rusqlite::params;
use trama_domain::Result;
use trama_domain::checkpoint::RepoState;
use trama_domain::event::EventRecord;
use trama_domain::ids::{MessageId, RepositoryId, ThreadId, WorkspaceId};
use trama_domain::message::Message;
use trama_domain::subthread::{RepoForkPoint, ReviewVerdict};
use trama_domain::thread::Thread;

/// How a subthread was forked from its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubthreadFork {
    pub thread_id: ThreadId,
    pub parent_thread_id: ThreadId,
    /// The parent's agent session at fork time; resumed with `--fork-session`
    /// for the subthread's first agent session.
    pub parent_external_session_id: Option<String>,
    /// Last parent message the subthread inherits.
    pub fork_message_id: Option<MessageId>,
    pub repos: Vec<RepoForkPoint>,
}

#[derive(Debug, Clone)]
pub struct ReviewVerdictRecord {
    pub id: String,
    pub review_thread_id: ThreadId,
    pub parent_thread_id: ThreadId,
    pub verdict: ReviewVerdict,
    pub snapshot: Vec<RepoState>,
    pub created_at: DateTime<Utc>,
}

impl TramaDb {
    /// Insert a subthread (thread row, attachments, concrete checkout
    /// locations), its fork metadata and events in one transaction.
    pub async fn create_subthread(
        &self,
        thread: &Thread,
        fork: &SubthreadFork,
        locations: &[crate::CheckoutLocation],
        events: &[EventRecord],
    ) -> Result<()> {
        self.create_subthread_with(thread, fork, locations, events, None)
            .await
    }

    /// [`Self::create_subthread`], also recording the child as a subagent
    /// in the same transaction when `subagent` is set.
    pub async fn create_subthread_with(
        &self,
        thread: &Thread,
        fork: &SubthreadFork,
        locations: &[crate::CheckoutLocation],
        events: &[EventRecord],
        subagent: Option<&crate::SubagentRecord>,
    ) -> Result<()> {
        let thread = thread.clone();
        let fork = fork.clone();
        let locations = locations.to_vec();
        let events = events.to_vec();
        let subagent = subagent.cloned();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            insert_thread(&tx, &thread)?;
            if let Some(subagent) = &subagent {
                crate::agents::insert_subagent(&tx, subagent)?;
            }
            crate::checkouts::insert_locations(
                &tx,
                &thread.id.to_string(),
                &thread.repo_attachments,
                &locations,
            )?;
            tx.execute(
                "INSERT INTO subthread_forks
                    (thread_id, parent_thread_id, parent_external_session_id, fork_message_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    fork.thread_id.to_string(),
                    fork.parent_thread_id.to_string(),
                    fork.parent_external_session_id,
                    fork.fork_message_id.as_ref().map(|id| id.to_string()),
                    rfc3339(Utc::now()),
                ],
            )
            .map_err(sql_err)?;
            for repo in &fork.repos {
                tx.execute(
                    "INSERT INTO subthread_repo_forks
                        (thread_id, repository_id, base_revision, fork_commit)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        fork.thread_id.to_string(),
                        repo.repository_id.to_string(),
                        repo.base_revision,
                        repo.fork_commit,
                    ],
                )
                .map_err(sql_err)?;
            }
            for event in &events {
                insert_event(&tx, event)?;
            }
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn get_subthread_fork(&self, thread_id: &ThreadId) -> Result<Option<SubthreadFork>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT thread_id, parent_thread_id, parent_external_session_id, fork_message_id
                     FROM subthread_forks WHERE thread_id = ?1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![thread_id]).map_err(sql_err)?;
            let Some(row) = rows.next().map_err(sql_err)? else {
                return Ok(None);
            };
            let id: String = row.get(0).map_err(sql_err)?;
            let parent: String = row.get(1).map_err(sql_err)?;
            let session: Option<String> = row.get(2).map_err(sql_err)?;
            let message: Option<String> = row.get(3).map_err(sql_err)?;
            let mut repo_stmt = conn
                .prepare(
                    "SELECT repository_id, base_revision, fork_commit
                     FROM subthread_repo_forks WHERE thread_id = ?1
                     ORDER BY repository_id ASC",
                )
                .map_err(sql_err)?;
            let repos = repo_stmt
                .query_map(params![id], |row| {
                    Ok(RepoForkPoint {
                        repository_id: RepositoryId::from(row.get::<_, String>(0)?),
                        base_revision: row.get(1)?,
                        fork_commit: row.get(2)?,
                    })
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            Ok(Some(SubthreadFork {
                thread_id: ThreadId::from(id),
                parent_thread_id: ThreadId::from(parent),
                parent_external_session_id: session,
                fork_message_id: message.map(MessageId::from),
                repos,
            }))
        })
        .await
    }

    /// Direct children of `parent_id`, oldest first.
    pub async fn list_subthreads(&self, parent_id: &ThreadId) -> Result<Vec<Thread>> {
        let parent_id = parent_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, workspace_id, parent_thread_id, kind, title
                     FROM threads
                     WHERE parent_thread_id = ?1
                     ORDER BY created_at ASC, id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![parent_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut threads = Vec::new();
            for row in rows {
                let (id, ws, parent, kind, title) = row.map_err(sql_err)?;
                let repo_attachments = load_attachments(conn, &id)?;
                threads.push(Thread {
                    id: ThreadId::from(id),
                    workspace_id: ws.map(WorkspaceId::from),
                    parent_thread_id: parent.map(ThreadId::from),
                    kind: enum_from_text(&kind)?,
                    title,
                    repo_attachments,
                });
            }
            Ok(threads)
        })
        .await
    }

    /// Store a review verdict plus the visible entry (message) and event on
    /// the parent thread, atomically.
    pub async fn record_review_verdict(
        &self,
        record: &ReviewVerdictRecord,
        message: &Message,
        event: &EventRecord,
    ) -> Result<()> {
        let record = record.clone();
        let message = message.clone();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "INSERT INTO review_verdicts
                    (id, review_thread_id, parent_thread_id, verdict_json, snapshot_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.id,
                    record.review_thread_id.to_string(),
                    record.parent_thread_id.to_string(),
                    to_json(&record.verdict)?,
                    to_json(&record.snapshot)?,
                    rfc3339(record.created_at),
                ],
            )
            .map_err(sql_err)?;
            insert_message(&tx, &message)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn latest_review_verdict(
        &self,
        review_thread_id: &ThreadId,
    ) -> Result<Option<ReviewVerdictRecord>> {
        let review_thread_id = review_thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, review_thread_id, parent_thread_id, verdict_json, snapshot_json, created_at
                     FROM review_verdicts
                     WHERE review_thread_id = ?1
                     ORDER BY created_at DESC, id DESC
                     LIMIT 1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![review_thread_id]).map_err(sql_err)?;
            let Some(row) = rows.next().map_err(sql_err)? else {
                return Ok(None);
            };
            let verdict: String = row.get(3).map_err(sql_err)?;
            let snapshot: String = row.get(4).map_err(sql_err)?;
            let created_at: String = row.get(5).map_err(sql_err)?;
            Ok(Some(ReviewVerdictRecord {
                id: row.get(0).map_err(sql_err)?,
                review_thread_id: ThreadId::from(row.get::<_, String>(1).map_err(sql_err)?),
                parent_thread_id: ThreadId::from(row.get::<_, String>(2).map_err(sql_err)?),
                verdict: from_json(&verdict)?,
                snapshot: from_json(&snapshot)?,
                created_at: parse_rfc3339(&created_at)?,
            }))
        })
        .await
    }

    /// Append a message and its event atomically.
    pub async fn append_message_with_event(
        &self,
        message: &Message,
        event: &EventRecord,
    ) -> Result<()> {
        let message = message.clone();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            insert_message(&tx, &message)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama_domain::checkpoint::WorkingChanges;
    use trama_domain::event::{Aggregate, DomainEvent, EventStore};
    use trama_domain::message::Actor;
    use trama_domain::thread::{AttachMode, ThreadKind, ThreadRepositoryAttachment};
    use trama_domain::workspace::{Repository, Workspace};

    #[tokio::test]
    async fn subthread_fork_and_verdict_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let db = TramaDb::open(&path).unwrap();
        let workspace = Workspace::new("subthreads");
        db.create_workspace(&workspace).await.unwrap();
        let repository = Repository::new(None, Some("/tmp/repo".into()), None);
        db.create_repository(&repository).await.unwrap();
        let parent = Thread {
            id: ThreadId::generate(),
            workspace_id: Some(workspace.id.clone()),
            parent_thread_id: None,
            kind: ThreadKind::Top,
            title: Some("parent".into()),
            repo_attachments: vec![ThreadRepositoryAttachment {
                repository_id: repository.id.clone(),
                base_revision: Some("base".into()),
                checkout_id: Some("parent-checkout".into()),
                attach_mode: AttachMode::Isolated,
            }],
        };
        db.create_thread(&parent).await.unwrap();
        let mut review =
            Thread::new_subthread(&parent, ThreadKind::Review, Some("review".into())).unwrap();
        review.repo_attachments[0].checkout_id = Some("review-checkout".into());
        review.repo_attachments[0].base_revision = Some("base".into());
        let fork = SubthreadFork {
            thread_id: review.id.clone(),
            parent_thread_id: parent.id.clone(),
            parent_external_session_id: Some("ext-parent".into()),
            fork_message_id: Some(MessageId::from("m1")),
            repos: vec![RepoForkPoint {
                repository_id: repository.id.clone(),
                base_revision: "base".into(),
                fork_commit: "fork".into(),
            }],
        };
        let created = EventRecord::new(
            Aggregate::Thread {
                id: review.id.clone(),
            },
            Actor::System,
            DomainEvent::ThreadCreated {
                thread: review.clone(),
            },
            vec![],
        )
        .unwrap();
        let location = crate::test_abs_path("/tmp/repo/.trama/worktrees/review/review-checkout");
        db.create_subthread(
            &review,
            &fork,
            &[crate::CheckoutLocation {
                checkout_id: "review-checkout".into(),
                repository_id: repository.id.clone(),
                path: location.clone(),
            }],
            &[created],
        )
        .await
        .unwrap();

        let verdict = ReviewVerdictRecord {
            id: "v1".into(),
            review_thread_id: review.id.clone(),
            parent_thread_id: parent.id.clone(),
            verdict: ReviewVerdict::RequestChanges {
                summary: "add tests".into(),
            },
            snapshot: vec![RepoState {
                repository_id: repository.id.clone(),
                commit: Some("snap".into()),
                tree: Some("tree".into()),
                working_changes: WorkingChanges::Dirty,
            }],
            created_at: Utc::now(),
        };
        let message = Message::new_user(parent.id.clone(), "review", "verdict");
        let event = EventRecord::new(
            Aggregate::Thread {
                id: parent.id.clone(),
            },
            Actor::System,
            DomainEvent::ReviewVerdictRecorded {
                review_thread_id: review.id.clone(),
                parent_thread_id: parent.id.clone(),
                verdict: verdict.verdict.clone(),
                snapshot: verdict.snapshot.clone(),
            },
            vec![],
        )
        .unwrap();
        db.record_review_verdict(&verdict, &message, &event)
            .await
            .unwrap();
        drop(db);

        let db = TramaDb::open(&path).unwrap();
        assert_eq!(db.get_subthread_fork(&review.id).await.unwrap(), Some(fork));
        assert_eq!(
            db.checkout_location(&review.id, &repository.id, &"review-checkout".into())
                .await
                .unwrap(),
            Some(location)
        );
        assert!(db.get_subthread_fork(&parent.id).await.unwrap().is_none());
        let children = db.list_subthreads(&parent.id).await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].kind, ThreadKind::Review);
        assert_eq!(children[0].parent_thread_id.as_ref(), Some(&parent.id));
        assert_eq!(
            children[0].repo_attachments[0].checkout_id,
            Some("review-checkout".into())
        );
        let stored = db.latest_review_verdict(&review.id).await.unwrap().unwrap();
        assert_eq!(stored.verdict, verdict.verdict);
        assert_eq!(stored.snapshot.len(), 1);
        assert_eq!(
            db.list_messages_by_thread(&parent.id).await.unwrap().len(),
            1
        );
        let events = db
            .for_aggregate(
                &Aggregate::Thread {
                    id: parent.id.clone(),
                },
                None,
                10,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "review.verdict_recorded");
        // Listing by workspace still returns both, with parent and kind.
        let all = db.list_threads(&workspace.id).await.unwrap();
        assert_eq!(all.len(), 2);
    }
}
