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
mod live;
mod proxy;
mod sockets;

use live::Hub;
pub use proxy::{ForwardedHeader, IpCidr, ProxyTrust};

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

impl AuthService {
    /// Opens the service for serving: also marks turns that were claimed when
    /// the previous process stopped as ambiguous.
    pub fn open(path: impl AsRef<Path>, config: AuthConfig) -> Result<Self, String> {
        Self::open_inner(path.as_ref(), config, true)
    }

    /// Opens the database for operator commands (`create-user`,
    /// `create-runner`, `backup`) that may run next to a live server. Unlike
    /// [`AuthService::open`] it never touches runner claims, which belong to
    /// the running process.
    pub fn open_admin(path: impl AsRef<Path>, config: AuthConfig) -> Result<Self, String> {
        Self::open_inner(path.as_ref(), config, false)
    }

    fn open_inner(path: &Path, config: AuthConfig, recover_claims: bool) -> Result<Self, String> {
        if config.session_ttl.is_zero() || config.session_ttl > MAX_SESSION_TTL {
            return Err("session TTL must be between one second and 30 days".into());
        }
        if config.max_login_attempts == 0
            || config.max_peer_attempts == 0
            || config.max_throttle_keys == 0
            || config.login_window.is_zero()
        {
            return Err("login throttle settings must be nonzero".into());
        }
        let path = path.to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            // Only directories this service creates are restricted (0700);
            // pre-existing, possibly shared parent directories are never
            // chmodded.
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|e| format!("create database directory: {e}"))?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create database directory: {e}"))?;
        }
        let connection = open_connection(&path)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS auth_users (
                     id INTEGER PRIMARY KEY,
                     username TEXT NOT NULL COLLATE NOCASE UNIQUE,
                     password_hash TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS auth_sessions (
                     token_hash BLOB PRIMARY KEY,
                     user_id INTEGER NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
                     expires_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS auth_sessions_expiry ON auth_sessions(expires_at);",
            )
            .map_err(|e| format!("initialize database: {e}"))?;
        collab::migrate(&connection)?;
        // A claim that was open when the previous process stopped has an
        // unknown outcome: mark it ambiguous instead of offering it again.
        if recover_claims {
            collab::abandon_all_claims(&connection, (config.clock)())
                .map_err(|_| "recover runner claims".to_owned())?;
        }
        drop(connection);
        let mut dummy_password = [0_u8; 32];
        random_fill(&mut dummy_password).map_err(|e| format!("prepare dummy password: {e}"))?;
        let dummy_password_hash = hash_password(&hex::encode(dummy_password))
            .map_err(|e| format!("prepare dummy password hash: {e}"))?;
        dummy_password.fill(0);
        let service = Self {
            database: path,
            config: Arc::new(config),
            dummy_password_hash: Arc::new(dummy_password_hash),
        };
        service.cleanup_expired()?;
        Ok(service)
    }

    pub async fn create_user(&self, username: &str, password: &str) -> Result<(), String> {
        validate_credentials(username, password)
            .map_err(|_| "username or password does not meet input requirements".to_owned())?;
        let database = self.database.clone();
        let username = username.to_owned();
        let password = password.to_owned();
        let now = (self.config.clock)();
        tokio::task::spawn_blocking(move || {
            let hash = hash_password(&password).map_err(|e| e.to_string())?;
            let connection = open_connection(&database)?;
            connection.execute(
                "INSERT INTO auth_users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
                params![username, hash, now],
            ).map_err(|e| if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
                "username already exists".to_owned()
            } else { format!("create user: {e}") })?;
            Ok(())
        }).await.map_err(|e| format!("create-user task failed: {e}"))?
    }

    /// Writes a consistent snapshot of the database to `destination`
    /// (SQLite `VACUUM INTO`, safe while the server is running). The file is
    /// created owner-only and an existing path is never overwritten.
    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<(), String> {
        let destination = destination.as_ref();
        destination
            .to_str()
            .filter(|text| !text.is_empty() && !text.starts_with("file:"))
            .ok_or_else(|| "backup path must be a plain, non-empty UTF-8 file path".to_owned())?;
        create_private_file(destination)
            .map_err(|e| format!("create backup file (it must not exist): {e}"))?;
        let result = dunce::canonicalize(destination)
            .map_err(|e| format!("resolve backup path: {e}"))
            .and_then(|resolved| {
                // SQLite is opened with NOFOLLOW, which also rejects symlinked
                // parents such as /var on macOS; hand it the resolved path.
                let resolved = resolved
                    .to_str()
                    .ok_or_else(|| "backup path must be UTF-8".to_owned())?
                    .to_owned();
                let connection = open_connection(&self.database)?;
                connection
                    .execute("VACUUM INTO ?1", [resolved])
                    .map(|_| ())
                    .map_err(|e| format!("write backup: {e}"))
            });
        if result.is_err() {
            let _ = std::fs::remove_file(destination);
        }
        result
    }

    fn cleanup_expired(&self) -> Result<(), String> {
        let connection = open_connection(&self.database)?;
        connection
            .execute(
                "DELETE FROM auth_sessions WHERE expires_at <= ?1",
                [(self.config.clock)()],
            )
            .map_err(|e| format!("clean expired sessions: {e}"))?;
        Ok(())
    }

    async fn login(&self, username: String, password: String) -> Result<LoginResponse, AuthError> {
        validate_credentials(&username, &password).map_err(|_| AuthError::InvalidInput)?;
        let database = self.database.clone();
        let dummy_password_hash = self.dummy_password_hash.clone();
        let ttl = self.config.session_ttl;
        let now = (self.config.clock)();
        tokio_blocking(move || {
            let connection = open_connection(&database).map_err(|_| AuthError::Database)?;
            let row: Option<(i64, String)> = connection
                .query_row(
                    "SELECT id, password_hash FROM auth_users WHERE username = ?1 COLLATE NOCASE",
                    [&username],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|_| AuthError::Database)?;
            let (user_id, saved_hash) = row.unwrap_or((0, (*dummy_password_hash).clone()));
            let parsed = PasswordHash::new(&saved_hash).map_err(|_| AuthError::Database)?;
            let verified = Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok();
            if user_id == 0 || !verified {
                return Err(AuthError::InvalidCredentials);
            }

            let mut token_bytes = [0_u8; 32];
            random_fill(&mut token_bytes).map_err(|_| AuthError::Database)?;
            let token = hex::encode(token_bytes);
            token_bytes.fill(0);
            let token_hash = Sha256::digest(token.as_bytes()).to_vec();
            let expires_at = now.saturating_add(ttl.as_secs() as i64);
            connection.execute(
                "INSERT INTO auth_sessions (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
                params![token_hash, user_id, expires_at],
            ).map_err(|_| AuthError::Database)?;
            Ok(LoginResponse {
                token,
                expires_at,
                username,
            })
        })
        .await
    }

    /// Provisions a runner credential for `username` and returns the token
    /// once. Used by the operator CLI; owners can also call `POST /runners`.
    pub async fn create_runner(&self, username: &str, name: &str) -> Result<String, String> {
        let database = self.database.clone();
        let username = username.to_owned();
        let name = name.to_owned();
        let now = (self.config.clock)();
        tokio_blocking(move || {
            let connection = open_connection(&database).map_err(|_| AuthError::Database)?;
            let user_id: i64 = connection
                .query_row(
                    "SELECT id FROM auth_users WHERE username = ?1 COLLATE NOCASE",
                    [&username],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| AuthError::Database)?
                .ok_or(AuthError::NotFound)?;
            collab::insert_runner(&connection, user_id, &name, now).map(|created| created.token)
        })
        .await
        .map_err(|error| match error {
            AuthError::NotFound => "unknown username".to_owned(),
            AuthError::InvalidInput => "runner name does not meet input requirements".to_owned(),
            AuthError::RateLimited => "too many active runners for this user".to_owned(),
            _ => "create runner failed".to_owned(),
        })
    }

    fn now(&self) -> i64 {
        (self.config.clock)()
    }

    async fn authenticate(&self, token: String) -> Result<Identity, AuthError> {
        let database = self.database.clone();
        let now = (self.config.clock)();
        tokio_blocking(move || {
            let connection = open_connection(&database).map_err(|_| AuthError::Database)?;
            let digest = Sha256::digest(token.as_bytes()).to_vec();
            connection
                .query_row(
                    "SELECT users.id, users.username FROM auth_sessions sessions
                 JOIN auth_users users ON users.id = sessions.user_id
                 WHERE sessions.token_hash = ?1 AND sessions.expires_at > ?2",
                    params![digest, now],
                    |row| {
                        Ok(Identity {
                            user_id: row.get(0)?,
                            username: row.get(1)?,
                        })
                    },
                )
                .optional()
                .map_err(|_| AuthError::Database)?
                .ok_or(AuthError::Unauthorized)
        })
        .await
    }

    async fn current_user(&self, token: String) -> Result<String, AuthError> {
        self.authenticate(token)
            .await
            .map(|identity| identity.username)
    }

    async fn logout(&self, token: String) -> Result<(), AuthError> {
        let database = self.database.clone();
        tokio_blocking(move || {
            let connection = open_connection(&database).map_err(|_| AuthError::Database)?;
            let digest = Sha256::digest(token.as_bytes()).to_vec();
            connection
                .execute("DELETE FROM auth_sessions WHERE token_hash = ?1", [digest])
                .map_err(|_| AuthError::Database)?;
            Ok(())
        })
        .await
    }
}

