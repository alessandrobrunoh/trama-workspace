//! Shared threads: ACL, durable ordered event log, idempotent commands and
//! the runner claim rules (ADR 0008, ADR 0009).
//!
//! Execution rule: a turn is authored by the person who pressed Send and can
//! only be claimed by a runner credential owned by that same person. Nothing
//! in this module ever offers a turn to another user's runner.

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    routing::{delete, get, post, put},
};
use getrandom::fill as random_fill;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use trama_collab_protocol::{
    AccessLevel, ChildCreated, CommandInfo, CommandKind, CommandReceipt, CommandState,
    CommentRequest, CreateChildRequest, CreateRunnerRequest, CreateThreadRequest, EventEnvelope,
    EventKind, EventsPage, InviteRequest, JoinRequest, MAX_PUBLISH_EVENTS,
    MAX_PUBLISH_PAYLOAD_BYTES, MemberInfo, MemberVia, PUBLISH_BODY_LIMIT, PendingTurn,
    PublishRequest, PublishResponse, ResolveRequest, RunnerCreated, RunnerInfo, RunnerReportKind,
    SendRequest, SetAccessRequest, SetAccessResponse, ThreadDetail, ThreadSummary, TurnOutcome,
    VerdictRequest,
};

use crate::{
    AppState, AuthError, Identity, bearer_token, live::Outbox, open_connection, tokio_blocking,
    valid_username,
};

/// HTTP body limit for collaboration routes (auth routes keep 4 KiB).
pub(crate) const COLLAB_BODY_LIMIT: usize = 64 * 1024;
/// Largest WebSocket message accepted from clients and runners.
pub(crate) const MAX_WS_MESSAGE_BYTES: usize = 96 * 1024;
pub(crate) const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_COMMENT_BYTES: usize = 16 * 1024;
const MAX_ANCHOR_BYTES: usize = 4 * 1024;
pub(crate) const MAX_RUNNER_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_TITLE_BYTES: usize = 200;
const MAX_COMMAND_ID_BYTES: usize = 64;
const MAX_ACTIVE_RUNNERS_PER_USER: i64 = 16;
/// Child threads (at any depth) under one root.
const MAX_CHILDREN_PER_ROOT: i64 = 2000;
const MAX_CHILD_SUBAGENT_BYTES: usize = 1024;
const MAX_FORK_ORIGIN_BYTES: usize = 128;
pub(crate) const MAX_PAGE: i64 = 500;
const DEFAULT_PAGE: i64 = 200;
/// Runner tokens carry a distinct prefix so they can never be parsed as a
/// session token (64 bare hex characters) and vice versa.
pub(crate) const RUNNER_TOKEN_PREFIX: &str = "trr_";

