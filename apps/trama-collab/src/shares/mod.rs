//! vNext shares (`/v3/shares`, docs/plan/03 §4): membership, a sequenced
//! durable event log with replay protection, CAS manifest commits, writer
//! leases and a WebSocket stream. Event bodies and manifests are opaque
//! ciphertext; the server never parses them.

mod error;
mod events;
pub(crate) mod fanout;
mod keys;
mod leases;
mod manifests;
mod members;
mod revoke;
mod stream;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use trama_collab_protocol::vnext::api::{ShareRole, V3_BODY_LIMIT};

use crate::AppState;
pub(crate) use error::ShareError;

pub(crate) fn migrate(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS v3_shares (
                 share_id TEXT PRIMARY KEY,
                 owner_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 last_seq INTEGER NOT NULL DEFAULT 0,
                 manifest_version INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS v3_share_members (
                 share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
                 user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 role TEXT NOT NULL CHECK (role IN ('owner', 'writer', 'reader')),
                 read_code INTEGER NOT NULL CHECK (read_code IN (0, 1)),
                 PRIMARY KEY (share_id, user_id)
             );
             CREATE TABLE IF NOT EXISTS v3_share_devices (
                 share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
                 device_id TEXT NOT NULL,
                 user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                 PRIMARY KEY (share_id, device_id)
             );
             CREATE TABLE IF NOT EXISTS v3_share_events (
                 share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
                 server_seq INTEGER NOT NULL,
                 event_id TEXT NOT NULL,
                 author_device TEXT NOT NULL,
                 author_seq INTEGER NOT NULL,
                 envelope TEXT NOT NULL,
                 PRIMARY KEY (share_id, server_seq),
                 UNIQUE (share_id, event_id),
                 UNIQUE (share_id, author_device, author_seq)
             );
             CREATE TABLE IF NOT EXISTS v3_share_manifests (
                 share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
                 version INTEGER NOT NULL,
                 history TEXT NOT NULL,
                 content TEXT,
                 publisher_id INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 PRIMARY KEY (share_id, version)
             );
             CREATE TABLE IF NOT EXISTS v3_share_leases (
                 share_id TEXT NOT NULL REFERENCES v3_shares(share_id) ON DELETE CASCADE,
                 thread_gid TEXT NOT NULL,
                 user_id INTEGER NOT NULL,
                 device_id TEXT NOT NULL,
                 expires_at INTEGER NOT NULL,
                 PRIMARY KEY (share_id, thread_gid)
             );",
        )
        .map_err(|e| format!("initialize share schema: {e}"))?;
    keys::migrate(connection)
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v3/shares", post(members::register))
        .route("/v3/shares/{share_id}", get(members::info))
        .route("/v3/shares/{share_id}/members", post(members::add_member))
        .route(
            "/v3/shares/{share_id}/members/{username}/revoke",
            post(revoke::revoke),
        )
        .route(
            "/v3/shares/{share_id}/events",
            post(events::publish).get(events::list),
        )
        .route("/v3/shares/{share_id}/stream", get(stream::socket))
        .route(
            "/v3/shares/{share_id}/snapshots/commit",
            post(manifests::commit),
        )
        .route(
            "/v3/shares/{share_id}/manifest/{version}",
            get(manifests::fetch),
        )
        .route(
            "/v3/shares/{share_id}/leases",
            post(leases::acquire).get(leases::list),
        )
        .route(
            "/v3/shares/{share_id}/leases/{thread_gid}/release",
            post(leases::release),
        )
        .route(
            "/v3/shares/{share_id}/key-packages",
            post(keys::post_key_package).get(keys::pending),
        )
        .route("/v3/shares/{share_id}/welcomes", post(keys::post_welcome))
        .route(
            "/v3/shares/{share_id}/welcomes/{device_id}",
            get(keys::get_welcome),
        )
        .layer(DefaultBodyLimit::max(V3_BODY_LIMIT))
}

/// The caller's grant on a share.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Grant {
    pub role: ShareRole,
    pub read_code: bool,
}

impl Grant {
    pub fn can_write(self) -> bool {
        matches!(self.role, ShareRole::Owner | ShareRole::Writer)
    }
}

pub(crate) fn role_str(role: ShareRole) -> &'static str {
    match role {
        ShareRole::Owner => "owner",
        ShareRole::Writer => "writer",
        ShareRole::Reader => "reader",
    }
}

fn parse_role(value: &str) -> Option<ShareRole> {
    Some(match value {
        "owner" => ShareRole::Owner,
        "writer" => ShareRole::Writer,
        "reader" => ShareRole::Reader,
        _ => return None,
    })
}

/// Same id rules as share events (ASCII, 1–128).
pub(crate) fn validate_id(id: &str) -> Result<(), ShareError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err(ShareError::Invalid("invalid id"))
    }
}

/// Non-members get `NotFound`, indistinguishable from a missing share.
pub(crate) fn grant(c: &Connection, share_id: &str, user_id: i64) -> Result<Grant, ShareError> {
    let row: Option<(String, i64)> = c
        .query_row(
            "SELECT role, read_code FROM v3_share_members WHERE share_id = ?1 AND user_id = ?2",
            params![share_id, user_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (role, read_code) = row.ok_or(ShareError::NotFound)?;
    let role = parse_role(&role).ok_or(ShareError::Database)?;
    Ok(Grant {
        role,
        read_code: role == ShareRole::Owner || read_code == 1,
    })
}

/// Runs `op` in an immediate transaction under the service write lock.
pub(crate) async fn write<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection) -> Result<T, ShareError> + Send + 'static,
) -> Result<T, ShareError> {
    let database = state.auth.database.clone();
    let hub = state.hub.clone();
    tokio::task::spawn_blocking(move || {
        let mut connection = crate::open_connection(&database).map_err(|_| ShareError::Database)?;
        let _guard = hub.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let value = op(&transaction)?;
        transaction.commit()?;
        Ok(value)
    })
    .await
    .map_err(|_| ShareError::Database)?
}

pub(crate) async fn read<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection) -> Result<T, ShareError> + Send + 'static,
) -> Result<T, ShareError> {
    let database = state.auth.database.clone();
    tokio::task::spawn_blocking(move || {
        let connection = crate::open_connection(&database).map_err(|_| ShareError::Database)?;
        op(&connection)
    })
    .await
    .map_err(|_| ShareError::Database)?
}
