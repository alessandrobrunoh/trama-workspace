use crate::codec::{
    enum_from_text, enum_text, from_json, is_constraint_violation, parse_rfc3339, path_to_text,
    rfc3339, sql_err, to_json,
};
use crate::store::TramaDb;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use std::path::{Path, PathBuf};
use trama_domain::Result;
use trama_domain::agent::{AgentSession, UsageReport};
use trama_domain::event::EventRecord;
use trama_domain::ids::{
    AgentSessionId, CheckoutId, MessageId, RepositoryId, ThreadId, WorkspaceId,
};
use trama_domain::message::Message;
use trama_domain::thread::{Thread, ThreadRepositoryAttachment};
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership, WorkspaceSettings};

impl TramaDb {
    pub async fn create_workspace(&self, workspace: &Workspace) -> Result<()> {
        let workspace = workspace.clone();
        self.blocking(move |conn| {
            let now = rfc3339(Utc::now());
            conn.execute(
                "INSERT INTO workspaces (id, name, settings_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    workspace.id.to_string(),
                    workspace.name,
                    to_json(&workspace.settings)?,
                    now,
                    now,
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, settings_json FROM workspaces ORDER BY created_at ASC, id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, name, settings_json) = row.map_err(sql_err)?;
                out.push(Workspace {
                    id: WorkspaceId::from(id),
                    name,
                    settings: from_json(&settings_json)?,
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn create_repository(&self, repository: &Repository) -> Result<()> {
        let repository = repository.clone();
        self.blocking(move |conn| {
            let local_path = repository
                .local_path
                .as_deref()
                .map(path_to_text)
                .transpose()?;
            conn.execute(
                "INSERT INTO repositories (id, canonical_remote, local_path, default_branch)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    repository.id.to_string(),
                    repository.canonical_remote,
                    local_path,
                    repository.default_branch,
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn add_membership(&self, membership: &WorkspaceMembership) -> Result<()> {
        let membership = membership.clone();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO workspace_repositories
                    (workspace_id, repository_id, alias, default_enabled)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    membership.workspace_id.to_string(),
                    membership.repository_id.to_string(),
                    membership.alias,
                    membership.default_enabled as i64,
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn list_memberships(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<WorkspaceMembership>> {
        let workspace_id = workspace_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT workspace_id, repository_id, alias, default_enabled
                     FROM workspace_repositories
                     WHERE workspace_id = ?1
                     ORDER BY alias ASC, repository_id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![workspace_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (ws, repo, alias, enabled) = row.map_err(sql_err)?;
                out.push(WorkspaceMembership {
                    workspace_id: WorkspaceId::from(ws),
                    repository_id: RepositoryId::from(repo),
                    alias,
                    default_enabled: enabled != 0,
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn create_thread(&self, thread: &Thread) -> Result<()> {
        self.create_thread_with_locations(thread, &[]).await
    }

    pub async fn create_thread_with_locations(
        &self,
        thread: &Thread,
        locations: &[crate::CheckoutLocation],
    ) -> Result<()> {
        let thread = thread.clone();
        let locations = locations.to_vec();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            insert_thread(&tx, &thread)?;
            crate::checkouts::insert_locations(
                &tx,
                &thread.id.to_string(),
                &thread.repo_attachments,
                &locations,
            )?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn list_threads(&self, workspace_id: &WorkspaceId) -> Result<Vec<Thread>> {
        let workspace_id = workspace_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, workspace_id, parent_thread_id, kind, title
                     FROM threads
                     WHERE workspace_id = ?1
                     ORDER BY created_at ASC, id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![workspace_id], |row| {
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

    pub async fn append_message(&self, message: &Message) -> Result<()> {
        let message = message.clone();
        self.blocking(move |conn| insert_message(conn, &message))
            .await
    }

    pub async fn list_messages_by_thread(&self, thread_id: &ThreadId) -> Result<Vec<Message>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, thread_id, parent_id, actor_json, role, content_json, created_at,
                            claude_uuid, annotation_json
                     FROM messages
                     WHERE thread_id = ?1
                     ORDER BY created_at ASC, id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![thread_id], message_tuple)
                .map_err(sql_err)?;
            rows_to_messages(rows)
        })
        .await
    }

    /// Full-text search over indexed message bodies (content JSON, via FTS5).
    pub async fn search_messages(&self, query: &str) -> Result<Vec<Message>> {
        let query = query.trim().to_owned();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT m.id, m.thread_id, m.parent_id, m.actor_json, m.role,
                            m.content_json, m.created_at, m.claude_uuid, m.annotation_json
                     FROM messages_fts
                     JOIN messages m ON m.id = messages_fts.message_id
                     WHERE messages_fts MATCH ?1
                     ORDER BY rank",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![query], message_tuple)
                .map_err(sql_err)?;
            rows_to_messages(rows)
        })
        .await
    }

    pub async fn create_agent_session(
        &self,
        thread_id: &ThreadId,
        session: &AgentSession,
        profile: &str,
    ) -> Result<()> {
        let thread_id = thread_id.to_string();
        let session = session.clone();
        let profile = profile.to_owned();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO agent_sessions (
                    id, thread_id, profile, external_session_id, model, cwd, status, started_at, ended_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, NULL)",
                params![
                    session.id.to_string(),
                    thread_id,
                    profile,
                    session.external_session_id,
                    session.model,
                    path_to_text(&session.cwd)?,
                    rfc3339(Utc::now()),
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn record_usage(
        &self,
        session_id: &AgentSessionId,
        thread_id: &ThreadId,
        report: &UsageReport,
    ) -> Result<()> {
        let session_id = session_id.to_string();
        let thread_id = thread_id.to_string();
        let report = report.clone();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO usage_events (
                    session_id, thread_id, model, tokens_in, tokens_out,
                    cache_read, cache_write, cost_usd_micros, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)",
                params![
                    session_id,
                    thread_id,
                    report.model,
                    report.input_tokens as i64,
                    report.output_tokens as i64,
                    report.cache_read_tokens as i64,
                    report.cache_write_tokens as i64,
                    rfc3339(Utc::now()),
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }
}

pub(crate) fn insert_thread(tx: &rusqlite::Transaction<'_>, thread: &Thread) -> Result<()> {
    let now = rfc3339(Utc::now());
    tx.execute(
        "INSERT INTO threads
            (id, workspace_id, parent_thread_id, kind, title, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            thread.id.to_string(),
            thread.workspace_id.as_ref().map(|id| id.to_string()),
            thread.parent_thread_id.as_ref().map(|id| id.to_string()),
            enum_text(&thread.kind)?,
            thread.title,
            now,
            now,
        ],
    )
    .map_err(sql_err)?;
    for attachment in &thread.repo_attachments {
        tx.execute(
            "INSERT INTO thread_repositories
                (thread_id, repository_id, base_revision, checkout_id, attach_mode)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                thread.id.to_string(),
                attachment.repository_id.to_string(),
                attachment.base_revision,
                attachment.checkout_id.as_ref().map(|id| id.to_string()),
                enum_text(&attachment.attach_mode)?,
            ],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}

pub(crate) fn load_attachments(
    conn: &rusqlite::Connection,
    thread_id: &str,
) -> Result<Vec<ThreadRepositoryAttachment>> {
    let mut stmt = conn
        .prepare(
            "SELECT repository_id, base_revision, checkout_id, attach_mode
             FROM thread_repositories
             WHERE thread_id = ?1
             ORDER BY repository_id ASC",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![thread_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (repo, base, checkout, mode) = row.map_err(sql_err)?;
        out.push(ThreadRepositoryAttachment {
            repository_id: RepositoryId::from(repo),
            base_revision: base,
            checkout_id: checkout.map(CheckoutId::from),
            attach_mode: enum_from_text(&mode)?,
        });
    }
    Ok(out)
}

type MessageTuple = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// Columns: id, thread_id, parent_id, actor_json, role, content_json,
/// created_at, claude_uuid, annotation_json.
pub(crate) fn message_tuple(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageTuple> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

pub(crate) fn rows_to_messages(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<MessageTuple>,
    >,
) -> Result<Vec<Message>> {
    let mut out = Vec::new();
    for row in rows {
        let (
            id,
            thread_id,
            parent_id,
            actor_json,
            role,
            content_json,
            created_at,
            claude_uuid,
            annotation_json,
        ) = row.map_err(sql_err)?;
        out.push(Message {
            id: MessageId::from(id),
            thread_id: ThreadId::from(thread_id),
            parent_id: parent_id.map(MessageId::from),
            actor: from_json(&actor_json)?,
            role: enum_from_text(&role)?,
            content: from_json(&content_json)?,
            created_at: parse_rfc3339(&created_at)?,
            claude_uuid,
            // An annotation this build cannot read is dropped, not fatal.
            annotation: annotation_json.and_then(|raw| from_json(&raw).ok()),
        });
    }
    Ok(out)
}

impl TramaDb {
    pub async fn get_workspace(&self, id: &WorkspaceId) -> Result<Option<Workspace>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare("SELECT id, name, settings_json FROM workspaces WHERE id = ?1")
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![id]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_workspace(
                    row.get(0).map_err(sql_err)?,
                    row.get(1).map_err(sql_err)?,
                    row.get(2).map_err(sql_err)?,
                )?)),
            }
        })
        .await
    }

    pub async fn get_workspace_by_name(&self, name: &str) -> Result<Option<Workspace>> {
        let name = name.to_owned();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, settings_json FROM workspaces WHERE name = ?1 ORDER BY id ASC LIMIT 1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![name]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_workspace(
                    row.get(0).map_err(sql_err)?,
                    row.get(1).map_err(sql_err)?,
                    row.get(2).map_err(sql_err)?,
                )?)),
            }
        })
        .await
    }

    pub async fn update_workspace_settings(
        &self,
        id: &WorkspaceId,
        settings: &WorkspaceSettings,
    ) -> Result<()> {
        let id = id.to_string();
        let settings_json = to_json(settings)?;
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE workspaces SET settings_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, settings_json, rfc3339(Utc::now())],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn get_repository(&self, id: &RepositoryId) -> Result<Option<Repository>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, canonical_remote, local_path, default_branch FROM repositories WHERE id = ?1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![id]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_repository(
                    row.get(0).map_err(sql_err)?,
                    row.get(1).map_err(sql_err)?,
                    row.get(2).map_err(sql_err)?,
                    row.get(3).map_err(sql_err)?,
                ))),
            }
        })
        .await
    }

