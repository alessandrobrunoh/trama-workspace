//! The openmls provider: pure-Rust crypto plus an in-memory key store whose
//! contents can be snapshotted for persistence.

use std::collections::HashMap;

use openmls_memory_storage::MemoryStorage;
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;

use crate::{E2eeError, Result};

const SNAPSHOT_MAGIC: &[u8; 8] = b"TRMLS\x00\x00\x01";

#[derive(Default)]
pub(crate) struct Provider {
    crypto: RustCrypto,
    storage: MemoryStorage,
}

impl OpenMlsProvider for Provider {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = MemoryStorage;

    fn storage(&self) -> &MemoryStorage {
        &self.storage
    }
    fn crypto(&self) -> &RustCrypto {
        &self.crypto
    }
    fn rand(&self) -> &RustCrypto {
        &self.crypto
    }
}

impl Provider {
    /// Serializes the whole key store (group state, epoch secrets, private
    /// keys). The output is secret material: callers must store it encrypted
    /// at rest (OS keychain-wrapped key), never upload it.
    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let values = self.storage.values.read().expect("storage lock");
        let mut entries: Vec<_> = values.iter().collect();
        entries.sort();
        let mut out = SNAPSHOT_MAGIC.to_vec();
        out.extend((entries.len() as u64).to_be_bytes());
        for (k, v) in entries {
            out.extend((k.len() as u64).to_be_bytes());
            out.extend(k);
            out.extend((v.len() as u64).to_be_bytes());
            out.extend(v);
        }
        out
    }

    pub(crate) fn restore(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader(
            bytes
                .strip_prefix(SNAPSHOT_MAGIC.as_slice())
                .ok_or_else(corrupt)?,
        );
        let count = r.len()?;
        let mut map = HashMap::with_capacity(count.min(4096));
        for _ in 0..count {
            let k = r.chunk()?;
            let v = r.chunk()?;
            map.insert(k, v);
        }
        if !r.0.is_empty() {
            return Err(corrupt());
        }
        let storage = MemoryStorage::default();
        *storage.values.write().expect("storage lock") = map;
        Ok(Self {
            crypto: RustCrypto::default(),
            storage,
        })
    }
}

fn corrupt() -> E2eeError {
    E2eeError::State("corrupt snapshot".into())
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return Err(corrupt());
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn len(&mut self) -> Result<usize> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| corrupt())?;
        usize::try_from(u64::from_be_bytes(b)).map_err(|_| corrupt())
    }
    fn chunk(&mut self) -> Result<Vec<u8>> {
        let n = self.len()?;
        Ok(self.take(n)?.to_vec())
    }
}
