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
use super::super::transport::{ShareClient, fetch_manifests, publish_manifests};
use crate::error::{DaemonError, Result};

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
            threads.push(ManifestThread {
                thread_gid: r.thread_gid.clone(),
                parent_gid: r.parent_gid.clone(),
                kind: parse_kind(&r.kind),
                title,
                message_slice: None,
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
        let envelope = tokio::task::spawn_blocking(move || {
            let (key_id, key) = signer.signing_key()?;
            seal_history(&manifest, &key_id, &key).map_err(|e| DaemonError::Invalid(e.to_string()))
        })
        .await
        .map_err(|e| DaemonError::Collab(e.to_string()))??;
        let epoch = self.share_epoch(share_id).await?;
        let (db, cipher, id) = (self.db().clone(), e2ee.cipher.clone(), share_id.to_owned());
        let expected = row
            .current_snapshot_version
            .max(info.latest_manifest_version);
        publish_manifests(
            client,
            &db,
            cipher.as_ref(),
            &id,
            epoch,
            expected,
            &envelope,
            None,
        )
        .await
    }

    /// Joiner: fetches, decrypts and verifies the latest history snapshot.
    pub(crate) async fn read_history_snapshot(
        &self,
        client: &ShareClient,
        share_id: &str,
    ) -> Result<HistoryManifestV1> {
        let e2ee = self.e2ee()?;
        let epoch = self.share_epoch(share_id).await?;
        let fetched =
            fetch_manifests(client, e2ee.cipher.as_ref(), share_id, epoch, 0, false).await?;
        let envelope = fetched.history;
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
        let mut keyring = Keyring::default();
        keyring.insert(key_id.clone(), publisher.publisher_device_id.clone(), raw);
        let manifest = open_history(&envelope, &keyring, None)
            .map_err(|e| DaemonError::Collab(format!("manifest rejected: {e}")))?;
        let (pin, id) = (e2ee.clone(), share_id.to_owned());
        tokio::task::spawn_blocking(move || pin.pin_manifest_key(&id, &key_id))
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))??;
        Ok(manifest)
    }
}