    pub async fn get_repository_by_local_path(&self, path: &Path) -> Result<Option<Repository>> {
        let path = path_to_text(path)?;
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, canonical_remote, local_path, default_branch
                     FROM repositories WHERE local_path = ?1 ORDER BY id ASC LIMIT 1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![path]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => Ok(Some(row_to_repository(
                    row.get(0).map_err(sql_err)?,
                    row.get(1).map_err(sql_err)?,
                    row.get(2).map_err(sql_err)?,
                    row.get(3).map_err(sql_err)?,
                ))),
            }
        })
        .await
    }

    pub async fn upsert_membership(&self, membership: &WorkspaceMembership) -> Result<()> {
        let membership = membership.clone();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO workspace_repositories
                    (workspace_id, repository_id, alias, default_enabled)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(workspace_id, repository_id) DO UPDATE SET
                    alias = excluded.alias,
                    default_enabled = excluded.default_enabled",
                params![
                    membership.workspace_id.to_string(),
                    membership.repository_id.to_string(),
                    membership.alias,
                    membership.default_enabled as i64,
                ],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn get_thread(&self, id: &ThreadId) -> Result<Option<Thread>> {
        let id = id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, workspace_id, parent_thread_id, kind, title FROM threads WHERE id = ?1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![id]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => {
                    let id: String = row.get(0).map_err(sql_err)?;
                    let ws: Option<String> = row.get(1).map_err(sql_err)?;
                    let parent: Option<String> = row.get(2).map_err(sql_err)?;
                    let kind: String = row.get(3).map_err(sql_err)?;
                    let title: Option<String> = row.get(4).map_err(sql_err)?;
                    let repo_attachments = load_attachments(conn, &id)?;
                    Ok(Some(Thread {
                        id: ThreadId::from(id),
                        workspace_id: ws.map(WorkspaceId::from),
                        parent_thread_id: parent.map(ThreadId::from),
                        kind: enum_from_text(&kind)?,
                        title,
                        repo_attachments,
                    }))
                }
            }
        })
        .await
    }

    /// Repositories attached to a thread, in attachment insert order (`rowid`).
    pub async fn list_thread_repositories(&self, thread_id: &ThreadId) -> Result<Vec<Repository>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT r.id, r.canonical_remote, r.local_path, r.default_branch
                     FROM thread_repositories tr
                     JOIN repositories r ON r.id = tr.repository_id
                     WHERE tr.thread_id = ?1
                     ORDER BY tr.rowid ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![thread_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, remote, local_path, branch) = row.map_err(sql_err)?;
                out.push(row_to_repository(id, remote, local_path, branch));
            }
            Ok(out)
        })
        .await
    }

    pub async fn latest_agent_session_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<AgentSession>> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, external_session_id, model, cwd
                     FROM agent_sessions
                     WHERE thread_id = ?1
                     ORDER BY started_at DESC, id DESC
                     LIMIT 1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![thread_id]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(None),
                Some(row) => {
                    let id: String = row.get(0).map_err(sql_err)?;
                    let external: Option<String> = row.get(1).map_err(sql_err)?;
                    let model: Option<String> = row.get(2).map_err(sql_err)?;
                    let cwd: String = row.get(3).map_err(sql_err)?;
                    Ok(Some(AgentSession {
                        id: AgentSessionId::from(id),
                        external_session_id: external,
                        model,
                        cwd: PathBuf::from(cwd),
                    }))
                }
            }
        })
        .await
    }

    pub async fn set_agent_session_external(
        &self,
        session_id: &AgentSessionId,
        external_session_id: Option<String>,
        model: Option<String>,
    ) -> Result<()> {
        let session_id = session_id.to_string();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE agent_sessions
                 SET external_session_id = COALESCE(?2, external_session_id),
                     model = COALESCE(?3, model)
                 WHERE id = ?1",
                params![session_id, external_session_id, model],
            )
            .map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn usage_totals_for_thread(&self, thread_id: &ThreadId) -> Result<UsageReport> {
        let thread_id = thread_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT
                        COALESCE(
                            (SELECT model FROM usage_events WHERE thread_id = ?1 ORDER BY seq DESC LIMIT 1),
                            ''
                        ),
                        COALESCE(SUM(tokens_in), 0),
                        COALESCE(SUM(tokens_out), 0),
                        COALESCE(SUM(cache_read), 0),
                        COALESCE(SUM(cache_write), 0)
                     FROM usage_events
                     WHERE thread_id = ?1",
                )
                .map_err(sql_err)?;
            let mut rows = stmt.query(params![thread_id]).map_err(sql_err)?;
            match rows.next().map_err(sql_err)? {
                None => Ok(UsageReport {
                    model: String::new(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                }),
                Some(row) => Ok(UsageReport {
                    model: row.get(0).map_err(sql_err)?,
                    input_tokens: row.get::<_, i64>(1).map_err(sql_err)? as u64,
                    output_tokens: row.get::<_, i64>(2).map_err(sql_err)? as u64,
                    cache_read_tokens: row.get::<_, i64>(3).map_err(sql_err)? as u64,
                    cache_write_tokens: row.get::<_, i64>(4).map_err(sql_err)? as u64,
                }),
            }
        })
        .await
    }

    /// Insert the workspace row and its `WorkspaceCreated` event in one
    /// transaction. The duplicate-name check runs inside the transaction
    /// (`INSERT ... WHERE NOT EXISTS`), so two creators — including on
    /// separate connections to the same database file — cannot both insert:
    /// exactly one row is written and the loser gets an error. Existing rows
    /// (including legacy duplicate names) are never deleted or rewritten.
    pub async fn create_workspace_with_event(
        &self,
        workspace: &Workspace,
        event: &EventRecord,
    ) -> Result<()> {
        let workspace = workspace.clone();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            let now = rfc3339(Utc::now());
            let inserted = tx
                .execute(
                    "INSERT INTO workspaces (id, name, settings_json, created_at, updated_at)
                     SELECT ?1, ?2, ?3, ?4, ?5
                     WHERE NOT EXISTS (SELECT 1 FROM workspaces WHERE name = ?2)",
                    params![
                        workspace.id.to_string(),
                        workspace.name,
                        to_json(&workspace.settings)?,
                        now.clone(),
                        now,
                    ],
                )
                .map_err(sql_err)?;
            if inserted != 1 {
                return Err(trama_domain::DomainError::Invalid(format!(
                    "a workspace named `{}` already exists",
                    workspace.name
                )));
            }
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }

    pub async fn list_all_repositories(&self) -> Result<Vec<Repository>> {
        self.blocking(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, canonical_remote, local_path, default_branch
                     FROM repositories
                     ORDER BY id ASC",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, remote, local_path, branch) = row.map_err(sql_err)?;
                out.push(row_to_repository(id, remote, local_path, branch));
            }
            Ok(out)
        })
        .await
    }

    /// Insert or reuse a repository by canonical local path, then upsert membership.
    /// The returned id is the stored row, which may differ from `repository.id`.
    pub async fn upsert_repository_membership(
        &self,
        repository: &Repository,
        membership: &WorkspaceMembership,
    ) -> Result<Repository> {
        let repository = repository.clone();
        let membership = membership.clone();
        self.blocking_mut(move |conn| {
            let local_path = repository.local_path.clone().ok_or_else(|| {
                trama_domain::DomainError::Invalid("repository upsert requires a local path".into())
            })?;
            let local_path_text = path_to_text(&local_path)?;
            let tx = conn.transaction().map_err(sql_err)?;
            let existing = find_repository_by_canonical_path(&tx, &local_path)?;
            let stored = if let Some(existing) = existing {
                let canonical_remote = repository
                    .canonical_remote
                    .clone()
                    .or(existing.canonical_remote.clone());
                let default_branch = repository
                    .default_branch
                    .clone()
                    .or(existing.default_branch.clone());
                tx.execute(
                    "UPDATE repositories
                     SET canonical_remote = ?2, local_path = ?3, default_branch = ?4
                     WHERE id = ?1",
                    params![
                        existing.id.to_string(),
                        canonical_remote,
                        local_path_text,
                        default_branch,
                    ],
                )
                .map_err(sql_err)?;
                Repository {
                    id: existing.id,
                    canonical_remote,
                    local_path: Some(local_path),
                    default_branch,
                }
            } else {
                tx.execute(
                    "INSERT INTO repositories (id, canonical_remote, local_path, default_branch)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        repository.id.to_string(),
                        repository.canonical_remote,
                        local_path_text,
                        repository.default_branch,
                    ],
                )
                .map_err(sql_err)?;
                repository
            };
            tx.execute(
                "INSERT INTO workspace_repositories
                    (workspace_id, repository_id, alias, default_enabled)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(workspace_id, repository_id) DO UPDATE SET
                    alias = excluded.alias,
                    default_enabled = excluded.default_enabled",
                params![
                    membership.workspace_id.to_string(),
                    stored.id.to_string(),
                    membership.alias,
                    membership.default_enabled as i64,
                ],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
            Ok(stored)
        })
        .await
    }

    /// Replace checkout metadata for the given attachments, clear the external
    /// session ids of the thread's agent sessions so the next prompt does not
    /// resume a pre-migration session (even on a mixed thread whose first
    /// checkout — and therefore cwd — is unchanged), and append one system
    /// message plus its event. A failure rolls back every attachment change,
    /// the session invalidation, and the message.
    pub async fn persist_thread_migration(
        &self,
        thread_id: &ThreadId,
        attachments: &[ThreadRepositoryAttachment],
        message: &Message,
        event: &EventRecord,
    ) -> Result<()> {
        self.persist_thread_migration_with_locations(thread_id, attachments, message, event, &[])
            .await
    }

    pub async fn persist_thread_migration_with_locations(
        &self,
        thread_id: &ThreadId,
        attachments: &[ThreadRepositoryAttachment],
        message: &Message,
        event: &EventRecord,
        locations: &[crate::CheckoutLocation],
    ) -> Result<()> {
        let thread_id = thread_id.to_string();
        let attachments = attachments.to_vec();
        let locations = locations.to_vec();
        let message = message.clone();
        let event = event.clone();
        self.blocking_mut(move |conn| {
            let tx = conn.transaction().map_err(sql_err)?;
            for attachment in &attachments {
                let updated = tx
                    .execute(
                        "UPDATE thread_repositories
                         SET base_revision = ?3, checkout_id = ?4, attach_mode = ?5
                         WHERE thread_id = ?1 AND repository_id = ?2",
                        params![
                            thread_id,
                            attachment.repository_id.to_string(),
                            attachment.base_revision,
                            attachment.checkout_id.as_ref().map(|id| id.to_string()),
                            enum_text(&attachment.attach_mode)?,
                        ],
                    )
                    .map_err(sql_err)?;
                if updated != 1 {
                    return Err(trama_domain::DomainError::Invalid(format!(
                        "thread attachment {} was not updated",
                        attachment.repository_id
                    )));
                }
            }
            crate::checkouts::insert_locations(&tx, &thread_id, &attachments, &locations)?;
            tx.execute(
                "UPDATE agent_sessions SET external_session_id = NULL WHERE thread_id = ?1",
                params![thread_id],
            )
            .map_err(sql_err)?;
            insert_message(&tx, &message)?;
            insert_event(&tx, &event)?;
            tx.commit().map_err(sql_err)?;
            Ok(())
        })
        .await
    }
}

