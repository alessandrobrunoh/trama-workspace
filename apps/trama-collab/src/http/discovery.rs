//! `GET /.well-known/trama-collab` (docs/plan/03 §6): public metadata for
//! capability negotiation. Unauthenticated; exposes no user or share data.
//!
//! The service speaks the v2 thread protocol and the v3 share routes
//! (`2..=3`). A capability is listed only once the routes behind it exist:
//! event stream, manifest store and writer leases. Not E2EE (the MLS layer
//! is separate), content transfer, chunk resume or direct transport. `server_id` comes from `--server-id` /
//! `TRAMA_COLLAB_SERVER_ID` when set, else a random persisted id.

use crate::*;
use trama_collab_protocol::vnext::VNEXT_PROTOCOL_VERSION;
use trama_collab_protocol::vnext::discovery::{
    Capability, ServerDiscovery, ServerLimits, ServerPolicy,
};
use trama_collab_protocol::{
    COLLAB_PROTOCOL_VERSION, MAX_PUBLISH_EVENTS, MAX_PUBLISH_PAYLOAD_BYTES,
};

/// Creates the server identity table and a random, stable `server_id`.
pub(crate) fn migrate(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS collab_server_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .map_err(|e| format!("initialize server metadata: {e}"))?;
    let mut bytes = [0u8; 16];
    random_fill(&mut bytes).map_err(|e| format!("generate server id: {e}"))?;
    connection
        .execute(
            "INSERT OR IGNORE INTO collab_server_meta (key, value) VALUES ('server_id', ?1)",
            params![format!("srv_{}", hex::encode(bytes))],
        )
        .map_err(|e| format!("store server id: {e}"))?;
    Ok(())
}

/// Maximum length of an operator-chosen server id.
const MAX_SERVER_ID_LEN: usize = 128;

/// An operator-chosen id: 1–128 ASCII letters, digits, `-`, `_`, `.`, `:`.
pub(crate) fn validate_server_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= MAX_SERVER_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err("server id must be 1-128 characters of A-Z, a-z, 0-9, '-', '_', '.', ':'".into())
    }
}

fn server_id(connection: &Connection) -> Result<String, String> {
    connection
        .query_row(
            "SELECT value FROM collab_server_meta WHERE key = 'server_id'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
}

pub(crate) fn document(server_id: String) -> ServerDiscovery {
    ServerDiscovery {
        server_id,
        min_client_protocol: COLLAB_PROTOCOL_VERSION,
        max_client_protocol: VNEXT_PROTOCOL_VERSION,
        capabilities: vec![
            Capability::EventStream,
            Capability::ManifestStore,
            Capability::WriterLeases,
        ],
        limits: ServerLimits {
            max_event_payload_bytes: Some(MAX_PUBLISH_PAYLOAD_BYTES as u64),
            max_events_per_publish: Some(MAX_PUBLISH_EVENTS as u32),
            ..ServerLimits::default()
        },
        policy: ServerPolicy::default(),
    }
}

pub(crate) async fn well_known(State(state): State<AppState>) -> Response {
    let database = state.auth.database.clone();
    let configured = state.auth.config.server_id.clone();
    let id = tokio_blocking(move || {
        if let Some(id) = configured {
            return Ok(id);
        }
        open_connection(&database)
            .and_then(|connection| server_id(&connection))
            .map_err(|_| AuthError::Database)
    })
    .await;
    match id {
        Ok(id) => (
            [(::http::header::CACHE_CONTROL, "public, max-age=300")],
            Json(document(id)),
        )
            .into_response(),
        Err(err) => err.into_response(),
    }
}
