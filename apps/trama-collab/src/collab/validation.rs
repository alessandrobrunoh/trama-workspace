//! Encoding helpers and input validation for collaboration requests.

use super::*;

/// snake_case name of a unit enum variant, matching the wire format.
pub(crate) fn tag<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => unreachable!("unit enum variants serialize as strings"),
    }
}

pub(super) fn untag<T: DeserializeOwned>(text: String) -> rusqlite::Result<T> {
    serde_json::from_value(Value::String(text)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub(super) fn db<E>(_: E) -> AuthError {
    AuthError::Database
}

pub(crate) fn random_hex(bytes: usize) -> Result<String, AuthError> {
    let mut buffer = vec![0_u8; bytes];
    random_fill(&mut buffer).map_err(db)?;
    let text = hex::encode(&buffer);
    buffer.fill(0);
    Ok(text)
}

pub(super) fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub(super) fn is_lower_hex(text: &str, len: usize) -> bool {
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

pub(super) fn validate_command_id(command_id: &str) -> Result<(), AuthError> {
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
pub(super) fn validate_text(text: &str, max: usize, allow_newlines: bool) -> Result<(), AuthError> {
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

pub(super) fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// Store primitives (synchronous, run on blocking threads)
// ---------------------------------------------------------------------------
