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
    http::{StatusCode, header::AUTHORIZATION},
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
mod sockets;

use live::Hub;

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
}

/// An authenticated user session.
#[derive(Clone, Debug)]
struct Identity {
    user_id: i64,
    username: String,
}

/// In-memory sliding-window login attempt tracking. All state is lost when
/// the process restarts; this is a local development control only.
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
    pub fn open(path: impl AsRef<Path>, config: AuthConfig) -> Result<Self, String> {
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
        let path = path.as_ref().to_path_buf();
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
        collab::abandon_all_claims(&connection, (config.clock)())
            .map_err(|_| "recover runner claims".to_owned())?;
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

pub fn router(auth: AuthService) -> Router {
    let state = AppState {
        auth,
        hash_slots: Arc::new(Semaphore::new(8)),
        throttle: Arc::new(Mutex::new(ThrottleState::default())),
        hub: Arc::new(Hub::default()),
    };
    let auth_routes = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    auth_routes
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

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(input): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AuthError> {
    validate_credentials(&input.username, &input.password).map_err(|_| AuthError::InvalidInput)?;
    if !allowed_attempt(&state, &input.username, peer.ip()) {
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
        std::fs::canonicalize(parent).map_err(|e| format!("resolve database directory: {e}"))?;
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

pub fn validate_bind(address: SocketAddr) -> Result<(), String> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(
            "external bind addresses are disabled; use loopback until TLS ingress is configured"
                .into(),
        )
    }
}