/// The service for direct (loopback) use: forwarding headers are ignored.
pub fn router(auth: AuthService) -> Router {
    router_with_proxy(auth, None)
}

/// The service behind a reverse proxy. With `Some(trust)`, the client address
/// used for login throttling is taken from the configured forwarding header,
/// but only when the TCP peer is a trusted proxy (see [`ProxyTrust`]).
pub fn router_with_proxy(auth: AuthService, proxy: Option<ProxyTrust>) -> Router {
    let state = AppState {
        auth,
        hash_slots: Arc::new(Semaphore::new(8)),
        throttle: Arc::new(Mutex::new(ThrottleState::default())),
        hub: Arc::new(Hub::default()),
        proxy: proxy.map(Arc::new),
    };
    let auth_routes = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    auth_routes
        .route("/healthz", get(healthz))
        .merge(collab::routes())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            acquire_hash_slot,
        ))
        .with_state(state)
}

async fn acquire_hash_slot(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Only login performs password hashing; protect the CPU-heavy path from unbounded work.
    if request.uri().path() != "/auth/login" {
        return next.run(request).await;
    }
    let Ok(permit) = state.hash_slots.clone().try_acquire_owned() else {
        return AuthError::Throttled.into_response();
    };
    let response = next.run(request).await;
    drop(permit);
    response
}

