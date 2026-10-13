//! Persistence interface for MLS epoch state.
//!
//! A [`crate::ShareGroup`] exports an opaque, *secret* snapshot after every
//! state change (commit merged, message sent/received: MLS ratchets advance on
//! each). Implementations must write it atomically and encrypted at rest; the
//! desktop/daemon will back this with the local DB + an OS-keychain key.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::Result;

/// Where a device keeps the MLS state of the shares it belongs to.
pub trait EpochStateStore: Send + Sync {
    fn load(&self, share_id: &str) -> Result<Option<Vec<u8>>>;
    /// Replaces the state atomically. Losing a write means the device must be
    /// re-added to the group (old epoch secrets are deleted by design).
    fn save(&self, share_id: &str, state: &[u8]) -> Result<()>;
    fn delete(&self, share_id: &str) -> Result<()>;
}

/// In-memory store for tests and ephemeral sessions.
#[derive(Default)]
pub struct MemoryEpochStore(Mutex<HashMap<String, Vec<u8>>>);

impl EpochStateStore for MemoryEpochStore {
    fn load(&self, share_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.0.lock().expect("lock").get(share_id).cloned())
    }
    fn save(&self, share_id: &str, state: &[u8]) -> Result<()> {
        self.0
            .lock()
            .expect("lock")
            .insert(share_id.to_owned(), state.to_vec());
        Ok(())
    }
    fn delete(&self, share_id: &str) -> Result<()> {
        self.0.lock().expect("lock").remove(share_id);
        Ok(())
    }
}
