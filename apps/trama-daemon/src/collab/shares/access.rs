//! Share membership views and access-loss detection.

use trama_db::ShareStatus;
use trama_domain::ids::{MessageId, ThreadId};
use trama_domain::share::ShareId;
use trama_protocol::{
    ShareHistoryEntry, ShareHistoryState, ShareMemberDevice, ShareMemberSnapshot, ThreadShareInfo,
};

use super::super::CollabService;
use crate::error::{DaemonError, Result};

impl CollabService {
    /// After a failed share call: when the server answers "not found" for
    /// the share itself (revoked, or the share is gone), the local share is
    /// marked closed. Transport errors change nothing.
    /// Returns `true` when this device has lost access: the share was
    /// already closed locally or is closed now (`access_revoked`).
    pub(crate) async fn check_share_access(&self, share_id: &str) -> bool {
        let share = ShareId::from(share_id);
        match self.db().share(&share).await {
            Ok(Some(row)) if row.status == ShareStatus::Closed => return true,
            Ok(Some(_)) => {}
            _ => return false,
        }
        let Ok((_, client)) = self.share_client() else {
            return false;
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
            return true;
        }
        false
    }

    /// The share a local thread belongs to, if any. The lease holder is
    /// best effort (omitted when the server is unreachable).
    pub(crate) async fn thread_share(&self, thread: &ThreadId) -> Result<Option<ThreadShareInfo>> {
        let local = thread.to_string();
        for row in self.db().shares().await? {
            let Some(mapped) = self
                .db()
                .share_threads(&row.share_id)
                .await?
                .into_iter()
                .find(|r| r.local_thread_id.as_deref() == Some(local.as_str()))
            else {
                continue;
            };
            let share_id = row.share_id.0.clone();
            let snapshot = self.share_snapshot(&share_id).await?;
            let mut lease_holder = None;
            if row.status == ShareStatus::Ready {
                if let Ok((_, client)) = self.share_client() {
                    if let Ok(leases) = client.leases(&share_id).await {
                        lease_holder = leases
                            .into_iter()
                            .find(|l| l.thread_gid == mapped.thread_gid.0)
                            .map(|l| l.holder);
                    }
                }
            }
            return Ok(Some(ThreadShareInfo {
                share_id,
                status: snapshot.status,
                role: snapshot.role,
                thread_gid: mapped.thread_gid.0,
                lease_holder,
            }));
        }
        Ok(None)
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