/// Liveness and database reachability for container health checks. It is
/// unauthenticated and returns no data; the proxy should not publish it.
async fn healthz(State(state): State<AppState>) -> Response {
    let database = state.auth.database.clone();
    let healthy = tokio_blocking(move || {
        open_connection(&database)
            .and_then(|connection| {
                connection
                    .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .map_err(|e| e.to_string())
            })
            .map(|_| ())
            .map_err(|_| AuthError::Database)
    })
    .await
    .is_ok();
    if healthy {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
    }
}

/// The address a request counts as coming from for throttling.
fn client_ip(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    match &state.proxy {
        Some(trust) => trust.client_ip(peer.ip(), headers),
        None => peer.ip(),
    }
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AuthError> {
    validate_credentials(&input.username, &input.password).map_err(|_| AuthError::InvalidInput)?;
    let ip = client_ip(&state, peer, &headers);
    if !allowed_attempt(&state, &input.username, ip) {
        return Err(AuthError::Throttled);
    }
    state
        .auth
        .login(input.username, input.password)
        .await
        .map(Json)
}

async fn me(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<UserResponse>, AuthError> {
    let token = bearer_token(
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )?;
    let username = state.auth.current_user(token).await?;
    Ok(Json(UserResponse { username }))
}

async fn logout(State(state): State<AppState>, request: Request) -> Result<StatusCode, AuthError> {
    // Revocation is idempotent by design: unknown and already-revoked tokens
    // also return 204, so logout never reveals whether a token existed.
    let token = bearer_token(
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )?;
    state.auth.logout(token).await?;
    // Open sockets re-validate their session and close if it is gone.
    state.hub.credentials_changed();
    Ok(StatusCode::NO_CONTENT)
}

/// Bearer tokens are canonical lowercase hex strings issued by login.
/// Uppercase variants are rejected rather than normalized, so a logout can
/// never report success for a string that is not the exact issued token.
fn bearer_token(value: Option<&str>) -> Result<String, AuthError> {
    let token = value
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AuthError::Unauthorized)?;
    if token.len() != 64
        || !token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(AuthError::Unauthorized);
    }
    Ok(token.to_owned())
}

