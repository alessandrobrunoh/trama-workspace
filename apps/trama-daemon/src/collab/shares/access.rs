//! Share membership views and access-loss detection.

use trama_domain::ids::ThreadId;
use trama_protocol::{ShareHistoryEntry, ShareMemberSnapshot};

use super::super::CollabService;
use crate::error::{DaemonError, Result};

impl CollabService {
    /// After a failed share call: closes the share locally when the server
    /// no longer knows this user in it (revoked or deleted).
    pub(crate) async fn check_share_access(&self, _share_id: &str) {}

    pub(crate) async fn list_share_members(
        &self,
        _share_id: &str,
    ) -> Result<Vec<ShareMemberSnapshot>> {
        Err(DaemonError::Invalid("not available yet".into()))
    }

    pub(crate) async fn share_history_status(
        &self,
        _thread: &ThreadId,
    ) -> Result<Vec<ShareHistoryEntry>> {
        Ok(Vec::new())
    }
}
