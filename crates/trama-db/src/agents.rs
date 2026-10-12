//! Subagent records, per-turn checkpoints, and lookups used to fork a
//! thread at a message (sessions, single messages, ancestors).

use crate::codec::{enum_from_text, enum_text, parse_rfc3339, rfc3339, sql_err};
use crate::projections::{insert_event, message_tuple, rows_to_messages};
use crate::store::TramaDb;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};
use std::path::PathBuf;
use trama_domain::Result;
use trama_domain::agent::AgentSession;
use trama_domain::checkpoint::{Checkpoint, RepoState};
use trama_domain::event::{Aggregate, DomainEvent, EventRecord};
use trama_domain::ids::{AgentSessionId, CheckpointId, MessageId, RepositoryId, ThreadId};
use trama_domain::message::{Actor, Message};
use trama_domain::subagent::{SpawnedBy, SubagentStatus};

/// Hard stop when walking a parent chain. Nesting has no product limit; this
/// only bounds a corrupted (cyclic) table.
const MAX_ANCESTORS: usize = 4096;

/// A subagent thread's spawn record and the state of its current task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRecord {
    pub thread_id: ThreadId,
    pub spawned_by: SpawnedBy,
    pub status: SubagentStatus,
    pub detail: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(crate) fn insert_subagent(tx: &Transaction<'_>, record: &SubagentRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO subagents
            (thread_id, parent_thread_id, profile, tool_use_id, status, detail, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            record.thread_id.to_string(),
            record.spawned_by.parent_thread_id.to_string(),
            enum_text(&record.spawned_by.profile)?,
            record.spawned_by.tool_use_id,
            enum_text(&record.status)?,
            record.detail,
            rfc3339(record.created_at),
            rfc3339(record.updated_at),
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

type SubagentRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    String,
    String,
);

fn subagent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubagentRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn decode_subagent(row: SubagentRow) -> Result<SubagentRecord> {
    let (thread, parent, profile, tool_use_id, status, detail, created, updated) = row;
    Ok(SubagentRecord {
        thread_id: ThreadId::from(thread),
        spawned_by: SpawnedBy {
            parent_thread_id: ThreadId::from(parent),
            tool_use_id,
            profile: enum_from_text(&profile)?,
        },
        status: enum_from_text(&status)?,
        detail,
        created_at: parse_rfc3339(&created)?,
        updated_at: parse_rfc3339(&updated)?,
    })
}

const SUBAGENT_COLUMNS: &str =
    "thread_id, parent_thread_id, profile, tool_use_id, status, detail, created_at, updated_at";

impl TramaDb {
    pub async fn get_subagent(&self, thread_id: &ThreadId) -> Result<Option<SubagentRecord>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let row = conn
                .query_row(
                    &format!("SELECT {SUBAGENT_COLUMNS} FROM subagents WHERE thread_id = ?1"),
                    params![thread_id],
                    subagent_row,
                )
                .optional()
                .map_err(sql_err)?;
            row.map(decode_subagent).transpose()
        })
        .await
    }

    /// Every subagent record, oldest first.
    pub async fn list_subagents(&self) -> Result<Vec<SubagentRecord>> {
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {SUBAGENT_COLUMNS} FROM subagents ORDER BY created_at ASC, thread_id ASC"
                ))
                .map_err(sql_err)?;
            let rows = stmt.query_map([], subagent_row).map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(decode_subagent(row.map_err(sql_err)?)?);
            }
            Ok(out)
        })
        .await
    }

    /// Update a subagent's task state and append the matching event,
    /// atomically.
    pub async fn set_subagent_status(
        &self,
        thread_id: &ThreadId,
        status: SubagentStatus,
        detail: Option<String>,
    ) -> Result<()> {
        let event = EventRecord::new(
            Aggregate::Thread {
                id: thread_id.clone(),
            },
            Actor::System,
            DomainEvent::SubagentStatusChanged {
                thread_id: thread_id.clone(),
                status,
                detail: detail.clone(),
            },
            vec![],
        )?;
        let thread_id = thread_id.to_string();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "UPDATE subagents SET status = ?2, detail = ?3, updated_at = ?4 WHERE thread_id = ?1",
                params![
                    thread_id,
                    enum_text(&status)?,
                    detail,
                    rfc3339(Utc::now())
                ],
            )
            .map_err(sql_err)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    /// Store a checkpoint (and its `CheckpointCreated` event). `event_seq`
    /// is set to the last event sequence before it: the checkpoint reflects
    /// everything recorded up to then.
    pub async fn record_checkpoint(&self, checkpoint: &Checkpoint) -> Result<Checkpoint> {
        let mut checkpoint = checkpoint.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let seq: i64 = tx
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
                    row.get(0)
                })
                .map_err(sql_err)?;
            checkpoint.event_seq = u64::try_from(seq).unwrap_or_default();
            tx.execute(
                "INSERT INTO checkpoints (id, thread_id, event_seq, label, created_at, message_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    checkpoint.id.to_string(),
                    checkpoint.thread_id.to_string(),
                    seq,
                    enum_text(&checkpoint.label)?,
                    rfc3339(checkpoint.created_at),
                    checkpoint.message_id.as_ref().map(|id| id.to_string()),
                ],
            )
            .map_err(sql_err)?;
            for state in &checkpoint.repo_states {
                tx.execute(
                    "INSERT INTO checkpoint_repo_states
                        (checkpoint_id, repository_id, \"commit\", tree, working_changes)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        checkpoint.id.to_string(),
                        state.repository_id.to_string(),
                        state.commit,
                        state.tree,
                        enum_text(&state.working_changes)?,
                    ],
                )
                .map_err(sql_err)?;
            }
            let event = EventRecord::new(
                Aggregate::Thread {
                    id: checkpoint.thread_id.clone(),
                },
                Actor::System,
                DomainEvent::CheckpointCreated {
                    checkpoint: checkpoint.clone(),
                },
                vec![],
            )?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(checkpoint)
        })
        .await
    }

    /// The thread's most recent checkpoint that names `message_id`.
    pub async fn checkpoint_for_message(
        &self,
        thread_id: &ThreadId,
        message_id: &MessageId,
    ) -> Result<Option<Checkpoint>> {
        let thread_id = thread_id.to_string();
        let message_id = message_id.to_string();
        self.blocking(move |conn| {
            let row = conn
                .query_row(
                    "SELECT id, event_seq, label, created_at FROM checkpoints
                     WHERE thread_id = ?1 AND message_id = ?2
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    params![thread_id, message_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(sql_err)?;
            let Some((id, seq, label, created_at)) = row else {
                return Ok(None);
            };
            let mut stmt = conn
                .prepare(
                    "SELECT repository_id, \"commit\", tree, working_changes
                     FROM checkpoint_repo_states WHERE checkpoint_id = ?1
                     ORDER BY repository_id ASC",
                )
                .map_err(sql_err)?;
            let states = stmt
                .query_map(params![id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            let mut repo_states = Vec::with_capacity(states.len());
            for (repository_id, commit, tree, working) in states {
                repo_states.push(RepoState {
                    repository_id: RepositoryId::from(repository_id),
                    commit,
                    tree,
                    working_changes: enum_from_text(&working)?,
                });
            }
            Ok(Some(Checkpoint {
                id: CheckpointId::from(id),
                thread_id: ThreadId::from(thread_id),
                event_seq: u64::try_from(seq).unwrap_or_default(),
                label: enum_from_text(&label)?,
                repo_states,
                created_at: parse_rfc3339(&created_at)?,
                message_id: Some(MessageId::from(message_id)),
            }))
        })
        .await
    }

    pub async fn get_message(&self, id: &MessageId) -> Result<Option<Message>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, thread_id, parent_id, actor_json, role, content_json, created_at,
                            claude_uuid, annotation_json
                     FROM messages WHERE id = ?1",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![id], message_tuple)
                .map_err(sql_err)?;
            Ok(rows_to_messages(rows)?.pop())
        })
        .await
    }

    pub async fn get_agent_session(&self, id: &AgentSessionId) -> Result<Option<AgentSession>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            conn.query_row(
                "SELECT id, external_session_id, model, cwd FROM agent_sessions WHERE id = ?1",
                params![id],
                session_row,
            )
            .optional()
            .map_err(sql_err)
        })
        .await
    }

    /// The thread's agent sessions, newest first.
    pub async fn list_agent_sessions_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<AgentSession>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, external_session_id, model, cwd FROM agent_sessions
                     WHERE thread_id = ?1 ORDER BY started_at DESC, id DESC",
                )
                .map_err(sql_err)?;
            stmt.query_map(params![thread_id], session_row)
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)
        })
        .await
    }

    /// Parent, grandparent, ... of `thread_id` (nearest first). Stops at a
    /// repeated id, so a corrupted cyclic chain cannot loop.
    pub async fn thread_ancestors(&self, thread_id: &ThreadId) -> Result<Vec<ThreadId>> {
        let start = thread_id.to_string();
        self.blocking(move |conn| {
            let mut out: Vec<ThreadId> = Vec::new();
            let mut seen = std::collections::HashSet::from([start.clone()]);
            let mut current = start;
            while out.len() < MAX_ANCESTORS {
                let parent: Option<String> = conn
                    .query_row(
                        "SELECT parent_thread_id FROM threads WHERE id = ?1",
                        params![current],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_err)?
                    .flatten();
                match parent {
                    Some(parent) if seen.insert(parent.clone()) => {
                        out.push(ThreadId::from(parent.clone()));
                        current = parent;
                    }
                    _ => break,
                }
            }
            Ok(out)
        })
        .await
    }
}