fn allowed_attempt(state: &AppState, username: &str, ip: IpAddr) -> bool {
    let now = (state.auth.config.clock)();
    let window = state.auth.config.login_window.as_secs() as i64;
    let expired = |attempt: &i64| now.saturating_sub(*attempt) >= window;
    let mut throttle = state
        .throttle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (username.to_lowercase(), ip);

    // Bound total tracked keys: prune expired entries first, then refuse to
    // grow the map further so rotated usernames cannot exhaust memory.
    if throttle.by_key.len() >= state.auth.config.max_throttle_keys
        && !throttle.by_key.contains_key(&key)
    {
        throttle
            .by_key
            .retain(|_, attempts| attempts.iter().any(|attempt| !expired(attempt)));
        throttle
            .by_peer
            .retain(|_, attempts| attempts.iter().any(|attempt| !expired(attempt)));
        if throttle.by_key.len() >= state.auth.config.max_throttle_keys {
            return false;
        }
    }

    // Per-peer total across all usernames caps spraying of fresh usernames.
    let ThrottleState { by_key, by_peer } = &mut *throttle;
    let peer_attempts = by_peer.entry(ip).or_default();
    peer_attempts.retain(|attempt| !expired(attempt));
    if peer_attempts.len() >= state.auth.config.max_peer_attempts {
        return false;
    }
    let entries = by_key.entry(key).or_default();
    entries.retain(|attempt| !expired(attempt));
    if entries.len() >= state.auth.config.max_login_attempts {
        return false;
    }
    peer_attempts.push(now);
    entries.push(now);
    true
}

fn validate_credentials(username: &str, password: &str) -> Result<(), AuthError> {
    if !valid_username(username)
        || password.len() < MIN_PASSWORD_BYTES
        || password.len() > MAX_PASSWORD_BYTES
    {
        return Err(AuthError::InvalidInput);
    }
    Ok(())
}

/// ASCII-only username alphabet (letters, digits, plus `.`, `_`, `-`, `@`),
/// at most `MAX_USERNAME_BYTES` bytes. Restricting to ASCII keeps the
/// case-insensitive uniqueness collation well-defined and excludes control
/// and line-separator characters such as U+2028 that could inject into
/// terminal or log output.
fn valid_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= MAX_USERNAME_BYTES
        && username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
}

fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let params = Params::new(19_456, 2, 1, None)?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

fn open_connection(path: &Path) -> Result<Connection, String> {
    prepare_database_file(path)?;
    // macOS aliases such as /var -> /private/var are legitimate parent paths.
    // Resolve the parent only: the final component must still be protected by
    // SQLite NOFOLLOW rather than silently following a database-file symlink.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent =
        dunce::canonicalize(parent).map_err(|e| format!("resolve database directory: {e}"))?;
    let filename = path
        .file_name()
        .ok_or_else(|| "database path must name a file".to_owned())?;
    let database_path = parent.join(filename);
    // No URI flag is passed, so the path is always treated as a plain
    // filename. NOFOLLOW refuses a symlinked final component even if one
    // appears between the checks above and this open.
    let connection = Connection::open_with_flags(
        database_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|e| format!("open database: {e}"))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|e| format!("configure database: {e}"))?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|e| format!("configure database: {e}"))?;
    Ok(connection)
}

/// Creates a new empty file readable by its owner only; fails if the path
/// exists (symlinks included). Missing parent directories are created 0700.
fn create_private_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    };
    #[cfg(not(unix))]
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    created.map(|_| ())
}

