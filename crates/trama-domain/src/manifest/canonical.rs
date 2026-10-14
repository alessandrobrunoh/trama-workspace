//! Trama canonical JSON v1: the unambiguous byte encoding signed inside a
//! [`SignedEnvelope`](super::SignedEnvelope).
//!
//! It is the subset of RFC 8785 (JCS) where both definitions coincide:
//! - no insignificant whitespace;
//! - object keys are ASCII and sorted by byte value, duplicates rejected;
//! - numbers are integers only (no floats, no exponent), so no number
//!   formatting ambiguity exists;
//! - strings use the minimal JSON escapes produced by `serde_json`
//!   (`\"`, `\\`, `\b \f \n \r \t`, other controls as lower-case `\u00xx`).
//!
//! A verifier re-encodes the decoded payload and requires identical bytes,
//! so a signed payload has exactly one valid representation.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalError {
    #[error("floating point numbers are not allowed in canonical JSON")]
    Float,
    #[error("object key is not ASCII: {0:?}")]
    NonAsciiKey(String),
    #[error("payload is not in canonical form")]
    NotCanonical,
    #[error("invalid JSON: {0}")]
    Json(String),
}

/// Encodes a value into canonical bytes.
pub fn to_canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value).map_err(|e| CanonicalError::Json(e.to_string()))?;
    let mut out = Vec::new();
    write_value(&value, &mut out)?;
    Ok(out)
}

/// Decodes canonical bytes, rejecting any non-canonical representation
/// (whitespace, key order, duplicate keys, floats, escapes).
pub fn from_canonical_bytes<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CanonicalError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| CanonicalError::Json(e.to_string()))?;
    let mut again = Vec::new();
    write_value(&value, &mut again)?;
    // Duplicate keys collapse while parsing, so the re-encoding differs.
    if again != bytes {
        return Err(CanonicalError::NotCanonical);
    }
    serde_json::from_value(value).map_err(|e| CanonicalError::Json(e.to_string()))
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => {
            out.extend(serde_json::to_vec(value).map_err(|e| CanonicalError::Json(e.to_string()))?)
        }
        Value::Number(n) => {
            if n.is_f64() {
                return Err(CanonicalError::Float);
            }
            out.extend(n.to_string().as_bytes());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            if let Some((key, _)) = entries.iter().find(|(k, _)| !k.is_ascii()) {
                return Err(CanonicalError::NonAsciiKey((*key).clone()));
            }
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(&Value::String(key.clone()), out)?;
                out.push(b':');
                write_value(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}
