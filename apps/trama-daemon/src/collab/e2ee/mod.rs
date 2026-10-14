//! Device E2EE state for vNext shares: one MLS identity per installation,
//! one MLS group per share (trama-e2ee), all persisted sealed with the
//! at-rest key. The [`MlsPayloadCipher`] replaces `NoCipher` for events and
//! manifests.

mod at_rest;
mod store;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use trama_e2ee::{DeviceIdentity, MlsPayloadCipher, PendingJoin};

pub(crate) use at_rest::{AtRestKey, KeySource};
pub(crate) use store::SealedStore;
use trama_db::TramaDb;

use crate::error::{DaemonError, Result};

const IDENTITY: &str = "identity";
const SIGNING: &str = "manifest-signing";

pub(crate) fn mls_err(e: impl std::fmt::Display) -> DaemonError {
    DaemonError::Collab(format!("e2ee: {e}"))
}

pub(crate) struct E2ee {
    pub(crate) cipher: Arc<MlsPayloadCipher>,
    pub(crate) identity: Arc<DeviceIdentity>,
    store: Arc<SealedStore>,
}

impl E2ee {
    /// Loads (or creates once) the identity for `device_id`. Blocking.
    pub(crate) fn open(db: TramaDb, app_dir: &Path, device_id: &str) -> Result<Self> {
        let key = Arc::new(AtRestKey::load_or_create(app_dir)?);
        if key.source == KeySource::File {
            crate::log::warn(format!(
                "e2ee state is sealed with the file key fallback in {}",
                app_dir.display()
            ));
        }
        Self::with_key(db, key, device_id)
    }

    pub(crate) fn with_key(db: TramaDb, key: Arc<AtRestKey>, device_id: &str) -> Result<Self> {
        let store = Arc::new(SealedStore::new(db, key));
        let identity = match store.get(IDENTITY)? {
            Some(bytes) => DeviceIdentity::from_secret_bytes(&bytes).map_err(mls_err)?,
            None => {
                let identity = DeviceIdentity::generate(device_id).map_err(mls_err)?;
                store.put(IDENTITY, &identity.to_secret_bytes().map_err(mls_err)?)?;
                identity
            }
        };
        let identity = Arc::new(identity);
        let cipher = Arc::new(MlsPayloadCipher::new(identity.clone(), store.clone()));
        Ok(Self {
            cipher,
            identity,
            store,
        })
    }

    /// Ed25519 key signing this device's manifests; `key_id` is
    /// `ed25519:<base64 public key>` so a joiner can pin it on first use.
    /// Blocking.
    pub(crate) fn signing_key(&self) -> Result<(String, ed25519_dalek::SigningKey)> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let seed: [u8; 32] = match self.store.get(SIGNING)? {
            Some(bytes) => bytes
                .try_into()
                .map_err(|_| DaemonError::Collab("stored signing key is malformed".into()))?,
            None => {
                let mut seed = [0u8; 32];
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed)
                    .map_err(|_| DaemonError::Collab("rng".into()))?;
                self.store.put(SIGNING, &seed)?;
                seed
            }
        };
        let key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let id = format!(
            "ed25519:{}",
            STANDARD.encode(key.verifying_key().to_bytes())
        );
        Ok((id, key))
    }

    /// Pinned manifest key of a share (trust on first use). Blocking.
    pub(crate) fn pin_manifest_key(&self, share_id: &str, key_id: &str) -> Result<()> {
        let name = format!("pin:{share_id}");
        match self.store.get(&name)? {
            Some(pinned) if pinned == key_id.as_bytes() => Ok(()),
            Some(_) => Err(DaemonError::Collab(
                "manifest signed by a different key than the pinned one".into(),
            )),
            None => self.store.put(&name, key_id.as_bytes()),
        }
    }

    /// Generic sealed blobs (snapshot cache). Blocking.
    pub(crate) fn put_sealed(&self, name: &str, value: &[u8]) -> Result<()> {
        self.store.put(name, value)
    }

    pub(crate) fn get_sealed(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.store.get(name)
    }

    pub(crate) fn device_id(&self) -> &str {
        self.identity.device_id()
    }

    /// A fresh key package for joining `share_id`; the pending state is
    /// stored until the Welcome arrives. Blocking.
    pub(crate) fn begin_join(&self, share_id: &str) -> Result<Vec<u8>> {
        let (pending, key_package) = PendingJoin::new(&self.identity).map_err(mls_err)?;
        self.store
            .put(&format!("pending:{share_id}"), &pending.snapshot())?;
        Ok(key_package)
    }

    /// Joins with the Welcome; returns the group epoch. Blocking.
    pub(crate) fn finish_join(&self, share_id: &str, welcome: &[u8]) -> Result<u64> {
        let name = format!("pending:{share_id}");
        let state = self
            .store
            .get(&name)?
            .ok_or_else(|| DaemonError::Invalid("no pending join for this share".into()))?;
        let group = PendingJoin::restore(&state)
            .and_then(|p| p.join(welcome))
            .map_err(mls_err)?;
        if group.share_id().map_err(mls_err)? != share_id {
            return Err(DaemonError::Collab("welcome is for another share".into()));
        }
        let epoch = group.epoch();
        self.cipher.insert(group).map_err(mls_err)?;
        self.store.delete(&name)?;
        Ok(epoch)
    }

    #[cfg(test)]
    pub(crate) fn has_group(&self, share_id: &str) -> bool {
        self.cipher.with_group(share_id, |_, _| Ok(())).is_ok()
    }
}