/// Replaces the database at `database` with the snapshot in `backup`
/// (made by [`AuthService::backup_to`]).
///
/// The snapshot must pass SQLite's integrity check and look like a
/// collaboration database. An existing database is kept next to the new one
/// as `<name>.before-restore-<unix time>`. Run it while the service is
/// stopped: the service holds no lock between requests, so a running one
/// would not notice the file being swapped.
pub fn restore_database(database: &Path, backup: &Path) -> Result<(), String> {
    let metadata =
        std::fs::symlink_metadata(backup).map_err(|e| format!("inspect backup file: {e}"))?;
    if !metadata.is_file() {
        return Err("backup path must be a regular file, not a symlink".into());
    }
    let resolved = dunce::canonicalize(backup).map_err(|e| format!("resolve backup path: {e}"))?;
    let source = Connection::open_with_flags(
        &resolved,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open backup: {e}"))?;
    let verdict: String = source
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| format!("check backup: {e}"))?;
    if verdict != "ok" {
        return Err("the backup failed SQLite's integrity check; nothing was changed".into());
    }
    let tables: i64 = source
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('auth_users', 'auth_sessions')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("check backup: {e}"))?;
    if tables != 2 {
        return Err("that file is not a trama-collab database; nothing was changed".into());
    }

    let kept = if std::fs::symlink_metadata(database).is_ok() {
        let name = database
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "database path must name a file".to_owned())?;
        let kept = database.with_file_name(format!("{name}.before-restore-{}", unix_time()));
        std::fs::rename(database, &kept)
            .map_err(|e| format!("move the current database aside: {e}"))?;
        Some(kept)
    } else {
        None
    };
    let undo = |error: String| {
        let _ = std::fs::remove_file(database);
        if let Some(kept) = &kept {
            let _ = std::fs::rename(kept, database);
        }
        error
    };
    create_private_file(database).map_err(|e| undo(format!("create database file: {e}")))?;
    let target = dunce::canonicalize(database)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .ok_or_else(|| undo("resolve database path".into()))?;
    source
        .execute("VACUUM INTO ?1", [target])
        .map_err(|e| undo(format!("write database: {e}")))?;
    Ok(())
}

/// Secures the database file before SQLite writes password hashes to it:
/// new files are pre-created owner-only (never the default 0644), existing
/// paths must be regular files (symlinks, dangling or not, are rejected)
/// and are tightened to 0600 whenever group or other permission bits are
/// set. Runs on every open, not only the first one.
fn prepare_database_file(path: &Path) -> Result<(), String> {
    // Only plain filesystem paths are supported: SQLite URI strings could
    // select modes or query parameters that bypass these checks.
    if path
        .as_os_str()
        .to_str()
        .is_some_and(|text| text.starts_with("file:"))
    {
        return Err("database path must be a plain file path, not a SQLite URI".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            // Pre-created owner-only; SQLite opens it as an existing file.
            Ok(_file) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(path)
                    .map_err(|e| format!("inspect database file: {e}"))?;
                if !metadata.is_file() {
                    return Err(
                        "database path must be a regular file; symlinks are not accepted".into(),
                    );
                }
                if metadata.permissions().mode() & 0o077 != 0 {
                    let mut permissions = metadata.permissions();
                    permissions.set_mode(0o600);
                    std::fs::set_permissions(path, permissions)
                        .map_err(|e| format!("restrict database file permissions: {e}"))?;
                }
                Ok(())
            }
            Err(e) => Err(format!("create database file: {e}")),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Reads a single secret (password) from `reader`, refusing input longer
/// than `max_bytes` so callers cannot be forced to buffer unbounded stdin.
/// One trailing newline (Unix or Windows) is stripped. The secret itself is
/// never included in returned error text.
pub fn read_secret(reader: impl Read, max_bytes: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "read secret from stdin".to_owned())?;
    if bytes.len() > max_bytes {
        return Err("secret exceeds maximum accepted length".into());
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| "secret must be valid UTF-8".to_owned())
}

async fn tokio_blocking<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| AuthError::Database)?
}

fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Direct (no proxy) serving is loopback only: the service speaks plain HTTP
/// and bearer tokens must not cross a network in clear text. Binding anything
/// else requires the explicit reverse-proxy opt-in in the `serve` command
/// (`--behind-proxy`), which skips this check and enables [`ProxyTrust`].
pub fn validate_bind(address: SocketAddr) -> Result<(), String> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {address}: the service speaks plain HTTP and only loopback \
             addresses are allowed by default. Behind a TLS reverse proxy on a private \
             network, opt in explicitly with `serve --behind-proxy` \
             (or TRAMA_COLLAB_BEHIND_PROXY=1)"
        ))
    }
}

/// One-shot `GET /healthz` over plain TCP, for container health checks where
/// no `curl` exists. `Ok` only for an HTTP 200 reply.
pub fn probe_health(address: SocketAddr, timeout: Duration) -> Result<(), String> {
    use std::io::Write;
    use std::net::TcpStream;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|e| format!("connect {address}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("send request: {e}"))?;
    let mut head = Vec::new();
    stream
        .take(64)
        .read_to_end(&mut head)
        .map_err(|e| format!("read reply: {e}"))?;
    let head = String::from_utf8_lossy(&head);
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else {
        Err(format!("unhealthy reply: {status:?}"))
    }
}
