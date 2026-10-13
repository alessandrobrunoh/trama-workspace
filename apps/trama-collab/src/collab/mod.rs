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

mod handlers;
mod storage;
mod validation;

pub(crate) use handlers::*;
pub(crate) use storage::*;
pub(crate) use validation::*;
