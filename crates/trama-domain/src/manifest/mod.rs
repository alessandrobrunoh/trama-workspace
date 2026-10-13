//! Signed share manifests (plan 03 §2 and §4).
//!
//! A snapshot is published as two independent [`SignedEnvelope`]s:
//! - [`HistoryManifestV1`] for every `read_history` member;
//! - [`ContentManifestV1`] only for `read_code` holders, bound to the
//!   history manifest by the SHA-256 of its canonical bytes.
//!
//! The split happens before encryption, so an opaque server never needs to
//! filter payloads. Encryption (MLS) and transport are M4 and out of scope:
//! the envelope here is what a client verifies *after* decrypting.

pub mod canonical;
mod content;
mod envelope;
mod history;

#[cfg(test)]
mod example_tests;
#[cfg(test)]
mod tests;

pub use content::{
    CONTENT_PAYLOAD_TYPE, ContentManifestV1, ContentPolicySummary, LfsPolicy, ManifestCheckout,
    ManifestObject, ManifestRepository, SubmodulePolicy,
};
pub use envelope::{EnvelopeError, Keyring, SignatureAlg, SignedEnvelope, TrustedKey};
pub use history::{ForkOrigin, HISTORY_PAYLOAD_TYPE, HistoryManifestV1, ManifestThread};

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

/// Major schema version understood by this build.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("unsupported manifest schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid manifest: {0}")]
    Invalid(String),
    #[error("envelope signer does not match the declared publisher")]
    PublisherMismatch,
    #[error("snapshot version {found} is older than pinned version {pinned}")]
    Rollback { pinned: u64, found: u64 },
    #[error("content manifest is not bound to this history manifest")]
    Unbound,
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
}

pub(crate) fn check_schema(version: u32) -> Result<(), ManifestError> {
    if version == MANIFEST_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ManifestError::UnsupportedSchema(version))
    }
}

/// Lower-case hex SHA-256 of an envelope's payload bytes.
pub fn payload_digest(envelope: &SignedEnvelope) -> String {
    Sha256::digest(&envelope.payload_bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Validates and signs a history manifest.
pub fn seal_history(
    manifest: &HistoryManifestV1,
    key_id: &str,
    key: &SigningKey,
) -> Result<SignedEnvelope, ManifestError> {
    manifest.validate()?;
    Ok(SignedEnvelope::seal(
        HISTORY_PAYLOAD_TYPE,
        manifest,
        key_id,
        key,
    )?)
}

/// Validates and signs a content manifest against its history envelope;
/// `history_manifest_digest` is filled in here.
pub fn seal_content(
    mut manifest: ContentManifestV1,
    history: &SignedEnvelope,
    key_id: &str,
    key: &SigningKey,
) -> Result<SignedEnvelope, ManifestError> {
    manifest.history_manifest_digest = payload_digest(history);
    manifest.validate()?;
    Ok(SignedEnvelope::seal(
        CONTENT_PAYLOAD_TYPE,
        &manifest,
        key_id,
        key,
    )?)
}

/// Verifies a history envelope: signature, canonical form, signer equals
/// publisher, structure, and no rollback below `pinned_version`.
pub fn open_history(
    envelope: &SignedEnvelope,
    keyring: &Keyring,
    pinned_version: Option<u64>,
) -> Result<HistoryManifestV1, ManifestError> {
    let (manifest, signer): (HistoryManifestV1, _) =
        envelope.open(HISTORY_PAYLOAD_TYPE, keyring)?;
    if signer != manifest.publisher_device_id {
        return Err(ManifestError::PublisherMismatch);
    }
    manifest.validate()?;
    if let Some(pinned) = pinned_version
        && manifest.snapshot_version < pinned
    {
        return Err(ManifestError::Rollback {
            pinned,
            found: manifest.snapshot_version,
        });
    }
    Ok(manifest)
}

/// Verifies a content envelope against an already verified history
/// envelope and manifest: same share and version, digest binding, and every
/// checkout belongs to a thread in the history perimeter.
pub fn open_content(
    envelope: &SignedEnvelope,
    keyring: &Keyring,
    history_envelope: &SignedEnvelope,
    history: &HistoryManifestV1,
) -> Result<ContentManifestV1, ManifestError> {
    let (manifest, signer): (ContentManifestV1, _) =
        envelope.open(CONTENT_PAYLOAD_TYPE, keyring)?;
    if signer != manifest.publisher_device_id {
        return Err(ManifestError::PublisherMismatch);
    }
    if manifest.share_id != history.share_id
        || manifest.snapshot_version != history.snapshot_version
        || manifest.history_manifest_digest != payload_digest(history_envelope)
    {
        return Err(ManifestError::Unbound);
    }
    manifest.validate()?;
    if let Some(co) = manifest
        .checkouts
        .iter()
        .find(|c| !history.contains(&c.thread_gid))
    {
        return Err(ManifestError::Invalid(format!(
            "checkout for {} outside the history perimeter",
            co.thread_gid
        )));
    }
    Ok(manifest)
}
