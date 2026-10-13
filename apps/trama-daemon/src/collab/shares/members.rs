//! Owner-only member removal: the server revokes the grant (and the
//! member's key packages and leases) and returns their devices; each device
//! still in the MLS group is removed with its own commit, broadcast as
//! `membership_changed`, and the snapshot is republished at the new epoch
//! so later snapshots are unreadable for the removed devices.

use trama_collab_protocol::vnext::events::ShareEventKind;
use trama_e2ee::E2eeError;
use trama_protocol::ShareRole;

use super::super::CollabService;
use super::super::e2ee::mls_err;
use super::outbound::Payload;
use crate::error::{DaemonError, Result};

impl CollabService {
    /// Returns the devices removed from the group and the final epoch.
    pub(crate) async fn remove_share_member(
        &self,
        share_id: &str,
        username: &str,
    ) -> Result<(Vec<String>, u64)> {
        let e2ee = self.e2ee()?;
        let (connection, client) = self.share_client()?;
        if connection.is_me(Some(username)) {
            return Err(DaemonError::Invalid("the owner cannot be removed".into()));
        }
        if client.info(share_id).await?.role != ShareRole::Owner {
            return Err(DaemonError::Invalid(
                "only the share owner removes members".into(),
            ));
        }
        let revoked = client.revoke_member(share_id, username).await?;
        let mut removed = Vec::new();
        for device in revoked.devices {
            let (owner, id, dev) = (e2ee.clone(), share_id.to_owned(), device.clone());
            let change = tokio::task::spawn_blocking(move || {
                owner
                    .cipher
                    .with_group(&id, |g, me| g.remove_member(me, &dev))
            })
            .await
            .map_err(|e| DaemonError::Collab(e.to_string()))?;
            let change = match change {
                Ok(change) => change,
                // Posted a key package but was never admitted.
                Err(E2eeError::UnknownMember) => continue,
                Err(e) => return Err(mls_err(e)),
            };
            self.emit_share_event(
                &client,
                share_id,
                ShareEventKind::MembershipChanged,
                None,
                Payload::Raw {
                    bytes: change.commit,
                    epoch: change.epoch,
                },
            )
            .await?;
            removed.push(device);
        }
        if !removed.is_empty() {
            self.publish_history_snapshot(&client, share_id).await?;
        }
        Ok((removed, self.share_epoch(share_id).await?))
    }
}
