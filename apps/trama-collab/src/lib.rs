use std::{
    collections::HashMap,
    io::Read,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use argon2::{
    Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use getrandom::fill as random_fill;
use rand_core::OsRng;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

mod collab;
mod credentials;
mod database;
mod http;
mod service;
mod shares;
mod util;

use credentials::*;
use database::*;
use util::*;

pub use database::restore_database;
pub use util::{probe_health, read_secret, validate_bind};

pub use http::routes::{router, router_with_proxy};

use http::live::Hub;
pub use http::proxy::{ForwardedHeader, IpCidr, ProxyTrust};

const MAX_BODY_BYTES: usize = 4096;
const MAX_USERNAME_BYTES: usize = 64;
/// Maximum password material accepted from stdin in the provisioning CLI.
pub const MAX_PASSWORD_BYTES: usize = 1024;
const MIN_PASSWORD_BYTES: usize = 10;
const MAX_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Configuration for the standalone authentication service.
#[derive(Clone)]
pub struct AuthConfig {
    /// Session lifetime issued at login.
    pub session_ttl: Duration,
    /// Login attempts allowed per (lowercased username, peer IP) per window.
    pub max_login_attempts: usize,
    /// Total login attempts allowed per peer IP across all usernames per
    /// window, so spraying fresh usernames cannot bypass the per-key limit.
    pub max_peer_attempts: usize,
    /// Maximum number of distinct (username, peer) keys tracked at once.
    /// Expired keys are pruned first; while the cap stays full, new keys are
    /// refused with the throttle response to bound memory.
    pub max_throttle_keys: usize,
    /// Sliding window for both attempt limits.
    pub login_window: Duration,
    /// Operator-chosen `server_id` for `/.well-known/trama-collab`; `None`
    /// uses a random id persisted in the database.
    pub server_id: Option<String>,
    /// Injected clock (seconds since the Unix epoch) so limits are testable.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session_ttl: Duration::from_secs(12 * 60 * 60),
            max_login_attempts: 10,
            max_peer_attempts: 100,
            max_throttle_keys: 10_000,
            login_window: Duration::from_secs(60),
            server_id: None,
            clock: Arc::new(unix_time),
        }
    }
}

#[derive(Clone)]
pub struct AuthService {
    database: PathBuf,
    config: Arc<AuthConfig>,
    dummy_password_hash: Arc<String>,
}

#[derive(Clone)]
struct AppState {
    auth: AuthService,
    // Only login performs password hashing; this bounds concurrent hashing.
    hash_slots: Arc<Semaphore>,
    throttle: Arc<Mutex<ThrottleState>>,
    /// Live fan-out, ephemeral drafts/presence and the event write lock.
    hub: Arc<Hub>,
    /// Set only when the service runs behind a trusted reverse proxy; absent,
    /// forwarding headers are never read.
    proxy: Option<Arc<ProxyTrust>>,
    /// Live fan-out of `/v3` share streams.
    shares: Arc<shares::fanout::ShareFanout>,
}

/// An authenticated user session.
#[derive(Clone, Debug)]
struct Identity {
    user_id: i64,
    username: String,
}

/// In-memory sliding-window login attempt tracking. All state is lost when
/// the process restarts and is per process (one instance is expected).
#[derive(Default)]
struct ThrottleState {
    by_key: HashMap<(String, IpAddr), Vec<i64>>,
    by_peer: HashMap<IpAddr, Vec<i64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    expires_at: i64,
    username: String,
}

#[derive(Serialize)]
struct UserResponse {
    username: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: &'static str,
}

#[derive(Debug)]
enum AuthError {
    InvalidCredentials,
    Unauthorized,
    Throttled,
    InvalidInput,
    Database,
    /// Unknown resource or one the caller may not see; the two are
    /// deliberately indistinguishable.
    NotFound,
    /// Visible to the caller but reserved for the thread owner.
    Forbidden,
    /// A command id was reused for a different thread or command kind.
    Conflict,
    RateLimited,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::InvalidCredentials => (StatusCode::UNAUTHORIZED, "invalid credentials"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Throttled => (StatusCode::TOO_MANY_REQUESTS, "too many login attempts"),
            Self::InvalidInput => (StatusCode::BAD_REQUEST, "invalid request"),
            Self::Database => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Conflict => (StatusCode::CONFLICT, "command id already used"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "too many requests"),
        };
        (status, Json(ErrorResponse { error })).into_response()
    }
}
