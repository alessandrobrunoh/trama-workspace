//! AuthService: account, session and throttling operations.

use super::*;

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

    pub(crate) fn open_inner(
        path: &Path,
        config: AuthConfig,
        recover_claims: bool,
    ) -> Result<Self, String> {
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
        if let Some(id) = &config.server_id {
            http::discovery::validate_server_id(id)?;
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
        http::discovery::migrate(&connection)?;
        shares::migrate(&connection)?;
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

    pub(crate) fn cleanup_expired(&self) -> Result<(), String> {
        let connection = open_connection(&self.database)?;
        connection
            .execute(
                "DELETE FROM auth_sessions WHERE expires_at <= ?1",
                [(self.config.clock)()],
            )
            .map_err(|e| format!("clean expired sessions: {e}"))?;
        Ok(())
    }

    pub(crate) async fn login(
        &self,
        username: String,
        password: String,
    ) -> Result<LoginResponse, AuthError> {
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

    pub(crate) fn now(&self) -> i64 {
        (self.config.clock)()
    }

    pub(crate) async fn authenticate(&self, token: String) -> Result<Identity, AuthError> {
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

    pub(crate) async fn current_user(&self, token: String) -> Result<String, AuthError> {
        self.authenticate(token)
            .await
            .map(|identity| identity.username)
    }

    pub(crate) async fn logout(&self, token: String) -> Result<(), AuthError> {
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
