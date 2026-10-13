//! History snapshots: the owner builds and signs a `HistoryManifestV1`
//! from the share's perimeter rows and commits it sealed with the group's
//! current epoch; a joiner opens it, verifies the signature against the
//! key named by `key_id` (`ed25519:<public key>`, pinned per share on first
//! use) and maps the threads locally. Content manifests and Git bundles
//! are not exported yet (no object upload route): v1 snapshots carry
//! history only.

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use ed25519_dalek::VerifyingKey;
use trama_domain::manifest::{
    HistoryManifestV1, Keyring, MANIFEST_SCHEMA_VERSION, ManifestThread, open_history, seal_history,
};
use trama_domain::share::{DeviceId, ShareId, SharedThreadKind};

use super::super::CollabService;
use super::super::e2ee::mls_err;
use super::super::transport::{ShareClient, open_manifest, publish_manifests};
use crate::error::{DaemonError, Result};
use trama_collab_protocol::vnext::api::{ManifestPart, SigningKeyRequest};
use trama_collab_protocol::vnext::cipher::PayloadKind;
use trama_domain::manifest::SignedEnvelope;

pub(crate) fn kind_str(kind: SharedThreadKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "child".into())
}

pub(crate) fn parse_kind(kind: &str) -> SharedThreadKind {
    serde_json::from_value(serde_json::Value::String(kind.into()))
        .unwrap_or(SharedThreadKind::Child)
}

pub(crate) struct VerifiedSnapshot {
    pub manifest: HistoryManifestV1,
    pub envelope: SignedEnvelope,
    pub keyring: Keyring,
    pub version: u64,
    pub epoch: u64,
}

