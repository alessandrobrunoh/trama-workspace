//! Sealed persistence of the device identity, MLS epoch states and pending
//! joins in TramaDB (`e2ee_sealed`). Synchronous: called from blocking
//! tasks only (the MLS cipher holds a std mutex).

use std::sync::Arc;

use trama_db::TramaDb;
use trama_e2ee::{E2eeError, EpochStateStore};

use super::at_rest::AtRestKey;
use crate::error::{DaemonError, Result};

pub(crate) struct SealedStore {
    db: TramaDb,
    key: Arc<AtRestKey>,
}

fn e2ee(e: impl std::fmt::Display) -> E2eeError {
    E2eeError::State(e.to_string())
}

impl SealedStore {
    pub(crate) fn new(db: TramaDb, key: Arc<AtRestKey>) -> Self {
        Self { db, key }
    }

    pub(crate) fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match self.db.sealed_get_sync(name)? {
            Some(sealed) => Ok(Some(self.key.open(name, &sealed)?)),
            None => Ok(None),
        }
    }

    pub(crate) fn put(&self, name: &str, value: &[u8]) -> Result<()> {
        let sealed = self.key.seal(name, value)?;
        self.db
            .sealed_put_sync(name, &sealed)
            .map_err(DaemonError::from)
    }

    pub(crate) fn delete(&self, name: &str) -> Result<()> {
        self.db.sealed_delete_sync(name).map_err(DaemonError::from)
    }
}

fn epoch_name(share_id: &str) -> String {
    format!("epoch:{share_id}")
}

impl EpochStateStore for SealedStore {
    fn load(&self, share_id: &str) -> trama_e2ee::Result<Option<Vec<u8>>> {
        self.get(&epoch_name(share_id)).map_err(e2ee)
    }

    fn save(&self, share_id: &str, state: &[u8]) -> trama_e2ee::Result<()> {
        self.put(&epoch_name(share_id), state).map_err(e2ee)
    }

    fn delete(&self, share_id: &str) -> trama_e2ee::Result<()> {
        SealedStore::delete(self, &epoch_name(share_id)).map_err(e2ee)
    }
}
