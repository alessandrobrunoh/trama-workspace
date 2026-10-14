//! [`PayloadCipher`] backed by MLS groups: the bridge transport code uses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use trama_collab_protocol::vnext::cipher::{CipherError, PayloadCipher, PayloadKind};

use crate::{DeviceIdentity, E2eeError, EpochStateStore, Inbound, ShareGroup};

/// Holds this device's groups. Every successful operation persists the new
/// epoch state to the store *before* returning the bytes, so a crash never
/// leaves sent ciphertext whose ratchet state was lost.
///
/// MLS cannot decrypt a device's own application messages; `open` is for
/// payloads written by other members.
pub struct MlsPayloadCipher {
    identity: Arc<DeviceIdentity>,
    store: Arc<dyn EpochStateStore>,
    groups: Mutex<HashMap<String, ShareGroup>>,
}

impl MlsPayloadCipher {
    pub fn new(identity: Arc<DeviceIdentity>, store: Arc<dyn EpochStateStore>) -> Self {
        Self {
            identity,
            store,
            groups: Mutex::new(HashMap::new()),
        }
    }

    /// Registers (or replaces) a group, e.g. after create or join.
    pub fn insert(&self, group: ShareGroup) -> crate::Result<()> {
        let id = group.share_id()?;
        self.store.save(&id, &group.snapshot())?;
        self.groups.lock().expect("lock").insert(id, group);
        Ok(())
    }

    /// Runs `f` on the share's group, loading it from the store on demand
    /// and persisting it afterwards. Membership changes and commits go
    /// through here too.
    pub fn with_group<T>(
        &self,
        share_id: &str,
        f: impl FnOnce(&mut ShareGroup, &DeviceIdentity) -> crate::Result<T>,
    ) -> crate::Result<T> {
        let mut groups = self.groups.lock().expect("lock");
        if !groups.contains_key(share_id) {
            let state = self
                .store
                .load(share_id)?
                .ok_or_else(|| E2eeError::State("no group for share".into()))?;
            groups.insert(share_id.to_owned(), ShareGroup::restore(share_id, &state)?);
        }
        let group = groups.get_mut(share_id).expect("inserted");
        let out = f(group, &self.identity);
        // Persist even on error: a failed decrypt may still have consumed
        // ratchet state, and the store must match memory.
        self.store.save(share_id, &group.snapshot())?;
        out
    }
}

/// AAD binding a ciphertext to its share, epoch and payload kind.
fn aad(share_id: &str, key_epoch: u64, kind: PayloadKind) -> Vec<u8> {
    let kind = match kind {
        PayloadKind::Event => "event",
        PayloadKind::HistoryManifest => "history_manifest",
        PayloadKind::ContentManifest => "content_manifest",
    };
    format!("trama/e2ee/v1\n{share_id}\n{key_epoch}\n{kind}").into_bytes()
}

fn epoch_matches(group: &ShareGroup, key_epoch: u64) -> crate::Result<()> {
    if group.epoch() == key_epoch {
        Ok(())
    } else {
        Err(E2eeError::State(format!(
            "key_epoch {key_epoch} but group is at {}",
            group.epoch()
        )))
    }
}

fn cipher_err(e: E2eeError) -> CipherError {
    CipherError(e.to_string())
}

impl PayloadCipher for MlsPayloadCipher {
    fn seal(
        &self,
        share_id: &str,
        key_epoch: u64,
        kind: PayloadKind,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CipherError> {
        self.with_group(share_id, |g, id| {
            epoch_matches(g, key_epoch)?;
            g.encrypt(id, &aad(share_id, key_epoch, kind), plaintext)
        })
        .map_err(cipher_err)
    }

    fn open(
        &self,
        share_id: &str,
        key_epoch: u64,
        kind: PayloadKind,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CipherError> {
        self.with_group(share_id, |g, _| {
            epoch_matches(g, key_epoch)?;
            match g.process(ciphertext, &aad(share_id, key_epoch, kind))? {
                Inbound::Application { plaintext, .. } => Ok(plaintext),
                Inbound::Commit { .. } => Err(E2eeError::Unexpected("commit in payload slot")),
            }
        })
        .map_err(cipher_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryEpochStore, PendingJoin};

    #[test]
    fn seal_open_through_seam_with_binding() {
        let alice = Arc::new(DeviceIdentity::generate("a").unwrap());
        let bob = Arc::new(DeviceIdentity::generate("b").unwrap());
        let mut ga = ShareGroup::create(&alice, "s1").unwrap();
        let (pending, kp) = PendingJoin::new(&bob).unwrap();
        let w = ga.add_members(&alice, &[&kp]).unwrap().welcome.unwrap();
        let ca = MlsPayloadCipher::new(alice, Arc::new(MemoryEpochStore::default()));
        let bob_store = Arc::new(MemoryEpochStore::default());
        let cb = MlsPayloadCipher::new(bob.clone(), bob_store.clone());
        ca.insert(ga).unwrap();
        cb.insert(pending.join(&w).unwrap()).unwrap();

        let ct = ca.seal("s1", 1, PayloadKind::Event, b"evt").unwrap();
        assert!(
            ca.seal("s1", 0, PayloadKind::Event, b"x").is_err(),
            "stale epoch"
        );
        assert!(
            cb.open("s1", 1, PayloadKind::HistoryManifest, &ct).is_err(),
            "kind bound"
        );

        let ct = ca
            .seal("s1", 1, PayloadKind::ContentManifest, b"m")
            .unwrap();
        // Bob restarts: state comes back from his store.
        let cb = MlsPayloadCipher::new(bob, bob_store);
        assert_eq!(
            cb.open("s1", 1, PayloadKind::ContentManifest, &ct).unwrap(),
            b"m"
        );
        assert!(cb.open("unknown", 1, PayloadKind::Event, &ct).is_err());
    }
}