impl CollabService {
    /// Current MLS epoch of `share_id` on this device.
    pub(crate) async fn share_epoch(&self, share_id: &str) -> Result<u64> {
        let e2ee = self.e2ee()?;
        let id = share_id.to_owned();
        tokio::task::spawn_blocking(move || e2ee.cipher.with_group(&id, |g, _| Ok(g.epoch())))
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))?
            .map_err(mls_err)
    }

    /// Owner: signs and commits the next history snapshot.
    pub(crate) async fn publish_history_snapshot(
        &self,
        client: &ShareClient,
        share_id: &str,
    ) -> Result<u64> {
        let share = ShareId::from(share_id);
        let row = self
            .db()
            .share(&share)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("share {share_id}")))?;
        let rows = self.db().share_threads(&share).await?;
        let mut threads = Vec::new();
        for r in &rows {
            let title = match &r.local_thread_id {
                Some(id) => self
                    .db()
                    .get_thread(&id.clone().into())
                    .await?
                    .and_then(|t| t.title)
                    .unwrap_or_default(),
                None => String::new(),
            };
            let message_slice = match &r.local_thread_id {
                Some(id) => {
                    self.thread_slice(client, share_id, &r.thread_gid.0, &id.clone().into())
                        .await?
                }
                None => None,
            };
            threads.push(ManifestThread {
                thread_gid: r.thread_gid.clone(),
                parent_gid: r.parent_gid.clone(),
                kind: parse_kind(&r.kind),
                title,
                message_slice,
                fork_origin: None,
            });
        }
        let e2ee = self.e2ee()?;
        let info = client.info(share_id).await?;
        let manifest = HistoryManifestV1 {
            schema_version: MANIFEST_SCHEMA_VERSION,
            share_id: share.clone(),
            snapshot_version: row.current_snapshot_version + 1,
            scope: row.scope,
            root_thread_gids: rows
                .iter()
                .filter(|r| r.parent_gid.is_none())
                .map(|r| r.thread_gid.clone())
                .collect(),
            include_future_descendants: row.policy.include_future_descendants,
            created_at: Utc::now().to_rfc3339(),
            publisher_device_id: DeviceId(e2ee.device_id().to_owned()),
            history_cursor: format!("seq:{}", info.last_server_seq),
            threads,
        };
        let signer = e2ee.clone();
        let (key_id, _) = signer.signing_key()?;
        // Idempotent: the server binds this device key to our account once.
        client
            .bind_signing_key(&SigningKeyRequest {
                device_id: e2ee.device_id().to_owned(),
                public_key: key_id.trim_start_matches("ed25519:").to_owned(),
            })
            .await?;
        let envelope = tokio::task::spawn_blocking(move || {
            let (key_id, key) = signer.signing_key()?;
            seal_history(&manifest, &key_id, &key).map_err(|e| DaemonError::Invalid(e.to_string()))
        })
        .await
        .map_err(|e| DaemonError::Collab(e.to_string()))??;
        let epoch = self.share_epoch(share_id).await?;
        let content = self
            .build_content(client, &row, &rows, &envelope, epoch)
            .await?;
        let (db, cipher, id) = (self.db().clone(), e2ee.cipher.clone(), share_id.to_owned());
        let expected = row
            .current_snapshot_version
            .max(info.latest_manifest_version);
        let version = publish_manifests(
            client,
            &db,
            cipher.as_ref(),
            &id,
            epoch,
            expected,
            &envelope,
            content.as_ref(),
        )
        .await?;
        // MLS cannot decrypt our own ciphertext: keep the plaintext.
        self.cache_snapshot(share_id, version, &envelope).await?;
        Ok(version)
    }

    /// Joiner: fetches, decrypts and verifies the latest history snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn read_history_snapshot(
        &self,
        client: &ShareClient,
        share_id: &str,
    ) -> Result<HistoryManifestV1> {
        Ok(self
            .read_verified_snapshot(client, share_id)
            .await?
            .manifest)
    }

    /// As [`Self::read_history_snapshot`], keeping the envelope and the
    /// verified keyring (content manifests are checked against both).
    pub(crate) async fn read_verified_snapshot(
        &self,
        client: &ShareClient,
        share_id: &str,
    ) -> Result<VerifiedSnapshot> {
        let e2ee = self.e2ee()?;
        let epoch = self.share_epoch(share_id).await?;
        let blob = client.manifest(share_id, 0, ManifestPart::History).await?;
        let envelope = match self.cached_snapshot(share_id, blob.version).await? {
            Some(envelope) => envelope,
            None => {
                // An MLS message decrypts once; the cache makes re-reads work.
                let (cipher, id) = (e2ee.cipher.clone(), share_id.to_owned());
                let text = blob.ciphertext.clone();
                let envelope = tokio::task::spawn_blocking(move || {
                    open_manifest(
                        cipher.as_ref(),
                        &id,
                        epoch,
                        PayloadKind::HistoryManifest,
                        &text,
                    )
                })
                .await
                .map_err(|e| DaemonError::Collab(e.to_string()))??;
                self.cache_snapshot(share_id, blob.version, &envelope)
                    .await?;
                envelope
            }
        };
        let key_id = envelope.key_id.clone();
        let raw = key_id
            .strip_prefix("ed25519:")
            .and_then(|b| STANDARD.decode(b).ok())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .and_then(|b| VerifyingKey::from_bytes(&b).ok())
            .ok_or_else(|| DaemonError::Collab("manifest key id is malformed".into()))?;
        // The publisher is checked against the signer by `open_history`.
        let publisher: HistoryManifestV1 = serde_json::from_slice(&envelope.payload_bytes)
            .map_err(|_| DaemonError::Collab("manifest payload is malformed".into()))?;
        // The key must be bound by the server to the owner's account, not
        // only pinned on first use.
        let info = client.info(share_id).await?;
        let b64 = key_id.trim_start_matches("ed25519:");
        let bound = client.signing_keys(share_id).await?.into_iter().any(|b| {
            b.username == info.owner
                && b.device_id == publisher.publisher_device_id.0
                && b.public_key == b64
        });
        if !bound {
            return Err(DaemonError::Collab(
                "manifest key is not bound to the share owner's account".into(),
            ));
        }
        let mut keyring = Keyring::default();
        keyring.insert(key_id.clone(), publisher.publisher_device_id.clone(), raw);
        let manifest = open_history(&envelope, &keyring, None)
            .map_err(|e| DaemonError::Collab(format!("manifest rejected: {e}")))?;
        let (pin, id) = (e2ee.clone(), share_id.to_owned());
        tokio::task::spawn_blocking(move || pin.pin_manifest_key(&id, &key_id))
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))??;
        Ok(VerifiedSnapshot {
            manifest,
            envelope,
            keyring,
            version: blob.version,
            epoch,
        })
    }

    /// Decrypted snapshot of `version`, sealed at rest; only the latest is
    /// kept. The signature is re-checked on every read.
    pub(crate) async fn cache_snapshot(
        &self,
        share_id: &str,
        version: u64,
        envelope: &SignedEnvelope,
    ) -> Result<()> {
        let e2ee = self.e2ee()?;
        let body = serde_json::to_vec(&(version, envelope))
            .map_err(|e| DaemonError::Invalid(e.to_string()))?;
        let name = format!("snapshot:{share_id}");
        tokio::task::spawn_blocking(move || e2ee.put_sealed(&name, &body))
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))?
    }

    pub(crate) async fn cached_snapshot(
        &self,
        share_id: &str,
        version: u64,
    ) -> Result<Option<SignedEnvelope>> {
        let e2ee = self.e2ee()?;
        let name = format!("snapshot:{share_id}");
        let body = tokio::task::spawn_blocking(move || e2ee.get_sealed(&name))
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))??;
        Ok(body
            .and_then(|b| serde_json::from_slice::<(u64, SignedEnvelope)>(&b).ok())
            .filter(|(v, _)| *v == version)
            .map(|(_, envelope)| envelope))
    }
}
