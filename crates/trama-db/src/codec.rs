use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use trama_domain::{DomainError, Result};

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

pub(crate) fn from_json<T: DeserializeOwned>(raw: &str) -> Result<T> {
    Ok(serde_json::from_str(raw)?)
}

/// Store internally-tagged / unit enums as their serde string (e.g. `"user"`).
pub(crate) fn enum_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

pub(crate) fn enum_from_text<T: DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).or_else(|_| from_json(raw))
}

pub(crate) fn rfc3339(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339()
}

pub(crate) fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|err| DomainError::Storage(format!("invalid timestamp {raw:?}: {err}")))
}

pub(crate) fn sql_err(err: rusqlite::Error) -> DomainError {
    DomainError::Storage(err.to_string())
}

pub(crate) fn is_constraint_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(info, _)
            if info.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

pub(crate) fn path_to_text(path: &std::path::Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DomainError::Invalid(format!("path is not valid UTF-8: {}", path.display())))
}
