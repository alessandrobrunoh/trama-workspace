//! `GET /.well-known/trama-collab` (docs/plan/03 §6): public metadata for
//! capability negotiation. Unauthenticated; exposes no user or share data.
//!
//! This service still speaks only collab protocol v2, so it advertises
//! `2..=2` and no vNext capabilities. A capability is listed only once the
//! routes behind it exist.

use crate::*;
use trama_collab_protocol::vnext::discovery::{ServerDiscovery, ServerLimits, ServerPolicy};
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
        max_client_protocol: COLLAB_PROTOCOL_VERSION,
        capabilities: Vec::new(),
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
    let id = tokio_blocking(move || {
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