fn row_to_workspace(id: String, name: String, settings_json: String) -> Result<Workspace> {
    Ok(Workspace {
        id: WorkspaceId::from(id),
        name,
        settings: from_json(&settings_json)?,
    })
}

fn row_to_repository(
    id: String,
    canonical_remote: Option<String>,
    local_path: Option<String>,
    default_branch: Option<String>,
) -> Repository {
    Repository {
        id: RepositoryId::from(id),
        canonical_remote,
        local_path: local_path.map(PathBuf::from),
        default_branch,
    }
}

fn find_repository_by_canonical_path(
    tx: &rusqlite::Transaction<'_>,
    canonical: &Path,
) -> Result<Option<Repository>> {
    let mut stmt = tx
        .prepare(
            "SELECT id, canonical_remote, local_path, default_branch FROM repositories ORDER BY id ASC",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(sql_err)?;
    for row in rows {
        let (id, remote, local_path, branch) = row.map_err(sql_err)?;
        let repository = row_to_repository(id, remote, local_path, branch);
        if repository
            .local_path
            .as_deref()
            .is_some_and(|path| same_canonical_path(path, canonical))
        {
            return Ok(Some(repository));
        }
    }
    Ok(None)
}

fn same_canonical_path(stored: &Path, canonical: &Path) -> bool {
    if stored == canonical {
        return true;
    }
    fs_canonicalize_exists(stored).is_some_and(|stored| stored == canonical)
}

fn fs_canonicalize_exists(path: &Path) -> Option<PathBuf> {
    dunce::canonicalize(path).ok()
}

pub(crate) fn insert_message(conn: &rusqlite::Connection, message: &Message) -> Result<()> {
    let annotation = match &message.annotation {
        Some(annotation) => Some(to_json(annotation)?),
        None => None,
    };
    conn.execute(
        "INSERT INTO messages
            (id, thread_id, parent_id, actor_json, role, content_json, created_at,
             claude_uuid, annotation_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            message.id.to_string(),
            message.thread_id.to_string(),
            message.parent_id.as_ref().map(|id| id.to_string()),
            to_json(&message.actor)?,
            enum_text(&message.role)?,
            to_json(&message.content)?,
            rfc3339(message.created_at),
            message.claude_uuid,
            annotation,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

pub(crate) fn insert_event(tx: &rusqlite::Transaction<'_>, record: &EventRecord) -> Result<()> {
    let insert = tx.execute(
        "INSERT INTO events (
            id, aggregate_json, actor_json, schema_version, kind,
            payload_json, parents_json, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            record.id,
            to_json(&record.aggregate)?,
            to_json(&record.actor)?,
            record.schema_version as i64,
            record.kind,
            to_json(&record.payload)?,
            to_json(&record.parents)?,
            rfc3339(record.created_at),
        ],
    );
    match insert {
        Ok(_) => Ok(()),
        Err(err) if is_constraint_violation(&err) => Err(trama_domain::DomainError::Invalid(
            format!("duplicate event id: {}", record.id),
        )),
        Err(err) => Err(sql_err(err)),
    }
}
