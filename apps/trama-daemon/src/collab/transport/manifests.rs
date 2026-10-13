//! Manifest publish and fetch. Signed envelopes are serialized, sealed by
//! the [`PayloadCipher`] (history and content separately, so a reader
//! without `read_code` never receives content bytes), base64-encoded and
//! committed with compare-and-swap. The local snapshot pin only advances
//! after the server accepted the commit. Signature verification
//! (`open_history` / `open_content`) is the caller's job with its keyring.

use base64::{Engine, engine::general_purpose::STANDARD};
use trama_collab_protocol::vnext::api::{CommitManifestRequest, ManifestPart};
use trama_collab_protocol::vnext::cipher::{PayloadCipher, PayloadKind};
use trama_db::TramaDb;
use trama_domain::manifest::SignedEnvelope;
use trama_domain::share::ShareId;

use super::ShareClient;
use crate::error::{DaemonError, Result};

fn seal(
    cipher: &dyn PayloadCipher,
    share_id: &str,
    key_epoch: u64,
    kind: PayloadKind,
    envelope: &SignedEnvelope,
) -> Result<String> {
    let bytes = serde_json::to_vec(envelope).map_err(|e| DaemonError::Invalid(e.to_string()))?;
    let sealed = cipher
        .seal(share_id, key_epoch, kind, &bytes)
        .map_err(|e| DaemonError::Collab(e.to_string()))?;
    Ok(STANDARD.encode(sealed))
}

fn open(
    cipher: &dyn PayloadCipher,
    share_id: &str,
    key_epoch: u64,
    kind: PayloadKind,
    blob: &str,
) -> Result<SignedEnvelope> {
    let sealed = STANDARD
        .decode(blob)
        .map_err(|_| DaemonError::Collab("manifest is not base64".into()))?;
    let bytes = cipher
        .open(share_id, key_epoch, kind, &sealed)
        .map_err(|e| DaemonError::Collab(e.to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| DaemonError::Collab("manifest envelope is malformed".into()))
}

/// Publishes `expected_version + 1` and advances the local pin.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish_manifests(
    client: &ShareClient,
    db: &TramaDb,
    cipher: &dyn PayloadCipher,
    share_id: &str,
    key_epoch: u64,
    expected_version: u64,
    history: &SignedEnvelope,
    content: Option<&SignedEnvelope>,
) -> Result<u64> {
    let version = expected_version
        .checked_add(1)
        .ok_or_else(|| DaemonError::Invalid("snapshot version overflow".into()))?;
    let request = CommitManifestRequest {
        expected_version,
        version,
        history: seal(
            cipher,
            share_id,
            key_epoch,
            PayloadKind::HistoryManifest,
            history,
        )?,
        content: content
            .map(|c| seal(cipher, share_id, key_epoch, PayloadKind::ContentManifest, c))
            .transpose()?,
    };
    client.commit_manifest(share_id, &request).await?;
    let share = ShareId::from(share_id);
    if !db
        .advance_share_snapshot(&share, expected_version, version)
        .await?
    {
        crate::log::warn(format!(
            "share {share_id}: local snapshot pin moved concurrently"
        ));
    }
    Ok(version)
}

pub(crate) struct FetchedManifests {
    pub version: u64,
    pub history: SignedEnvelope,
    pub content: Option<SignedEnvelope>,
}

/// Fetches `version` (0 = latest). `with_content` needs a `read_code` grant.
pub(crate) async fn fetch_manifests(
    client: &ShareClient,
    cipher: &dyn PayloadCipher,
    share_id: &str,
    key_epoch: u64,
    version: u64,
    with_content: bool,
) -> Result<FetchedManifests> {
    let history = client
        .manifest(share_id, version, ManifestPart::History)
        .await?;
    let content = if with_content {
        let blob = client
            .manifest(share_id, history.version, ManifestPart::Content)
            .await?;
        Some(open(
            cipher,
            share_id,
            key_epoch,
            PayloadKind::ContentManifest,
            &blob.ciphertext,
        )?)
    } else {
        None
    };
    Ok(FetchedManifests {
        version: history.version,
        history: open(
            cipher,
            share_id,
            key_epoch,
            PayloadKind::HistoryManifest,
            &history.ciphertext,
        )?,
        content,
    })
}