pub(crate) fn migrate(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS collab_threads (
                 id TEXT PRIMARY KEY,
                 owner_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 title TEXT NOT NULL,
                 access TEXT NOT NULL CHECK (access IN ('invited', 'link')),
                 link_token_hash BLOB,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS collab_members (
                 thread_id TEXT NOT NULL REFERENCES collab_threads(id) ON DELETE CASCADE,
                 user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 via TEXT NOT NULL CHECK (via IN ('owner', 'invite', 'link')),
                 added_at INTEGER NOT NULL,
                 PRIMARY KEY (thread_id, user_id)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS collab_members_user ON collab_members(user_id);
             CREATE TABLE IF NOT EXISTS collab_events (
                 thread_id TEXT NOT NULL REFERENCES collab_threads(id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL CHECK (seq > 0),
                 kind TEXT NOT NULL,
                 author_id INTEGER REFERENCES auth_users(id) ON DELETE SET NULL,
                 command_id TEXT,
                 payload TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 PRIMARY KEY (thread_id, seq)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS collab_runners (
                 id INTEGER PRIMARY KEY,
                 user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 name TEXT NOT NULL,
                 token_hash BLOB NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL,
                 revoked_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS collab_commands (
                 id INTEGER PRIMARY KEY,
                 author_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 command_id TEXT NOT NULL,
                 thread_id TEXT NOT NULL REFERENCES collab_threads(id) ON DELETE CASCADE,
                 kind TEXT NOT NULL,
                 state TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 runner_id INTEGER REFERENCES collab_runners(id),
                 runner_conn TEXT,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL,
                 UNIQUE (author_id, command_id)
             );
             CREATE INDEX IF NOT EXISTS collab_commands_state
                 ON collab_commands(author_id, kind, state);
             CREATE INDEX IF NOT EXISTS collab_commands_thread ON collab_commands(thread_id, seq);
             CREATE INDEX IF NOT EXISTS collab_commands_conn
                 ON collab_commands(runner_conn) WHERE runner_conn IS NOT NULL;",
        )
        .map_err(|e| format!("initialize collaboration schema: {e}"))?;
    // Version 3: child threads (subthreads, forks, subagents) under a root
    // whose membership they inherit.
    if connection
        .prepare("SELECT root_id FROM collab_threads LIMIT 0")
        .is_err()
    {
        connection
            .execute_batch(
                "ALTER TABLE collab_threads ADD COLUMN parent_id TEXT;
                 ALTER TABLE collab_threads ADD COLUMN root_id TEXT;
                 ALTER TABLE collab_threads ADD COLUMN kind TEXT;",
            )
            .map_err(|e| format!("add child thread columns: {e}"))?;
    }
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS collab_threads_root ON collab_threads(root_id)
                 WHERE root_id IS NOT NULL;
             PRAGMA user_version=3;",
        )
        .map_err(|e| format!("initialize collaboration schema: {e}"))
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/threads", post(create_thread).get(list_threads))
        .route("/threads/{thread_id}", get(get_thread))
        .route("/threads/{thread_id}/access", put(set_access))
        .route("/threads/{thread_id}/invites", post(invite))
        .route(
            "/threads/{thread_id}/members/{username}",
            delete(remove_member),
        )
        .route("/threads/{thread_id}/join", post(join))
        .route("/threads/{thread_id}/events", get(list_events))
        .route("/threads/{thread_id}/commands", get(list_commands))
        .route("/threads/{thread_id}/send", post(send))
        .route("/threads/{thread_id}/comments", post(comment))
        .route(
            "/threads/{thread_id}/comments/{seq}/resolve",
            post(resolve_comment),
        )
        .route("/threads/{thread_id}/verdicts", post(verdict))
        .route(
            "/threads/{thread_id}/comments/{seq}/reopen",
            post(reopen_comment),
        )
        .route(
            "/threads/{thread_id}/comments/{seq}/delete",
            post(delete_comment),
        )
        .route("/threads/{thread_id}/children", post(create_child))
        .route(
            "/threads/{thread_id}/publish",
            post(publish).layer(DefaultBodyLimit::max(PUBLISH_BODY_LIMIT)),
        )
        .route(
            "/threads/{thread_id}/ws",
            get(crate::sockets::thread_socket),
        )
        .route("/runners", post(create_runner).get(list_runners))
        .route("/runners/{runner_id}", delete(revoke_runner))
        .route("/runner/ws", get(crate::sockets::runner_socket))
        .layer(DefaultBodyLimit::max(COLLAB_BODY_LIMIT))
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// snake_case name of a unit enum variant, matching the wire format.
pub(crate) fn tag<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => unreachable!("unit enum variants serialize as strings"),
    }
}

fn untag<T: DeserializeOwned>(text: String) -> rusqlite::Result<T> {
    serde_json::from_value(Value::String(text)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn db<E>(_: E) -> AuthError {
    AuthError::Database
}

pub(crate) fn random_hex(bytes: usize) -> Result<String, AuthError> {
    let mut buffer = vec![0_u8; bytes];
    random_fill(&mut buffer).map_err(db)?;
    let text = hex::encode(&buffer);
    buffer.fill(0);
    Ok(text)
}

fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Thread ids are 32 lowercase hex characters; anything else is "not found"
/// without touching the database.
pub(crate) fn validate_thread_id(thread_id: &str) -> Result<(), AuthError> {
    if is_lower_hex(thread_id, 32) {
        Ok(())
    } else {
        Err(AuthError::NotFound)
    }
}

fn validate_command_id(command_id: &str) -> Result<(), AuthError> {
    if !command_id.is_empty()
        && command_id.len() <= MAX_COMMAND_ID_BYTES
        && command_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(AuthError::InvalidInput)
    }
}

/// Free text (titles, messages, comments) must be within `max` bytes and
/// contain no control or line/paragraph separator characters other than
/// newline and tab, so it cannot inject into terminals or logs.
fn validate_text(text: &str, max: usize, allow_newlines: bool) -> Result<(), AuthError> {
    let ok = !text.trim().is_empty()
        && text.len() <= max
        && text.chars().all(|c| {
            (allow_newlines && matches!(c, '\n' | '\t'))
                || !(c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        });
    if ok {
        Ok(())
    } else {
        Err(AuthError::InvalidInput)
    }
}

pub(crate) fn validate_draft(text: &str) -> Result<(), AuthError> {
    if text.is_empty() {
        return Ok(());
    }
    validate_text(text, MAX_TEXT_BYTES, true)
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// Store primitives (synchronous, run on blocking threads)
// ---------------------------------------------------------------------------

struct ThreadRow {
    title: String,
    /// Owner and access of the root (a child has no ACL of its own).
    owner_id: i64,
    owner: String,
    access: AccessLevel,
    via: MemberVia,
    parent_id: Option<String>,
    root_id: Option<String>,
    kind: Option<String>,
}

impl ThreadRow {
    fn summary(self, thread_id: String, last_seq: i64) -> ThreadSummary {
        ThreadSummary {
            thread_id,
            title: self.title,
            owner: self.owner,
            access: self.access,
            via: self.via,
            last_seq,
            parent_thread_id: self.parent_id,
            root_thread_id: self.root_id,
            kind: self.kind,
        }
    }
}

/// Returns the thread only if `user_id` is a member of it, or of the root
/// a child thread belongs to; otherwise `NotFound`, whether or not the
/// thread exists. Child threads inherit the root's membership, so revoking
/// access to the root revokes the whole family at once.
fn member_thread(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<ThreadRow, AuthError> {
    connection
        .query_row(
            "SELECT t.title, r.owner_id, o.username, r.access, m.via, t.parent_id, t.root_id, t.kind
             FROM collab_threads t
             JOIN collab_threads r ON r.id = COALESCE(t.root_id, t.id)
             JOIN collab_members m ON m.thread_id = r.id
             JOIN auth_users o ON o.id = r.owner_id
             WHERE t.id = ?1 AND m.user_id = ?2",
            params![thread_id, user_id],
            |row| {
                Ok(ThreadRow {
                    title: row.get(0)?,
                    owner_id: row.get(1)?,
                    owner: row.get(2)?,
                    access: untag(row.get(3)?)?,
                    via: untag(row.get(4)?)?,
                    parent_id: row.get(5)?,
                    root_id: row.get(6)?,
                    kind: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::NotFound)
}

/// Owner-only actions apply to a root thread; a child's access is its
/// root's, so managing it there is `400`.
fn owner_thread(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<ThreadRow, AuthError> {
    let thread = member_thread(connection, thread_id, user_id)?;
    if thread.owner_id != user_id {
        return Err(AuthError::Forbidden);
    }
    if thread.root_id.is_some() {
        return Err(AuthError::InvalidInput);
    }
    Ok(thread)
}

pub(crate) fn is_member(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
) -> Result<bool, AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_threads t
             JOIN collab_members m ON m.thread_id = COALESCE(t.root_id, t.id)
             WHERE t.id = ?1 AND m.user_id = ?2",
            params![thread_id, user_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(db)
}

/// Usernames with access to `thread_id` (the root's members).
pub(crate) fn member_names(
    connection: &Connection,
    thread_id: &str,
) -> Result<Vec<String>, AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
             WHERE m.thread_id = (SELECT COALESCE(root_id, id) FROM collab_threads WHERE id = ?1)",
        )
        .map_err(db)?;
    statement
        .query_map([thread_id], |row| row.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

/// A root and every child under it: the threads whose sockets must
/// re-check access when the root's membership changes.
fn family_ids(connection: &Connection, root_id: &str) -> Result<Vec<String>, AuthError> {
    let mut statement = connection
        .prepare("SELECT id FROM collab_threads WHERE id = ?1 OR root_id = ?1")
        .map_err(db)?;
    statement
        .query_map([root_id], |row| row.get::<_, String>(0))
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

fn last_seq(connection: &Connection, thread_id: &str) -> Result<i64, AuthError> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM collab_events WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .map_err(db)
}

/// Appends one event with the next sequence number. Must run inside an
/// IMMEDIATE transaction so concurrent writers cannot pick the same `seq`.
#[allow(clippy::too_many_arguments)]
fn append_event(
    connection: &Connection,
    outbox: &mut Outbox,
    thread_id: &str,
    kind: EventKind,
    author: Option<&Identity>,
    command_id: Option<&str>,
    payload: Value,
    now: i64,
) -> Result<EventEnvelope, AuthError> {
    let seq = last_seq(connection, thread_id)? + 1;
    connection
        .execute(
            "INSERT INTO collab_events (thread_id, seq, kind, author_id, command_id, payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                thread_id,
                seq,
                tag(&kind),
                author.map(|a| a.user_id),
                command_id,
                payload.to_string(),
                now
            ],
        )
        .map_err(db)?;
    let event = EventEnvelope {
        thread_id: thread_id.to_owned(),
        seq,
        kind,
        author: author.map(|a| a.username.clone()),
        command_id: command_id.map(str::to_owned),
        created_at: now,
        payload,
    };
    outbox.events.push(event.clone());
    Ok(event)
}

/// Events after `after`, only if `user_id` is (still) a member. Every
/// backfill path goes through this check, so a revoked user cannot read
/// events through a reconnect or a lag recovery.
pub(crate) fn member_events(
    connection: &Connection,
    thread_id: &str,
    user_id: i64,
    after: i64,
    limit: i64,
) -> Result<EventsPage, AuthError> {
    member_thread(connection, thread_id, user_id)?;
    let mut statement = connection
        .prepare(
            "SELECT e.seq, e.kind, u.username, e.command_id, e.created_at, e.payload
             FROM collab_events e LEFT JOIN auth_users u ON u.id = e.author_id
             WHERE e.thread_id = ?1 AND e.seq > ?2 ORDER BY e.seq LIMIT ?3",
        )
        .map_err(db)?;
    let events = statement
        .query_map(params![thread_id, after, limit], |row| {
            let payload: String = row.get(5)?;
            Ok(EventEnvelope {
                thread_id: thread_id.to_owned(),
                seq: row.get(0)?,
                kind: untag(row.get(1)?)?,
                author: row.get(2)?,
                command_id: row.get(3)?,
                created_at: row.get(4)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)?;
    Ok(EventsPage {
        events,
        last_seq: last_seq(connection, thread_id)?,
    })
}

/// Looks up an earlier command with the same (author, command_id). A retry
/// returns the original receipt; reuse for another thread or kind conflicts.
fn existing_command(
    connection: &Connection,
    author_id: i64,
    command_id: &str,
    thread_id: &str,
    kind: CommandKind,
) -> Result<Option<CommandReceipt>, AuthError> {
    let row: Option<(String, String, String, i64)> = connection
        .query_row(
            "SELECT thread_id, kind, state, seq FROM collab_commands
             WHERE author_id = ?1 AND command_id = ?2",
            params![author_id, command_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(db)?;
    let Some((existing_thread, existing_kind, state, seq)) = row else {
        return Ok(None);
    };
    if existing_thread != thread_id || existing_kind != tag(&kind) {
        return Err(AuthError::Conflict);
    }
    Ok(Some(CommandReceipt {
        command_id: command_id.to_owned(),
        thread_id: thread_id.to_owned(),
        kind,
        state: untag(state).map_err(db)?,
        seq,
    }))
}

#[allow(clippy::too_many_arguments)]
fn insert_command(
    connection: &Connection,
    author_id: i64,
    command_id: &str,
    thread_id: &str,
    kind: CommandKind,
    state: CommandState,
    seq: i64,
    now: i64,
) -> Result<CommandReceipt, AuthError> {
    connection
        .execute(
            "INSERT INTO collab_commands
                 (author_id, command_id, thread_id, kind, state, seq, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![
                author_id,
                command_id,
                thread_id,
                tag(&kind),
                tag(&state),
                seq,
                now
            ],
        )
        .map_err(db)?;
    Ok(CommandReceipt {
        command_id: command_id.to_owned(),
        thread_id: thread_id.to_owned(),
        kind,
        state,
        seq,
    })
}

fn user_identity(connection: &Connection, user_id: i64) -> Result<Identity, AuthError> {
    connection
        .query_row(
            "SELECT username FROM auth_users WHERE id = ?1",
            [user_id],
            |row| row.get(0),
        )
        .map(|username| Identity { user_id, username })
        .map_err(db)
}

pub(crate) struct CreatedRunner {
    pub runner_id: i64,
    pub name: String,
    pub token: String,
}

pub(crate) fn insert_runner(
    connection: &Connection,
    user_id: i64,
    name: &str,
    now: i64,
) -> Result<CreatedRunner, AuthError> {
    if !valid_username(name) {
        return Err(AuthError::InvalidInput);
    }
    let active: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM collab_runners WHERE user_id = ?1 AND revoked_at IS NULL",
            [user_id],
            |row| row.get(0),
        )
        .map_err(db)?;
    if active >= MAX_ACTIVE_RUNNERS_PER_USER {
        return Err(AuthError::RateLimited);
    }
    let token = format!("{RUNNER_TOKEN_PREFIX}{}", random_hex(32)?);
    connection
        .execute(
            "INSERT INTO collab_runners (user_id, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![user_id, name, digest(&token), now],
        )
        .map_err(db)?;
    Ok(CreatedRunner {
        runner_id: connection.last_insert_rowid(),
        name: name.to_owned(),
        token,
    })
}

/// An authenticated runner credential. `owner` is the only user whose turns
/// this runner may see or claim.
#[derive(Clone, Debug)]
pub(crate) struct RunnerIdentity {
    pub runner_id: i64,
    pub name: String,
    pub owner: Identity,
}

pub(crate) fn runner_token(header: Option<&str>) -> Result<String, AuthError> {
    let token = header
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AuthError::Unauthorized)?;
    match token.strip_prefix(RUNNER_TOKEN_PREFIX) {
        Some(secret) if is_lower_hex(secret, 64) => Ok(token.to_owned()),
        _ => Err(AuthError::Unauthorized),
    }
}

pub(crate) fn lookup_runner(
    connection: &Connection,
    token: &str,
) -> Result<RunnerIdentity, AuthError> {
    connection
        .query_row(
            "SELECT r.id, r.name, u.id, u.username FROM collab_runners r
             JOIN auth_users u ON u.id = r.user_id
             WHERE r.token_hash = ?1 AND r.revoked_at IS NULL",
            [digest(token)],
            |row| {
                Ok(RunnerIdentity {
                    runner_id: row.get(0)?,
                    name: row.get(1)?,
                    owner: Identity {
                        user_id: row.get(2)?,
                        username: row.get(3)?,
                    },
                })
            },
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::Unauthorized)
}

pub(crate) fn runner_active(connection: &Connection, runner_id: i64) -> Result<bool, AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_runners WHERE id = ?1 AND revoked_at IS NULL",
            [runner_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(db)
}

/// Pending turns authored by `owner_id` in threads the owner still belongs
/// to. Turns authored by anyone else are never listed.
pub(crate) fn pending_turns(
    connection: &Connection,
    owner_id: i64,
) -> Result<Vec<PendingTurn>, AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT c.command_id, c.thread_id, c.seq, e.payload
             FROM collab_commands c
             JOIN collab_events e ON e.thread_id = c.thread_id AND e.seq = c.seq
             JOIN collab_threads t ON t.id = c.thread_id
             JOIN collab_members m
                 ON m.thread_id = COALESCE(t.root_id, t.id) AND m.user_id = c.author_id
             WHERE c.author_id = ?1 AND c.kind = 'turn' AND c.state = 'pending'
             ORDER BY c.id LIMIT ?2",
        )
        .map_err(db)?;
    statement
        .query_map(params![owner_id, MAX_PAGE], |row| {
            let payload: String = row.get(3)?;
            Ok(PendingTurn {
                command_id: row.get(0)?,
                thread_id: row.get(1)?,
                seq: row.get(2)?,
                text: turn_text(&payload),
            })
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)
}

fn turn_text(payload: &str) -> String {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value["text"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

pub(crate) enum ClaimOutcome {
    Claimed(PendingTurn),
    Rejected(&'static str),
}

struct TurnRow {
    id: i64,
    thread_id: String,
    seq: i64,
    state: CommandState,
    runner_conn: Option<String>,
}

/// Finds a turn authored by the runner's owner. The author filter is the
/// enforcement point of the sender-runs-turn rule.
fn owner_turn(
    connection: &Connection,
    runner: &RunnerIdentity,
    command_id: &str,
) -> Result<Option<TurnRow>, AuthError> {
    connection
        .query_row(
            "SELECT id, thread_id, seq, state, runner_conn FROM collab_commands
             WHERE author_id = ?1 AND command_id = ?2 AND kind = 'turn'",
            params![runner.owner.user_id, command_id],
            |row| {
                Ok(TurnRow {
                    id: row.get(0)?,
                    thread_id: row.get(1)?,
                    seq: row.get(2)?,
                    state: untag(row.get(3)?)?,
                    runner_conn: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(db)
}

pub(crate) fn claim_turn(
    connection: &Connection,
    outbox: &mut Outbox,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
    now: i64,
) -> Result<ClaimOutcome, AuthError> {
    if validate_command_id(command_id).is_err() {
        return Ok(ClaimOutcome::Rejected("not found"));
    }
    let Some(turn) = owner_turn(connection, runner, command_id)? else {
        return Ok(ClaimOutcome::Rejected("not found"));
    };
    if !is_member(connection, &turn.thread_id, runner.owner.user_id)? {
        return Ok(ClaimOutcome::Rejected("not found"));
    }
    let text: String = connection
        .query_row(
            "SELECT payload FROM collab_events WHERE thread_id = ?1 AND seq = ?2",
            params![turn.thread_id, turn.seq],
            |row| row.get(0),
        )
        .map_err(db)?;
    let pending = PendingTurn {
        command_id: command_id.to_owned(),
        thread_id: turn.thread_id.clone(),
        seq: turn.seq,
        text: turn_text(&text),
    };
    match turn.state {
        CommandState::Pending => {}
        // A retried claim on the same connection is idempotent.
        CommandState::Claimed if turn.runner_conn.as_deref() == Some(connection_id) => {
            return Ok(ClaimOutcome::Claimed(pending));
        }
        CommandState::Claimed => return Ok(ClaimOutcome::Rejected("already claimed")),
        _ => return Ok(ClaimOutcome::Rejected("not pending")),
    }
    // Conditional update: only a still-pending turn can be claimed.
    let changed = connection
        .execute(
            "UPDATE collab_commands SET state = 'claimed', runner_id = ?1, runner_conn = ?2, updated_at = ?3
             WHERE id = ?4 AND state = 'pending'",
            params![runner.runner_id, connection_id, now, turn.id],
        )
        .map_err(db)?;
    if changed != 1 {
        return Ok(ClaimOutcome::Rejected("already claimed"));
    }
    append_event(
        connection,
        outbox,
        &turn.thread_id,
        EventKind::CommandClaimed,
        Some(&runner.owner),
        Some(command_id),
        json!({"runner": runner.name}),
        now,
    )?;
    Ok(ClaimOutcome::Claimed(pending))
}

/// The turn must be claimed by this very connection and its author must
/// still be a member of the thread.
fn claimed_by(
    connection: &Connection,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
) -> Result<TurnRow, &'static str> {
    let turn = owner_turn(connection, runner, command_id)
        .map_err(|_| "internal error")?
        .ok_or("not found")?;
    if turn.state != CommandState::Claimed || turn.runner_conn.as_deref() != Some(connection_id) {
        return Err("turn is not claimed by this runner connection");
    }
    if !is_member(connection, &turn.thread_id, runner.owner.user_id)
        .map_err(|_| "internal error")?
    {
        return Err("not found");
    }
    Ok(turn)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn report_turn(
    connection: &Connection,
    outbox: &mut Outbox,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
    kind: RunnerReportKind,
    payload: Value,
    now: i64,
) -> Result<Result<i64, &'static str>, AuthError> {
    if json_len(&payload) > MAX_RUNNER_PAYLOAD_BYTES {
        return Ok(Err("payload too large"));
    }
    let turn = match claimed_by(connection, runner, connection_id, command_id) {
        Ok(turn) => turn,
        Err(reason) => return Ok(Err(reason)),
    };
    let kind = match kind {
        RunnerReportKind::AgentEvent => EventKind::AgentEvent,
        RunnerReportKind::ChangesSummary => EventKind::ChangesSummary,
    };
    let event = append_event(
        connection,
        outbox,
        &turn.thread_id,
        kind,
        Some(&runner.owner),
        Some(command_id),
        payload,
        now,
    )?;
    Ok(Ok(event.seq))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_turn(
    connection: &Connection,
    outbox: &mut Outbox,
    runner: &RunnerIdentity,
    connection_id: &str,
    command_id: &str,
    outcome: TurnOutcome,
    result: Value,
    now: i64,
) -> Result<Result<(CommandState, i64), &'static str>, AuthError> {
    if json_len(&result) > MAX_RUNNER_PAYLOAD_BYTES {
        return Ok(Err("payload too large"));
    }
    let turn = match claimed_by(connection, runner, connection_id, command_id) {
        Ok(turn) => turn,
        Err(reason) => return Ok(Err(reason)),
    };
    let state = match outcome {
        TurnOutcome::Completed => CommandState::Completed,
        TurnOutcome::Failed => CommandState::Failed,
    };
    connection
        .execute(
            "UPDATE collab_commands SET state = ?1, runner_conn = NULL, updated_at = ?2 WHERE id = ?3",
            params![tag(&state), now, turn.id],
        )
        .map_err(db)?;
    let event = append_event(
        connection,
        outbox,
        &turn.thread_id,
        EventKind::CommandFinished,
        Some(&runner.owner),
        Some(command_id),
        json!({"state": state, "result": result}),
        now,
    )?;
    Ok(Ok((state, event.seq)))
}

/// Marks claims as ambiguous: those held by `connection_id`, or every open
/// claim when `None` (process restart). Never re-queues them.
fn abandon_claims(
    connection: &Connection,
    outbox: &mut Outbox,
    connection_id: Option<&str>,
    now: i64,
) -> Result<(), AuthError> {
    let mut statement = connection
        .prepare(
            "SELECT c.id, c.thread_id, c.command_id, c.author_id, r.name
             FROM collab_commands c LEFT JOIN collab_runners r ON r.id = c.runner_id
             WHERE c.state = 'claimed' AND (?1 IS NULL OR c.runner_conn = ?1)",
        )
        .map_err(db)?;
    let claims = statement
        .query_map([connection_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db)?;
    drop(statement);
    for (id, thread_id, command_id, author_id, runner) in claims {
        connection
            .execute(
                "UPDATE collab_commands SET state = 'ambiguous', runner_conn = NULL, updated_at = ?1
                 WHERE id = ?2",
                params![now, id],
            )
            .map_err(db)?;
        let author = user_identity(connection, author_id)?;
        let reason = if connection_id.is_some() {
            "runner disconnected after claiming"
        } else {
            "service restarted while the turn was claimed"
        };
        append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::CommandAmbiguous,
            Some(&author),
            Some(&command_id),
            json!({"runner": runner, "reason": reason}),
            now,
        )?;
    }
    Ok(())
}

pub(crate) fn abandon_connection(
    connection: &Connection,
    outbox: &mut Outbox,
    connection_id: &str,
    now: i64,
) -> Result<(), AuthError> {
    abandon_claims(connection, outbox, Some(connection_id), now)
}

/// Startup recovery; no subscribers exist yet, so the outbox is discarded.
pub(crate) fn abandon_all_claims(connection: &Connection, now: i64) -> Result<(), AuthError> {
    connection.execute_batch("BEGIN IMMEDIATE").map_err(db)?;
    let result = abandon_claims(connection, &mut Outbox::default(), None, now);
    let finish = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
    connection.execute_batch(finish).map_err(db)?;
    result
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

/// Runs `op` in an IMMEDIATE transaction while holding the hub write lock,
/// then publishes its outbox before releasing the lock, so live delivery
/// order equals commit (sequence) order.
pub(crate) async fn write<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection, &mut Outbox) -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    let database = state.auth.database.clone();
    let hub = state.hub.clone();
    tokio_blocking(move || {
        let mut connection = open_connection(&database).map_err(db)?;
        let _guard = hub
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let mut outbox = Outbox::default();
        let value = op(&transaction, &mut outbox)?;
        transaction.commit().map_err(db)?;
        hub.publish(outbox);
        Ok(value)
    })
    .await
}

pub(crate) async fn read<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection) -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    let database = state.auth.database.clone();
    tokio_blocking(move || {
        let connection = open_connection(&database).map_err(db)?;
        op(&connection)
    })
    .await
}

pub(crate) async fn session(state: &AppState, headers: &HeaderMap) -> Result<Identity, AuthError> {
    let token = bearer_token(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()))?;
    state.auth.authenticate(token).await
}

async fn mutating_session(state: &AppState, headers: &HeaderMap) -> Result<Identity, AuthError> {
    let identity = session(state, headers).await?;
    state
        .hub
        .allow_mutation(identity.user_id, state.auth.now())?;
    Ok(identity)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

async fn create_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateThreadRequest>,
) -> Result<(StatusCode, Json<ThreadSummary>), AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_text(&input.title, MAX_TITLE_BYTES, false)?;
    let now = state.auth.now();
    let thread_id = random_hex(16)?;
    let summary = write(&state, move |connection, outbox| {
        connection
            .execute(
                "INSERT INTO collab_threads (id, owner_id, title, access, created_at)
                 VALUES (?1, ?2, ?3, 'invited', ?4)",
                params![thread_id, identity.user_id, input.title, now],
            )
            .map_err(db)?;
        connection
            .execute(
                "INSERT INTO collab_members (thread_id, user_id, via, added_at) VALUES (?1, ?2, 'owner', ?3)",
                params![thread_id, identity.user_id, now],
            )
            .map_err(db)?;
        let event = append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::MemberJoined,
            Some(&identity),
            None,
            json!({"username": identity.username, "via": MemberVia::Owner}),
            now,
        )?;
        Ok(ThreadSummary {
            thread_id,
            title: input.title,
            owner: identity.username,
            access: AccessLevel::Invited,
            via: MemberVia::Owner,
            last_seq: event.seq,
            parent_thread_id: None,
            root_thread_id: None,
            kind: None,
        })
    })
    .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn list_threads(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ThreadSummary>>, AuthError> {
    let identity = session(&state, &headers).await?;
    let threads = read(&state, move |connection| {
        let mut statement = connection
            .prepare(
                "SELECT t.id, t.title, o.username, t.access, m.via,
                        (SELECT COALESCE(MAX(e.seq), 0) FROM collab_events e WHERE e.thread_id = t.id)
                 FROM collab_members m
                 JOIN collab_threads t ON t.id = m.thread_id
                 JOIN auth_users o ON o.id = t.owner_id
                 WHERE m.user_id = ?1 ORDER BY t.created_at, t.id LIMIT ?2",
            )
            .map_err(db)?;
        statement
            .query_map(params![identity.user_id, MAX_PAGE], |row| {
                Ok(ThreadSummary {
                    thread_id: row.get(0)?,
                    title: row.get(1)?,
                    owner: row.get(2)?,
                    access: untag(row.get(3)?)?,
                    via: untag(row.get(4)?)?,
                    last_seq: row.get(5)?,
                    parent_thread_id: None,
                    root_thread_id: None,
                    kind: None,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)
    })
    .await?;
    Ok(Json(threads))
}

async fn get_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadDetail>, AuthError> {
    let identity = session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let detail = read(&state, move |connection| {
        let thread = member_thread(connection, &thread_id, identity.user_id)?;
        let mut statement = connection
            .prepare(
                "SELECT u.username, m.via FROM collab_members m JOIN auth_users u ON u.id = m.user_id
                 WHERE m.thread_id = (SELECT COALESCE(root_id, id) FROM collab_threads WHERE id = ?1)
                 ORDER BY m.added_at, u.username",
            )
            .map_err(db)?;
        let members = statement
            .query_map([&thread_id], |row| {
                Ok(MemberInfo {
                    username: row.get(0)?,
                    via: untag(row.get(1)?)?,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)?;
        let last_seq = last_seq(connection, &thread_id)?;
        Ok(ThreadDetail {
            summary: thread.summary(thread_id, last_seq),
            members,
        })
    })
    .await?;
    Ok(Json(detail))
}

/// Owner only. `link` issues a fresh link token (rotating any previous one);
/// `invited` revokes the link and removes everyone who joined through it.
async fn set_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<SetAccessRequest>,
) -> Result<Json<SetAccessResponse>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let now = state.auth.now();
    let link_token = match input.access {
        AccessLevel::Link => Some(random_hex(32)?),
        AccessLevel::Invited => None,
    };
    let response = SetAccessResponse {
        access: input.access,
        link_token: link_token.clone(),
    };
    write(&state, move |connection, outbox| {
        let thread = owner_thread(connection, &thread_id, identity.user_id)?;
        connection
            .execute(
                "UPDATE collab_threads SET access = ?1, link_token_hash = ?2 WHERE id = ?3",
                params![
                    tag(&input.access),
                    link_token.as_deref().map(digest),
                    thread_id
                ],
            )
            .map_err(db)?;
        if input.access == AccessLevel::Invited {
            let mut statement = connection
                .prepare(
                    "SELECT u.id, u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
                     WHERE m.thread_id = ?1 AND m.via = 'link'",
                )
                .map_err(db)?;
            let removed = statement
                .query_map([&thread_id], |row| {
                    Ok(Identity {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                    })
                })
                .map_err(db)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db)?;
            drop(statement);
            connection
                .execute(
                    "DELETE FROM collab_members WHERE thread_id = ?1 AND via = 'link'",
                    [&thread_id],
                )
                .map_err(db)?;
            let family = family_ids(connection, &thread_id)?;
            for member in removed {
                append_event(
                    connection,
                    outbox,
                    &thread_id,
                    EventKind::MemberRemoved,
                    Some(&identity),
                    None,
                    json!({"username": member.username}),
                    now,
                )?;
                for id in &family {
                    outbox.removed.push((id.clone(), member.username.clone()));
                }
            }
        }
        if thread.access != input.access {
            append_event(
                connection,
                outbox,
                &thread_id,
                EventKind::AccessChanged,
                Some(&identity),
                None,
                json!({"access": input.access}),
                now,
            )?;
        }
        outbox.access_changed.extend(family_ids(connection, &thread_id)?);
        Ok(())
    })
    .await?;
    Ok(Json(response))
}

async fn invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<InviteRequest>,
) -> Result<StatusCode, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if !valid_username(&input.username) {
        return Err(AuthError::InvalidInput);
    }
    let now = state.auth.now();
    write(&state, move |connection, outbox| {
        owner_thread(connection, &thread_id, identity.user_id)?;
        let invitee: Identity = connection
            .query_row(
                "SELECT id, username FROM auth_users WHERE username = ?1 COLLATE NOCASE",
                [&input.username],
                |row| {
                    Ok(Identity {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db)?
            .ok_or(AuthError::NotFound)?;
        let current: Option<String> = connection
            .query_row(
                "SELECT via FROM collab_members WHERE thread_id = ?1 AND user_id = ?2",
                params![thread_id, invitee.user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        match current.as_deref() {
            // Idempotent: already invited (or the owner).
            Some("invite") | Some("owner") => return Ok(()),
            // An explicit invite outlives link revocation.
            Some(_) => {
                connection
                    .execute(
                        "UPDATE collab_members SET via = 'invite' WHERE thread_id = ?1 AND user_id = ?2",
                        params![thread_id, invitee.user_id],
                    )
                    .map_err(db)?;
                return Ok(());
            }
            None => {}
        }
        connection
            .execute(
                "INSERT INTO collab_members (thread_id, user_id, via, added_at) VALUES (?1, ?2, 'invite', ?3)",
                params![thread_id, invitee.user_id, now],
            )
            .map_err(db)?;
        append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::MemberJoined,
            Some(&identity),
            None,
            json!({"username": invitee.username, "via": MemberVia::Invite}),
            now,
        )?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, username)): Path<(String, String)>,
) -> Result<StatusCode, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if !valid_username(&username) {
        return Err(AuthError::NotFound);
    }
    let now = state.auth.now();
    write(&state, move |connection, outbox| {
        let thread = owner_thread(connection, &thread_id, identity.user_id)?;
        let member: Identity = connection
            .query_row(
                "SELECT u.id, u.username FROM collab_members m JOIN auth_users u ON u.id = m.user_id
                 WHERE m.thread_id = ?1 AND u.username = ?2 COLLATE NOCASE",
                params![thread_id, username],
                |row| {
                    Ok(Identity {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db)?
            .ok_or(AuthError::NotFound)?;
        if member.user_id == thread.owner_id {
            return Err(AuthError::InvalidInput);
        }
        connection
            .execute(
                "DELETE FROM collab_members WHERE thread_id = ?1 AND user_id = ?2",
                params![thread_id, member.user_id],
            )
            .map_err(db)?;
        append_event(
            connection,
            outbox,
            &thread_id,
            EventKind::MemberRemoved,
            Some(&identity),
            None,
            json!({"username": member.username}),
            now,
        )?;
        let family = family_ids(connection, &thread_id)?;
        for id in &family {
            outbox.removed.push((id.clone(), member.username.clone()));
        }
        outbox.access_changed.extend(family);
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Joins through the current link token. Any mismatch (no link, wrong or
/// rotated token, unknown thread) is the same `404`.
async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<JoinRequest>,
) -> Result<Json<ThreadSummary>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if !is_lower_hex(&input.link_token, 64) {
        return Err(AuthError::NotFound);
    }
    let now = state.auth.now();
    let summary = write(&state, move |connection, outbox| {
        let linked = connection
            .query_row(
                "SELECT 1 FROM collab_threads
                 WHERE id = ?1 AND access = 'link' AND link_token_hash = ?2",
                params![thread_id, digest(&input.link_token)],
                |_| Ok(()),
            )
            .optional()
            .map_err(db)?
            .is_some();
        if !linked {
            return Err(AuthError::NotFound);
        }
        if !is_member(connection, &thread_id, identity.user_id)? {
            connection
                .execute(
                    "INSERT INTO collab_members (thread_id, user_id, via, added_at) VALUES (?1, ?2, 'link', ?3)",
                    params![thread_id, identity.user_id, now],
                )
                .map_err(db)?;
            append_event(
                connection,
                outbox,
                &thread_id,
                EventKind::MemberJoined,
                Some(&identity),
                None,
                json!({"username": identity.username, "via": MemberVia::Link}),
                now,
            )?;
        }
        let thread = member_thread(connection, &thread_id, identity.user_id)?;
        let last_seq = last_seq(connection, &thread_id)?;
        Ok(thread.summary(thread_id, last_seq))
    })
    .await?;
    Ok(Json(summary))
}

#[derive(Deserialize)]
pub(crate) struct AfterQuery {
    #[serde(default)]
    pub after: i64,
    #[serde(default)]
    pub limit: Option<i64>,
}

async fn list_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Query(query): Query<AfterQuery>,
) -> Result<Json<EventsPage>, AuthError> {
    let identity = session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    if query.after < 0 || !(1..=MAX_PAGE).contains(&limit) {
        return Err(AuthError::InvalidInput);
    }
    let page = read(&state, move |connection| {
        member_events(connection, &thread_id, identity.user_id, query.after, limit)
    })
    .await?;
    Ok(Json(page))
}

async fn list_commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<CommandInfo>>, AuthError> {
    let identity = session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let commands = read(&state, move |connection| {
        member_thread(connection, &thread_id, identity.user_id)?;
        let mut statement = connection
            .prepare(
                "SELECT c.command_id, c.kind, u.username, c.state, c.seq, r.name
                 FROM collab_commands c
                 JOIN auth_users u ON u.id = c.author_id
                 LEFT JOIN collab_runners r ON r.id = c.runner_id
                 WHERE c.thread_id = ?1 AND c.kind = 'turn' ORDER BY c.seq DESC LIMIT ?2",
            )
            .map_err(db)?;
        let mut commands = statement
            .query_map(params![thread_id, MAX_PAGE], |row| {
                Ok(CommandInfo {
                    command_id: row.get(0)?,
                    thread_id: thread_id.clone(),
                    kind: untag(row.get(1)?)?,
                    author: row.get(2)?,
                    state: untag(row.get(3)?)?,
                    seq: row.get(4)?,
                    runner: row.get(5)?,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)?;
        commands.reverse();
        Ok(commands)
    })
    .await?;
    Ok(Json(commands))
}

/// Shared shape of every idempotent thread command: membership check, replay
/// of an earlier (author, command_id), else apply and record.
async fn run_command(
    state: &AppState,
    identity: Identity,
    thread_id: String,
    command_id: String,
    kind: CommandKind,
    apply: impl FnOnce(
        &Connection,
        &mut Outbox,
        &Identity,
        &str,
        i64,
    ) -> Result<EventEnvelope, AuthError>
    + Send
    + 'static,
) -> Result<(CommandReceipt, bool), AuthError> {
    validate_thread_id(&thread_id)?;
    validate_command_id(&command_id)?;
    let now = state.auth.now();
    write(state, move |connection, outbox| {
        member_thread(connection, &thread_id, identity.user_id)?;
        if let Some(receipt) =
            existing_command(connection, identity.user_id, &command_id, &thread_id, kind)?
        {
            return Ok((receipt, true));
        }
        let event = apply(connection, outbox, &identity, &command_id, now)?;
        let state = if kind == CommandKind::Turn {
            CommandState::Pending
        } else {
            CommandState::Applied
        };
        let receipt = insert_command(
            connection,
            identity.user_id,
            &command_id,
            &thread_id,
            kind,
            state,
            event.seq,
            now,
        )?;
        if kind == CommandKind::Turn {
            outbox.turns.push((
                identity.user_id,
                PendingTurn {
                    command_id: command_id.clone(),
                    thread_id: thread_id.clone(),
                    seq: event.seq,
                    text: event.payload["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                },
            ));
        }
        Ok((receipt, false))
    })
    .await
}

/// Send: the sender's text (or current live draft) becomes one user turn
/// authored by the sender. Only the sender's own runner may claim it.
async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<SendRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    let text = input
        .text
        .or_else(|| state.hub.draft(&thread_id, &identity.username));
    let username = identity.username.clone();
    let sent = text.clone();
    let (receipt, replayed) = run_command(
        &state,
        identity,
        thread_id.clone(),
        input.command_id,
        CommandKind::Turn,
        move |connection, outbox, identity, command_id, now| {
            let text = text.ok_or(AuthError::InvalidInput)?;
            validate_text(&text, MAX_TEXT_BYTES, true)?;
            append_event(
                connection,
                outbox,
                &thread_id,
                EventKind::MessageAppended,
                Some(identity),
                Some(command_id),
                json!({"text": text}),
                now,
            )
        },
    )
    .await?;
    if !replayed && let Some(sent) = sent {
        state
            .hub
            .clear_sent_draft(&receipt.thread_id, &username, &sent, state.auth.now());
    }
    Ok(Json(receipt))
}

async fn comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<CommentRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_text(&input.body, MAX_COMMENT_BYTES, true)?;
    if input
        .anchor
        .as_ref()
        .is_some_and(|anchor| json_len(anchor) > MAX_ANCHOR_BYTES)
    {
        return Err(AuthError::InvalidInput);
    }
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Comment,
        move |connection, outbox, identity, command_id, now| match input.parent_seq {
            Some(parent_seq) => {
                require_comment(connection, &thread, parent_seq)?;
                append_event(
                    connection,
                    outbox,
                    &thread,
                    EventKind::CommentReplied,
                    Some(identity),
                    Some(command_id),
                    json!({"parent_seq": parent_seq, "body": input.body}),
                    now,
                )
            }
            None => append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentCreated,
                Some(identity),
                Some(command_id),
                json!({"body": input.body, "anchor": input.anchor}),
                now,
            ),
        },
    )
    .await?;
    Ok(Json(receipt))
}

fn require_comment(connection: &Connection, thread_id: &str, seq: i64) -> Result<(), AuthError> {
    connection
        .query_row(
            "SELECT 1 FROM collab_events WHERE thread_id = ?1 AND seq = ?2 AND kind = 'comment_created'",
            params![thread_id, seq],
            |_| Ok(()),
        )
        .optional()
        .map_err(db)?
        .ok_or(AuthError::InvalidInput)
}

async fn resolve_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, seq)): Path<(String, i64)>,
    Json(input): Json<ResolveRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Resolve,
        move |connection, outbox, identity, command_id, now| {
            require_comment(connection, &thread, seq)?;
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentResolved,
                Some(identity),
                Some(command_id),
                json!({"comment_seq": seq}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

async fn verdict(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<VerdictRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    if let Some(body) = &input.body {
        validate_text(body, MAX_COMMENT_BYTES, true)?;
    }
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Verdict,
        move |connection, outbox, identity, command_id, now| {
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::ReviewVerdict,
                Some(identity),
                Some(command_id),
                json!({"verdict": input.verdict, "body": input.body}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

async fn reopen_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, seq)): Path<(String, i64)>,
    Json(input): Json<ResolveRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Reopen,
        move |connection, outbox, identity, command_id, now| {
            require_comment(connection, &thread, seq)?;
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentReopened,
                Some(identity),
                Some(command_id),
                json!({"comment_seq": seq}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

/// Only the comment's author may delete it (and its replies, implicitly).
async fn delete_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((thread_id, seq)): Path<(String, i64)>,
    Json(input): Json<ResolveRequest>,
) -> Result<Json<CommandReceipt>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let thread = thread_id.clone();
    let (receipt, _) = run_command(
        &state,
        identity,
        thread_id,
        input.command_id,
        CommandKind::Delete,
        move |connection, outbox, identity, command_id, now| {
            require_comment(connection, &thread, seq)?;
            let author: Option<i64> = connection
                .query_row(
                    "SELECT author_id FROM collab_events WHERE thread_id = ?1 AND seq = ?2",
                    params![thread, seq],
                    |row| row.get(0),
                )
                .map_err(db)?;
            if author != Some(identity.user_id) {
                return Err(AuthError::Forbidden);
            }
            append_event(
                connection,
                outbox,
                &thread,
                EventKind::CommentDeleted,
                Some(identity),
                Some(command_id),
                json!({"comment_seq": seq}),
                now,
            )
        },
    )
    .await?;
    Ok(Json(receipt))
}

fn valid_label(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Create a child thread (subthread, fork or subagent) under any thread of
/// a family. Any member may: the child is how their own agent's work in the
/// shared thread stays visible. The child inherits the root's membership;
/// a `child_thread_added` event in the parent's log announces it.
async fn create_child(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(parent_id): Path<String>,
    Json(input): Json<CreateChildRequest>,
) -> Result<(StatusCode, Json<ChildCreated>), AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&parent_id)?;
    validate_command_id(&input.command_id)?;
    validate_text(&input.title, MAX_TITLE_BYTES, false)?;
    if !valid_label(&input.kind, 32)
        || input
            .subagent
            .as_ref()
            .is_some_and(|value| !value.is_object() || json_len(value) > MAX_CHILD_SUBAGENT_BYTES)
        || input
            .fork_origin
            .as_deref()
            .is_some_and(|origin| !valid_label(origin, MAX_FORK_ORIGIN_BYTES))
    {
        return Err(AuthError::InvalidInput);
    }
    let now = state.auth.now();
    let child_id = random_hex(16)?;
    let created = write(&state, move |connection, outbox| {
        let parent = member_thread(connection, &parent_id, identity.user_id)?;
        if let Some(receipt) = existing_command(
            connection,
            identity.user_id,
            &input.command_id,
            &parent_id,
            CommandKind::Child,
        )? {
            // A retry: the child created the first time.
            let payload: String = connection
                .query_row(
                    "SELECT payload FROM collab_events WHERE thread_id = ?1 AND seq = ?2",
                    params![parent_id, receipt.seq],
                    |row| row.get(0),
                )
                .map_err(db)?;
            let child = serde_json::from_str::<Value>(&payload)
                .ok()
                .and_then(|v| v["child_thread_id"].as_str().map(str::to_owned))
                .ok_or(AuthError::Database)?;
            let row = member_thread(connection, &child, identity.user_id)?;
            let last = last_seq(connection, &child)?;
            return Ok(ChildCreated {
                thread: row.summary(child, last),
                seq: receipt.seq,
            });
        }
        let root = parent.root_id.clone().unwrap_or_else(|| parent_id.clone());
        let children: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM collab_threads WHERE root_id = ?1",
                [&root],
                |row| row.get(0),
            )
            .map_err(db)?;
        if children >= MAX_CHILDREN_PER_ROOT {
            return Err(AuthError::RateLimited);
        }
        connection
            .execute(
                "INSERT INTO collab_threads
                     (id, owner_id, title, access, created_at, parent_id, root_id, kind)
                 VALUES (?1, ?2, ?3, 'invited', ?4, ?5, ?6, ?7)",
                params![
                    child_id,
                    identity.user_id,
                    input.title,
                    now,
                    parent_id,
                    root,
                    input.kind
                ],
            )
            .map_err(db)?;
        let event = append_event(
            connection,
            outbox,
            &parent_id,
            EventKind::ChildThreadAdded,
            Some(&identity),
            Some(&input.command_id),
            json!({
                "child_thread_id": child_id,
                "title": input.title,
                "kind": input.kind,
                "subagent": input.subagent,
                "fork_origin": input.fork_origin,
            }),
            now,
        )?;
        insert_command(
            connection,
            identity.user_id,
            &input.command_id,
            &parent_id,
            CommandKind::Child,
            CommandState::Applied,
            event.seq,
            now,
        )?;
        let row = member_thread(connection, &child_id, identity.user_id)?;
        Ok(ChildCreated {
            thread: row.summary(child_id, 0),
            seq: event.seq,
        })
    })
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Append a batch of history, activity, changes or status events authored
/// by the caller, in order and in one transaction. Each event is idempotent
/// by its `command_id`. Counted as one mutation for rate limiting.
async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(thread_id): Path<String>,
    Json(input): Json<PublishRequest>,
) -> Result<Json<PublishResponse>, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    validate_thread_id(&thread_id)?;
    if input.events.is_empty() || input.events.len() > MAX_PUBLISH_EVENTS {
        return Err(AuthError::InvalidInput);
    }
    for event in &input.events {
        validate_command_id(&event.command_id)?;
        if !event.payload.is_object() || json_len(&event.payload) > MAX_PUBLISH_PAYLOAD_BYTES {
            return Err(AuthError::InvalidInput);
        }
    }
    let now = state.auth.now();
    let receipts = write(&state, move |connection, outbox| {
        member_thread(connection, &thread_id, identity.user_id)?;
        let mut receipts = Vec::with_capacity(input.events.len());
        for event in input.events {
            if let Some(receipt) = existing_command(
                connection,
                identity.user_id,
                &event.command_id,
                &thread_id,
                CommandKind::Publish,
            )? {
                receipts.push(receipt);
                continue;
            }
            let appended = append_event(
                connection,
                outbox,
                &thread_id,
                event.kind.event_kind(),
                Some(&identity),
                Some(&event.command_id),
                event.payload,
                now,
            )?;
            receipts.push(insert_command(
                connection,
                identity.user_id,
                &event.command_id,
                &thread_id,
                CommandKind::Publish,
                CommandState::Applied,
                appended.seq,
                now,
            )?);
        }
        Ok(receipts)
    })
    .await?;
    Ok(Json(PublishResponse { receipts }))
}

async fn create_runner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateRunnerRequest>,
) -> Result<(StatusCode, Json<RunnerCreated>), AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let now = state.auth.now();
    let created = read(&state, move |connection| {
        insert_runner(connection, identity.user_id, &input.name, now)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(RunnerCreated {
            runner_id: created.runner_id,
            name: created.name,
            token: created.token,
        }),
    ))
}

async fn list_runners(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<RunnerInfo>>, AuthError> {
    let identity = session(&state, &headers).await?;
    let runners = read(&state, move |connection| {
        let mut statement = connection
            .prepare(
                "SELECT id, name, created_at, revoked_at IS NOT NULL FROM collab_runners
                 WHERE user_id = ?1 ORDER BY id",
            )
            .map_err(db)?;
        statement
            .query_map([identity.user_id], |row| {
                Ok(RunnerInfo {
                    runner_id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    revoked: row.get(3)?,
                })
            })
            .map_err(db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db)
    })
    .await?;
    Ok(Json(runners))
}

/// Only the owner can revoke a runner; another user's runner id is `404`.
/// Open runner sockets close and their claims become ambiguous.
async fn revoke_runner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(runner_id): Path<i64>,
) -> Result<StatusCode, AuthError> {
    let identity = mutating_session(&state, &headers).await?;
    let now = state.auth.now();
    let changed = read(&state, move |connection| {
        connection
            .execute(
                "UPDATE collab_runners SET revoked_at = ?1
                 WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
                params![now, runner_id, identity.user_id],
            )
            .map_err(db)
    })
    .await?;
    if changed == 0 {
        return Err(AuthError::NotFound);
    }
    state.hub.credentials_changed();
    Ok(StatusCode::NO_CONTENT)
}
