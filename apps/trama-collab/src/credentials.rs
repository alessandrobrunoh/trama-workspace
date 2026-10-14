//! Bearer tokens, login throttling and password checks.

use super::*;

/// Bearer tokens are canonical lowercase hex strings issued by login.
/// Uppercase variants are rejected rather than normalized, so a logout can
/// never report success for a string that is not the exact issued token.
pub(crate) fn bearer_token(value: Option<&str>) -> Result<String, AuthError> {
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

pub(crate) fn allowed_attempt(state: &AppState, username: &str, ip: IpAddr) -> bool {
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

pub(crate) fn validate_credentials(username: &str, password: &str) -> Result<(), AuthError> {
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
pub(crate) fn valid_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= MAX_USERNAME_BYTES
        && username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
}

pub(crate) fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let params = Params::new(19_456, 2, 1, None)?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}