fn session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentSession> {
    Ok(AgentSession {
        id: AgentSessionId::from(row.get::<_, String>(0)?),
        external_session_id: row.get(1)?,
        model: row.get(2)?,
        cwd: PathBuf::from(row.get::<_, Option<String>>(3)?.unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubthreadFork;
    use trama_domain::checkpoint::{CheckpointLabel, WorkingChanges};
    use trama_domain::event::EventStore;
    use trama_domain::message::MessageAnnotation;
    use trama_domain::subagent::SubagentProfile;
    use trama_domain::thread::{Thread, ThreadKind};
    use trama_domain::workspace::Workspace;

    async fn chain(db: &TramaDb, depth: usize) -> Vec<Thread> {
        let workspace = Workspace::new("agents");
        db.create_workspace(&workspace).await.unwrap();
        let mut top = Thread::new_for_workspace(&workspace, &[], Some("top".into()));
        top.repo_attachments.clear();
        db.create_thread(&top).await.unwrap();
        let mut threads = vec![top];
        for _ in 0..depth {
            let child =
                Thread::new_subthread(threads.last().unwrap(), ThreadKind::Sub, None).unwrap();
            db.create_thread(&child).await.unwrap();
            threads.push(child);
        }
        threads
    }

    #[tokio::test]
    async fn subagent_status_checkpoints_and_ancestors_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trama.db");
        let db = TramaDb::open(&path).unwrap();
        let threads = chain(&db, 3).await;
        let ancestors = db.thread_ancestors(&threads[3].id).await.unwrap();
        assert_eq!(
            ancestors,
            vec![
                threads[2].id.clone(),
                threads[1].id.clone(),
                threads[0].id.clone()
            ]
        );
        assert!(
            db.thread_ancestors(&threads[0].id)
                .await
                .unwrap()
                .is_empty()
        );

        // A subagent recorded together with its subthread.
        let parent = &threads[3];
        let child = Thread::new_subthread(parent, ThreadKind::Sub, Some("w".into())).unwrap();
        let record = SubagentRecord {
            thread_id: child.id.clone(),
            spawned_by: SpawnedBy {
                parent_thread_id: parent.id.clone(),
                tool_use_id: Some("toolu_9".into()),
                profile: SubagentProfile::Scout,
            },
            status: SubagentStatus::Queued,
            detail: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let fork = SubthreadFork {
            thread_id: child.id.clone(),
            parent_thread_id: parent.id.clone(),
            parent_external_session_id: None,
            fork_message_id: None,
            repos: vec![],
        };
        db.create_subthread_with(&child, &fork, &[], &[], Some(&record))
            .await
            .unwrap();
        db.set_subagent_status(&child.id, SubagentStatus::Failed, Some("boom".into()))
            .await
            .unwrap();

        // Messages keep their uuid and annotation.
        let mut answer = Message::new_system(parent.id.clone(), "spawned");
        answer.claude_uuid = Some("uuid-a".into());
        answer.annotation = Some(MessageAnnotation::SubagentSpawned {
            child_thread_id: child.id.clone(),
            title: "w".into(),
            profile: SubagentProfile::Scout,
        });
        db.append_message(&answer).await.unwrap();
        let checkpoint = db
            .record_checkpoint(&Checkpoint {
                id: CheckpointId::generate(),
                thread_id: parent.id.clone(),
                event_seq: 0,
                label: CheckpointLabel::Turn,
                repo_states: vec![RepoState {
                    repository_id: RepositoryId::from("repo"),
                    commit: Some("c1".into()),
                    tree: Some("t1".into()),
                    working_changes: WorkingChanges::Dirty,
                }],
                created_at: Utc::now(),
                message_id: Some(answer.id.clone()),
            })
            .await
            .unwrap();
        assert!(checkpoint.event_seq > 0);
        drop(db);

        let db = TramaDb::open(&path).unwrap();
        let stored = db.get_subagent(&child.id).await.unwrap().unwrap();
        assert_eq!(stored.status, SubagentStatus::Failed);
        assert_eq!(stored.detail.as_deref(), Some("boom"));
        assert_eq!(stored.spawned_by, record.spawned_by);
        assert_eq!(db.list_subagents().await.unwrap().len(), 1);
        let message = db.get_message(&answer.id).await.unwrap().unwrap();
        assert_eq!(message.claude_uuid.as_deref(), Some("uuid-a"));
        assert_eq!(message.annotation, answer.annotation);
        let listed = db.list_messages_by_thread(&parent.id).await.unwrap();
        assert_eq!(listed[0].annotation, answer.annotation);
        let found = db
            .checkpoint_for_message(&parent.id, &answer.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, checkpoint.id);
        assert_eq!(found.repo_states[0].commit.as_deref(), Some("c1"));
        assert_eq!(found.repo_states[0].working_changes, WorkingChanges::Dirty);
        assert!(
            db.checkpoint_for_message(&child.id, &answer.id)
                .await
                .unwrap()
                .is_none()
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
        assert!(events.iter().any(|e| e.kind == "checkpoint.created"));
        let events = db
            .for_aggregate(&Aggregate::Thread { id: child.id }, None, 10)
            .await
            .unwrap();
        assert!(events.iter().any(|e| e.kind == "subagent.status_changed"));
    }
}
