//! Share membership views and access-loss detection.

use trama_db::ShareStatus;
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::share::ShareId;
use trama_protocol::{
    ShareHistoryEntry, ShareHistoryState, ShareMemberDevice, ShareMemberSnapshot,
};

use super::super::CollabService;
use crate::error::{DaemonError, Result};

impl CollabService {
    /// After a failed share call: when the server answers "not found" for
    /// the share itself (revoked, or the share is gone), the local share is
    /// marked closed. Transport errors change nothing.
    pub(crate) async fn check_share_access(&self, share_id: &str) {
        let share = ShareId::from(share_id);
        match self.db().share(&share).await {
            Ok(Some(row)) if row.status != ShareStatus::Closed => {}
            _ => return,
        }
        let Ok((_, client)) = self.share_client() else {
            return;
        };
        if let Err(DaemonError::NotFound(_)) = client.info(share_id).await {
            crate::log::warn(format!("share {share_id}: access lost, closing it locally"));
            if let Err(err) = self
                .db()
                .set_share_status(&share, ShareStatus::Closed)
                .await
            {
                crate::log::warn(format!("share {share_id}: not closed: {err}"));
            }
            self.share_events().forget(share_id);
        }
    }

    pub(crate) async fn list_share_members(
        &self,
        share_id: &str,
    ) -> Result<Vec<ShareMemberSnapshot>> {
        let (_, client) = self.share_client()?;
        let members = client.members(share_id).await?;
        let bound = client.signing_keys(share_id).await?;
        let group = match self.e2ee() {
            Ok(e2ee) => {
                let id = share_id.to_owned();
                tokio::task::spawn_blocking(move || {
                    e2ee.cipher.with_group(&id, |g, _| g.members()).ok()
                })
                .await
                .ok()
                .flatten()
            }
            Err(_) => None,
        };
        Ok(members
            .into_iter()
            .map(|m| ShareMemberSnapshot {
                devices: m
                    .devices
                    .iter()
                    .map(|d| ShareMemberDevice {
                        device_id: d.clone(),
                        in_group: group.as_ref().map(|g| g.contains(d)),
                        signing_key_bound: bound
                            .iter()
                            .any(|b| &b.device_id == d && b.username == m.username),
                    })
                    .collect(),
                username: m.username,
                role: m.role,
                read_code: m.read_code,
            })
            .collect())
    }

    pub(crate) async fn share_history_status(
        &self,
        thread: &ThreadId,
    ) -> Result<Vec<ShareHistoryEntry>> {
        Ok(self
            .db()
            .share_message_states(&thread.to_string())
            .await?
            .into_iter()
            .map(|row| ShareHistoryEntry {
                message_id: MessageId::from(row.message_id),
                state: if row.state == "shared" {
                    ShareHistoryState::Shared
                } else {
                    ShareHistoryState::NotSharedNoLease
                },
            })
            .collect())
    }
}
